use super::*;

/// The live list `agy models` 1.3.1 reported at discovery-landing time:
/// the pin's `claude-*-5-5-*` generation is gone, `claude-sonnet-4-6` and
/// `claude-opus-4-6-thinking` exist without effort tiers.
fn live_1_3_1() -> Snapshot {
    Snapshot::with_live(&[
        ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
        ("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
        ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
        ("gemini-3.7-flash-high", "Gemini 3.7 Flash (High)"),
        ("gemini-3.7-flash-medium", "Gemini 3.7 Flash (Medium)"),
        ("gemini-3.7-flash-low", "Gemini 3.7 Flash (Low)"),
        ("gemini-3.6-flash-high", "Gemini 3.6 Flash (High)"),
        ("gemini-3.6-flash-medium", "Gemini 3.6 Flash (Medium)"),
        ("gemini-3.6-flash-low", "Gemini 3.6 Flash (Low)"),
        ("gemini-3.1-pro-high", "Gemini 3.1 Pro (High)"),
        ("gemini-3.1-pro-low", "Gemini 3.1 Pro (Low)"),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6 (Thinking)"),
        ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)"),
        ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
    ])
}

// --- pinned fallback (discovery down): the old pinned-table behavior ---

#[test]
fn pinned_fallback_keeps_windows_and_aliases() {
    let snap = Snapshot::pinned_only();
    assert_eq!(
        context_window_in(&snap, "gemini-3.8-flash-low"),
        Some(1_048_576)
    );
    assert_eq!(
        context_window_in(&snap, "claude-sonnet-5-5-low"),
        Some(200_000)
    );
    assert_eq!(
        context_window_in(&snap, "claude-opus-5-5-high"),
        Some(1_000_000)
    );
    assert_eq!(canonical_in(&snap, "flash"), "gemini-3.8-flash-low");
    assert_eq!(canonical_in(&snap, "pro"), "gemini-3.1-pro-low");
    assert_eq!(canonical_in(&snap, "sonnet"), "claude-sonnet-5-5-low");
    assert_eq!(canonical_in(&snap, "opus"), "claude-opus-5-5-low");
    assert_eq!(context_window_in(&snap, "sonnet"), Some(200_000));
}

#[test]
fn pinned_fallback_effort_swaps_stay_on_pins() {
    let snap = Snapshot::pinned_only();
    assert_eq!(
        native_model_for_effort_in(&snap, "flash", Some("high")),
        "gemini-3.8-flash-high"
    );
    assert_eq!(
        native_model_for_effort_in(&snap, "sonnet", Some("medium")),
        "claude-sonnet-5-5-medium"
    );
    // No pinned medium pro → keep the resolved id.
    assert_eq!(
        native_model_for_effort_in(&snap, "pro", Some("medium")),
        "gemini-3.1-pro-low"
    );
    assert_eq!(
        native_model_for_effort_in(&snap, "flash", Some("xhigh")),
        "gemini-3.8-flash-low"
    );
    assert_eq!(
        native_model_for_effort_in(&snap, "flash", None),
        "gemini-3.8-flash-low"
    );
}

#[test]
fn pinned_fallback_unknown_ids_pass_through() {
    let snap = Snapshot::pinned_only();
    assert_eq!(context_window_in(&snap, "gpt-oss-120b-medium"), None);
    assert_eq!(context_window_in(&snap, "some-future-model"), None);
    assert_eq!(
        canonical_in(&snap, "some-future-model"),
        "some-future-model"
    );
}

// --- live discovery answers: the listing, aliases, effort follow it ---

#[test]
fn live_list_replaces_the_pinned_table() {
    let snap = live_1_3_1();
    let ids = all_ids_in(&snap);
    // Live ids are listed…
    for want in [
        "gemini-3.8-flash-low",
        "claude-sonnet-4-6",
        "gpt-oss-120b-medium",
    ] {
        assert!(ids.contains(&want.to_string()), "missing {want}: {ids:?}");
    }
    // …and stale pins are not resurrected.
    assert!(!ids.contains(&"claude-sonnet-5-5-low".to_string()));
    // Aliases remain part of the advertised surface.
    for want in ["flash", "pro", "sonnet", "opus"] {
        assert!(ids.contains(&want.to_string()), "missing alias {want}");
    }
}

