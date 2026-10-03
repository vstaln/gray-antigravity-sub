use super::*;

#[test]
fn full_ids_pass_through_and_keep_windows() {
    assert_eq!(native_model("gemini-3.8-flash-low"), "gemini-3.8-flash-low");
    assert_eq!(
        native_model("claude-sonnet-5-5-high"),
        "claude-sonnet-5-5-high"
    );
    assert_eq!(context_window("gemini-3.8-flash-low"), Some(1_048_576));
    assert_eq!(context_window("claude-sonnet-5-5-low"), Some(200_000));
    assert_eq!(context_window("claude-opus-5-5-high"), Some(1_000_000));
}

#[test]
fn unknown_windows_are_none_never_guessed() {
    assert_eq!(context_window("gpt-oss-120b-medium"), None);
    assert_eq!(context_window("some-future-model"), None);
    // Unpinned ids still route (future `agy models` entries keep working).
    assert_eq!(native_model("some-future-model"), "some-future-model");
}

#[test]
fn aliases_resolve_to_cheap_defaults() {
    assert_eq!(native_model("flash"), "gemini-3.8-flash-low");
    assert_eq!(native_model("pro"), "gemini-3.1-pro-low");
    assert_eq!(native_model("sonnet"), "claude-sonnet-5-5-low");
    assert_eq!(native_model("opus"), "claude-opus-5-5-low");
    assert_eq!(context_window("sonnet"), Some(200_000));
}

#[test]
fn catalog_covers_live_ids_and_aliases() {
    let ids = all_ids();
    for want in [
        "gemini-3.8-flash-low",
        "gemini-3.1-pro-high",
        "claude-sonnet-5-5-low",
        "claude-opus-5-5-high",
        "gpt-oss-120b-medium",
        "flash",
        "sonnet",
        "opus",
    ] {
        assert!(ids.contains(&want.to_string()), "missing {want}: {ids:?}");
    }
}
