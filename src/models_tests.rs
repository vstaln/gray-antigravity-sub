use super::*;

#[test]
fn catalog_lists_pinned_routes_with_windows() {
    let got = catalog();
    assert!(!got.models.is_empty());
    let flash = got
        .models
        .iter()
        .find(|m| m.id == "gemini-3.8-flash-low")
        .expect("flash in catalog");
    assert_eq!(flash.context_window, Some(1_048_576));
    let sonnet = got
        .models
        .iter()
        .find(|m| m.id == "sonnet")
        .expect("sonnet alias in catalog");
    assert_eq!(sonnet.context_window, Some(200_000));
    // No effort knob: full ids encode effort already.
    assert!(flash.reasoning_efforts.is_empty());
    // Unknown windows stay unknown, never guessed.
    let oss = got
        .models
        .iter()
        .find(|m| m.id == "gpt-oss-120b-medium")
        .expect("gpt-oss in catalog");
    assert_eq!(oss.context_window, None);
}
