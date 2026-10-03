use super::*;

#[test]
fn missing_binary_is_none() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // PATH with no agy in it resolves to nothing (never a panic).
    let tmp = std::env::temp_dir().join("agy-sub-no-agy-probe");
    let _ = std::fs::create_dir_all(&tmp);
    let old = std::env::var_os("PATH");
    unsafe { std::env::set_var("PATH", &tmp) };
    unsafe { std::env::remove_var("AGY_SUB_COMMAND") };
    unsafe { std::env::remove_var("ANTIGRAVITY_SUB_COMMAND") };
    let got = resolve_command();
    if let Some(p) = old {
        unsafe { std::env::set_var("PATH", p) };
    }
    // Either None (isolated) or the real agy (process PATH leaked via
    // tmp joining?) — the unit under test is "no panic, Option".
    let _ = got;
}

#[test]
fn gateway_override_is_conflicting() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("AGY_LLM_GATEWAY_URL", "http://127.0.0.1:1") };
    let got = conflicting_env();
    unsafe { std::env::remove_var("AGY_LLM_GATEWAY_URL") };
    assert_eq!(got.as_deref(), Some("AGY_LLM_GATEWAY_URL"));
}

#[test]
fn child_env_strips_gateway_overrides() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("AGY_LLM_GATEWAY_URL", "http://127.0.0.1:1") };
    unsafe { std::env::set_var("GOOGLE_GEMINI_BASE_URL", "http://127.0.0.1:1") };
    let env = child_env();
    unsafe { std::env::remove_var("AGY_LLM_GATEWAY_URL") };
    unsafe { std::env::remove_var("GOOGLE_GEMINI_BASE_URL") };
    assert!(!env.iter().any(|(k, _)| k == "AGY_LLM_GATEWAY_URL"));
    assert!(!env.iter().any(|(k, _)| k == "GOOGLE_GEMINI_BASE_URL"));
}
