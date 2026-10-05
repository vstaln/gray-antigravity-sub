//! Setup-time probes of the user's `agy` CLI: binary presence and login
//! state. `agy` owns its own auth (OS keyring + browser sign-in, token at
//! `~/.gemini/antigravity-cli/`): this crate never reads, copies, or
//! forwards any credential material — it only probes whether a login
//! exists and fails closed otherwise.
//!
//! Login probe design (all verified against `agy` 1.2.16 behaviour):
//! * `agy models` is unauthenticated (works with an empty HOME), so it only
//!   proves the binary runs — never login state.
//! * An empty-`HOME` `agy -p` run can never see the real login (token file at
//!   `~/.gemini/antigravity-cli/` plus the OS keyring), so it always prints
//!   the Google OAuth URL, calls `xdg-open` (the stray Firefox window), waits
//!   out the OAuth timeout and fails — even when the user is logged in. The
//!   probe therefore stages a fresh HOME holding just a symlink to the token
//!   file plus the skeleton `settings.json` (the same staging the chat path
//!   uses, verified to authenticate and answer) — never the real HOME.
//! * Fail-closed ordering: no token file means logged out with no spawn at
//!   all (no browser, no wait). Otherwise one lightweight staged-HOME `agy -p`
//!   run with stdin `/dev/null`, a short timeout, and the browser neutralized
//!   (`BROWSER=/bin/true` plus empty `DISPLAY`/`WAYLAND_DISPLAY` so `xdg-open`
//!   can never reach Firefox) confirms the login still answers. Anything
//!   unexpected degrades to "unknown", never to "logged in".

use std::path::PathBuf;

/// `agy` is missing (or not on PATH): install hint, never a spawn panic.
pub const INSTALL_HINT: &str = "`agy` not found on PATH. Install it with \
    `curl -fsSL https://antigravity.google/cli/install.sh | bash`, then run \
    `agy` once to sign in with Google. \
    Override the binary with AGY_SUB_COMMAND=/path/to/agy.";
/// `agy` is present but has no usable login here.
pub const LOGIN_HINT: &str = "Antigravity CLI is installed but not logged in. Run `agy` once and complete the Google sign-in, then retry.";

fn env_override() -> Option<String> {
    std::env::var("AGY_SUB_COMMAND")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var("ANTIGRAVITY_SUB_COMMAND")
                .ok()
                .filter(|v| !v.is_empty())
        })
}