#[test]
fn aliases_degrade_to_live_family_members() {
    let snap = live_1_3_1();
    // Pinned defaults still routable → unchanged.
    assert_eq!(canonical_in(&snap, "flash"), "gemini-3.8-flash-low");
    assert_eq!(canonical_in(&snap, "pro"), "gemini-3.1-pro-low");
    // `claude-sonnet-5-5-low` is gone from the live list → the alias
    // follows the family instead of pointing at a dead route.
    assert_eq!(canonical_in(&snap, "sonnet"), "claude-sonnet-4-6");
    assert_eq!(canonical_in(&snap, "opus"), "claude-opus-4-6-thinking");
}

#[test]
fn alias_family_prefers_low_tier() {
    let snap = Snapshot::with_live(&[
        ("claude-sonnet-9-9-high", "S9 High"),
        ("claude-sonnet-9-9-low", "S9 Low"),
        ("claude-sonnet-9-9-medium", "S9 Medium"),
    ]);
    assert_eq!(canonical_in(&snap, "sonnet"), "claude-sonnet-9-9-low");
}

#[test]
fn effort_swaps_onto_live_ids_even_when_unpinned() {
    // A release the pin never heard of: effort mapping still works
    // because candidacy is the live set, not the table.
    let snap = Snapshot::with_live(&[
        ("gemini-9-9-flash-low", "G9 Low"),
        ("gemini-9-9-flash-high", "G9 High"),
    ]);
    assert_eq!(
        native_model_for_effort_in(&snap, "gemini-9-9-flash-low", Some("high")),
        "gemini-9-9-flash-high"
    );
    // …and refuses ids the live list does not carry.
    assert_eq!(
        native_model_for_effort_in(&snap, "gemini-9-9-flash-low", Some("medium")),
        "gemini-9-9-flash-low"
    );
}

#[test]
fn effort_keeps_resolved_id_when_tier_is_absent() {
    let snap = live_1_3_1();
    // `claude-sonnet-4-6` has no effort tiers: effort cannot move it.
    assert_eq!(
        native_model_for_effort_in(&snap, "sonnet", Some("high")),
        "claude-sonnet-4-6"
    );
    // `gemini-3.1-pro` lists no medium: stays on the resolved low.
    assert_eq!(
        native_model_for_effort_in(&snap, "pro", Some("medium")),
        "gemini-3.1-pro-low"
    );
}

#[test]
fn windows_come_from_pins_names_from_live() {
    let snap = live_1_3_1();
    // Known window survives on a live id; unverified live ids stay None.
    assert_eq!(
        context_window_in(&snap, "gemini-3.8-flash-low"),
        Some(1_048_576)
    );
    assert_eq!(context_window_in(&snap, "claude-sonnet-4-6"), None);
    // The degraded alias reports its new route's window (unknown → None).
    assert_eq!(context_window_in(&snap, "sonnet"), None);
    // `agy`'s own display name wins for live ids.
    assert_eq!(
        display_name_in(&snap, "claude-sonnet-4-6"),
        "Claude Sonnet 4.6 (Thinking) (Antigravity)"
    );
    assert_eq!(
        display_name_in(&snap, "sonnet"),
        "Claude Sonnet 4.6 (Thinking) (Antigravity)"
    );
    // Pinned-but-delisted ids read unpinned; unknown ids title-case.
    assert_eq!(
        display_name_in(&snap, "claude-opus-5-5-high"),
        "Claude Opus 5.5 High (Antigravity, unpinned)"
    );
    assert_eq!(
        display_name_in(&snap, "some-future-model"),
        "Some Future Model (Antigravity, unpinned)"
    );
}

#[test]
fn public_api_smoke_with_whatever_source_is_available() {
    // Deterministic regardless of environment: the catalog is never
    // empty and aliases are always advertised, live or pinned.
    let ids = all_ids();
    assert!(!ids.is_empty());
    for alias in ["flash", "pro", "sonnet", "opus"] {
        assert!(ids.contains(&alias.to_string()), "missing {alias}: {ids:?}");
    }
    let _ = context_window("flash");
    let _ = native_model("flash");
    let _ = display_name("flash");
}
