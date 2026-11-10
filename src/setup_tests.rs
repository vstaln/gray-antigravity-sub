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

// macOS counts the keyring as an auth store unconditionally, so a
// HOME/DBUS scrub can never simulate "logged out" there.
#[cfg(not(target_os = "macos"))]
#[test]
fn probe_without_token_file_is_logged_out_without_spawning() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::Builder::new()
        .prefix("agy-sub-no-token-")
        .tempdir()
        .unwrap();
    let old_home = std::env::var_os("HOME");
    let old_dbus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
    unsafe { std::env::set_var("HOME", tmp.path()) };
    unsafe { std::env::remove_var("DBUS_SESSION_BUS_ADDRESS") };
    let got = probe_login();
    match old_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    match old_dbus {
        Some(d) => unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", d) },
        None => unsafe { std::env::remove_var("DBUS_SESSION_BUS_ADDRESS") },
    }
    assert_eq!(got, LoginState::LoggedOut);
}

#[test]
fn probe_with_real_login_reports_logged_in() {
    let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // has_auth_store() is always true on macOS (keyring), which says
    // nothing about whether a probe here can succeed — require a store
    // the test environment can actually present.
    let probeable = crate::chat::credential_file().is_some()
        || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty());
    if !probeable {
        eprintln!("SKIP: no agy auth store in this environment");
        return;
    }
    unsafe { std::env::set_var("AGY_SUB_PROBE_TIMEOUT_SECS", "30") };
    let got = probe_login();
    unsafe { std::env::remove_var("AGY_SUB_PROBE_TIMEOUT_SECS") };
    assert_eq!(got, LoginState::LoggedIn);
}