/// Resolve the `agy` binary: explicit override, then PATH.
pub fn resolve_command() -> Option<String> {
    if let Some(v) = env_override() {
        return Some(v);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["agy", "agy.exe", "agy.cmd"] {
            let p: PathBuf = dir.join(name);
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Fail-closed auth env: a set value would redirect the subscription bearer
/// somewhere else (gateway/proxy override) or switch the backend to
/// API-key/Vertex/Cloud auth instead of the user's subscription login.
pub fn conflicting_env() -> Option<String> {
    for key in [
        "AGY_LLM_GATEWAY_URL",
        "AGY_LLM_GATEWAY_API_KEY",
        "AGY_LLM_GATEWAY_PROXY_URL",
        "AGY_LLM_GATEWAY_MODELS",
        "AGY_LLM_GATEWAY_HEADERS",
        "AGY_LLM_GATEWAY_CA_CERT",
        "AGY_LLM_GATEWAY_WIRE_PROTOCOL",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "GOOGLE_GEMINI_BASE_URL",
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_GENAI_USE_VERTEXAI",
        "GOOGLE_GENAI_USE_ENTERPRISE",
    ] {
        if std::env::var_os(key).is_some() {
            return Some(key.to_string());
        }
    }
    None
}

/// Child env for every spawn: never inherit a conflicting value, always
/// quiet the CLI's nonessential traffic.
pub fn child_env() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            ![
                "AGY_LLM_GATEWAY_URL",
                "AGY_LLM_GATEWAY_API_KEY",
                "AGY_LLM_GATEWAY_PROXY_URL",
                "AGY_LLM_GATEWAY_MODELS",
                "AGY_LLM_GATEWAY_HEADERS",
                "AGY_LLM_GATEWAY_CA_CERT",
                "AGY_LLM_GATEWAY_WIRE_PROTOCOL",
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
                "GOOGLE_GEMINI_BASE_URL",
                "GOOGLE_CLOUD_PROJECT",
                "GOOGLE_GENAI_USE_VERTEXAI",
                "GOOGLE_GENAI_USE_ENTERPRISE",
            ]
            .contains(&k.as_str())
        })
        .collect();
    for (k, v) in [
        ("AGY_CLI_DISABLE_AUTO_UPDATE", "1"),
        ("AGY_CLI_DISABLE_INPUT_MODE_REASSERT", "1"),
    ] {
        if !out.iter().any(|(k2, _)| k2 == k) {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

/// Login state. `Unknown` degrades to the pinned catalog, never to a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginState {
    LoggedIn,
    LoggedOut,
    Unknown,
}

/// Probe login state with a staged-HOME `agy -p` run.
///
/// The probe stages a fresh HOME holding just a symlink to the user's `agy`
/// OAuth token file plus the skeleton `settings.json` (the same staging the
/// chat path uses, verified to authenticate and answer) — never the user's
/// real HOME, so probes write nothing to, read nothing from, and disturb
/// nothing in the real credential store or conversation history.
/// * no token file → logged out with no spawn (no browser, no OAuth wait).
/// * staged run exits 0 → logged in (the OS credential store answered).
/// * staged run exits nonzero → logged out.
/// * timeout / spawn failure / anything unexpected → `Unknown`.
///
/// The browser is neutralized (`BROWSER=/bin/true` plus empty
/// `DISPLAY`/`WAYLAND_DISPLAY`, stdin `/dev/null`) so a stale login can
/// never pop a Firefox OAuth page out of a background probe — even though
/// `agy` calls `xdg-open` directly, `xdg-open` with no display and
/// `BROWSER=/bin/true` exits without touching Firefox.
///
/// `AGY_SUB_PROBE_TIMEOUT_SECS` overrides the default 30s (tests use 1s…5s).
pub fn probe_login() -> LoginState {
    let binary = match resolve_command() {
        Some(b) => b,
        None => return LoginState::Unknown,
    };
    // No token file: logged out without spawning (and without any browser).
    // The probe never reads token bytes; it only checks the file exists,
    // then lets the user's own CLI answer through the staged symlink.
    let token_src = match crate::chat::credential_file() {
        Some(path) => path,
        None => return LoginState::LoggedOut,
    };
    let stage = match tempfile::Builder::new().prefix("agy-sub-probe-").tempdir() {
        Ok(stage) => stage,
        Err(_) => return LoginState::Unknown,
    };
    let home = stage.path().to_path_buf();
    let cli_dir = home.join(".gemini").join("antigravity-cli");
    if std::fs::create_dir_all(&cli_dir).is_err() {
        return LoginState::Unknown;
    }
    // Stage the token symlink (auth without ever reading token bytes) plus
    // the skeleton settings.json `agy` needs instead of its defaults.
    #[cfg(unix)]
    if std::os::unix::fs::symlink(&token_src, cli_dir.join("antigravity-oauth-token")).is_err() {
        return LoginState::Unknown;
    }
    #[cfg(not(unix))]
    {
        let _ = &token_src;
        return LoginState::Unknown;
    }
    if std::fs::write(
        cli_dir.join("settings.json"),
        r#"{"toolPermission":"request-review"}"#,
    )
    .is_err()
    {
        return LoginState::Unknown;
    }
    let timeout_secs: u64 = std::env::var("AGY_SUB_PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let mut child = match std::process::Command::new(&binary)
        .args(["--model", "gemini-3.8-flash-low", "-p", "hi"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .envs(child_env())
        // Staged HOME: token symlink (auth) + skeleton settings.json.
        // Cwd stays the spawner's so the prompt-cache prefix is stable.
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        // Neutralize the browser so a stale/expired login can never open
        // Firefox from a background probe: `agy` calls `xdg-open` directly
        // (ignores `BROWSER`), but `xdg-open` with no display and
        // `BROWSER=/bin/true` exits without touching Firefox.
        .env("BROWSER", "/bin/true")
        .env("DISPLAY", "")
        .env("WAYLAND_DISPLAY", "")
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return LoginState::Unknown,
    };
    let status = wait_timeout(&mut child, std::time::Duration::from_secs(timeout_secs));
    // Never leave a hung OAuth wait behind.
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return LoginState::Unknown;
    }
    match status {
        Some(Ok(s)) if s.success() => LoginState::LoggedIn,
        Some(Ok(_)) => LoginState::LoggedOut,
        _ => LoginState::Unknown,
    }
}

fn wait_timeout(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(Ok(status)),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Some(Err(e)),
        }
    }
}

#[path = "setup_tests.rs"]
#[cfg(test)]
mod tests;
