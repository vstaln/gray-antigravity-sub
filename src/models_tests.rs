use super::*;

/// Pin a deterministic "live" list via `AGY_SUB_MODELS_FILE` so the
/// catalog test does not depend on the machine's `agy` or network.
struct ModelsFileGuard {
    _env: std::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

fn pin_live(rows: &str) -> ModelsFileGuard {
    let env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::Builder::new()
        .prefix("agy-sub-models-")
        .tempdir()
        .unwrap();
    let file = dir.path().join("models.tsv");
    std::fs::write(&file, rows).unwrap();
    unsafe { std::env::set_var("AGY_SUB_MODELS_FILE", &file) };
    ModelsFileGuard {
        _env: env,
        _dir: dir,
    }
}

impl Drop for ModelsFileGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("AGY_SUB_MODELS_FILE") };
    }
}

#[test]
fn catalog_collapses_effort_tiers_into_family_rows() {
    let _g = pin_live(
        "gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
         gemini-3.8-flash-high\tGemini 3.8 Flash (High)\n\
         claude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)\n\
         gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n",
    );
    let got = catalog();
    assert!(!got.models.is_empty());
    // The family lists once, under its base id — never per tier.
    assert!(!got.models.iter().any(|m| m.id == "gemini-3.8-flash-low"));
    let flash = got
        .models
        .iter()
        .find(|m| m.id == "gemini-3.8-flash")
        .expect("flash family in catalog");
    // Pinned window still enriches via a member; the effort tag is
    // trimmed off agy's name.
    assert_eq!(flash.context_window, Some(1_048_576));
    assert_eq!(flash.name, "Gemini 3.8 Flash (Antigravity)");
    // Tiers are the row's declared variants, in catalog order.
    let tiers: Vec<_> = flash.variants.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(tiers, ["gemini-3.8-flash-low", "gemini-3.8-flash-high"]);
    assert_eq!(
        flash
            .variants
            .iter()
            .map(|v| v.effort.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["low", "high"]
    );
    assert_eq!(flash.reasoning_efforts, ["low", "high"]);
    assert!(flash.slots.is_empty());
    // A one-tier family still collapses; unknown windows stay unknown.
    let oss = got
        .models
        .iter()
        .find(|m| m.id == "gpt-oss-120b")
        .expect("gpt-oss family in catalog");
    assert_eq!(oss.context_window, None);
    assert_eq!(oss.name, "GPT-OSS 120B (Antigravity)");
    assert_eq!(oss.reasoning_efforts, ["medium"]);
    // A tierless live id stays its own plain row.
    let sonnet = got
        .models
        .iter()
        .find(|m| m.id == "claude-sonnet-4-6")
        .expect("live sonnet in catalog");
    assert_eq!(sonnet.context_window, None);
    assert!(sonnet.variants.is_empty());
    assert!(sonnet.reasoning_efforts.is_empty());
    // The alias row resolves onto the live family member.
    let alias = got
        .models
        .iter()
        .find(|m| m.id == "sonnet")
        .expect("sonnet alias in catalog");
    assert_eq!(alias.name, "Claude Sonnet 4.6 (Thinking) (Antigravity)");
    // Stale pinned ids are not advertised while discovery answers.
    assert!(!got.models.iter().any(|m| m.id == "claude-sonnet-5-5-low"));
}

#[test]
fn pinned_fallback_groups_pinned_families() {
    // Discovery down: the pinned table still collapses per family.
    let got = catalog_in(&crate::catalog::Snapshot::pinned_only());
    let flash = got
        .models
        .iter()
        .find(|m| m.id == "gemini-3.8-flash")
        .expect("flash family in catalog");
    assert_eq!(flash.name, "Gemini 3.8 Flash (Antigravity)");
    assert_eq!(flash.context_window, Some(1_048_576));
    assert_eq!(flash.reasoning_efforts, ["low", "medium", "high"]);
    let pro = got
        .models
        .iter()
        .find(|m| m.id == "gemini-3.1-pro")
        .expect("pro family in catalog");
    assert_eq!(pro.reasoning_efforts, ["low", "high"]);
    let claude = got
        .models
        .iter()
        .find(|m| m.id == "claude-opus-5-5")
        .expect("opus family in catalog");
    assert_eq!(claude.context_window, Some(1_000_000));
    assert_eq!(claude.reasoning_efforts, ["low", "medium", "high"]);
}
