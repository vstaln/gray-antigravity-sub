//! Setup-time probes of the user's `agy` CLI: binary presence and login
//! state. `agy` owns its own auth (OS keyring + browser sign-in, token at
//! `~/.gemini/antigravity-cli/`): this crate never reads, copies, or
//! forwards any credential material — it only probes whether a login
//! exists and fails closed otherwise.
//!
//! Login probe design (all verified against `agy` 1.2.16 behaviour):
//! * `agy models` is unauthenticated (works with an empty HOME), so it only
//!   proves the binary runs — never login state.
//! * A `HOME`-isolated `agy -p` run (env `HOME=<empty>`, stdin `/dev/null`,
//!   stdout discarded) proves login: without credentials `agy` prints its
//!   Google OAuth URL to stderr and exits nonzero ("authentication failed
//!   or timed out"); with a login the run succeeds. The probe HOME is
//!   always empty — never the user's real HOME — so nothing is written to,
//!   read from, or disturbed in the real credential store either way.
//! * A short timeout keeps a logged-out machine from hanging on the OAuth
//!   wait; anything unexpected degrades to "unknown", never to "logged in".

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

/// Probe login state with a `HOME`-isolated `agy -p` run.
///
/// The probe HOME is a fresh empty temp dir (never the user's real HOME):
/// * logged out → `agy` prints its Google OAuth URL and exits nonzero.
/// * logged in → the run succeeds (exit 0). Success here means the OS
///   credential store answered — the probe never touches token files.
/// * timeout / spawn failure / anything unexpected → `Unknown`.
///
/// `AGY_SUB_PROBE_TIMEOUT_SECS` overrides the default 45s (tests use 1s…5s).
pub fn probe_login() -> LoginState {
    let binary = match resolve_command() {
        Some(b) => b,
        None => return LoginState::Unknown,
    };
    let home = match tempfile::Builder::new().prefix("agy-sub-probe-").tempdir() {
        Ok(d) => d,
        Err(_) => return LoginState::Unknown,
    };
    // `keep` so the empty dir (and any first-run cache `agy` writes there)
    // outlives the probe and never pollutes the real HOME.
    let home_path = home.keep();
    let timeout_secs: u64 = std::env::var("AGY_SUB_PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(45);
    let mut child = match std::process::Command::new(&binary)
        .args(["--model", "gemini-3.8-flash-low", "-p", "hi"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .envs(child_env())
        .env("HOME", &home_path)
        // No real-HOME fallback for anything `agy` might consult.
        .env("XDG_CONFIG_HOME", home_path.join("config"))
        .env("XDG_DATA_HOME", home_path.join("data"))
        .env("XDG_CACHE_HOME", home_path.join("cache"))
        // No keyring/agent forwarding into the probe: a "login" here must
        // come from the machine's own store, not ambient forwarded creds.
        .env("DBUS_SESSION_BUS_ADDRESS", "disabled")
        .env("GNOME_KEYRING_CONTROL", "")
        .env("SSH_AUTH_SOCK", "")
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
