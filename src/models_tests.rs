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
fn catalog_lists_live_routes_with_windows() {
    let _g = pin_live(
        "gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
         claude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)\n\
         gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n",
    );
    let got = catalog();
    assert!(!got.models.is_empty());
    let flash = got
        .models
        .iter()
        .find(|m| m.id == "gemini-3.8-flash-low")
        .expect("flash in catalog");
    // Pinned window still enriches a live id; agy's own name is used.
    assert_eq!(flash.context_window, Some(1_048_576));
    assert_eq!(flash.name, "Gemini 3.8 Flash (Low) (Antigravity)");
    // No effort knob: full ids encode effort already.
    assert!(flash.reasoning_efforts.is_empty());
    assert!(flash.variants.is_empty());
    assert!(flash.slots.is_empty());
    // Unknown windows stay unknown, never guessed.
    let oss = got
        .models
        .iter()
        .find(|m| m.id == "gpt-oss-120b-medium")
        .expect("gpt-oss in catalog");
    assert_eq!(oss.context_window, None);
    // A live id with no pinned window is listed with None.
    let sonnet = got
        .models
        .iter()
        .find(|m| m.id == "claude-sonnet-4-6")
        .expect("live sonnet in catalog");
    assert_eq!(sonnet.context_window, None);
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
