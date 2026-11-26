//! The exact protocol-1.2 Antigravity subscription provider declaration.

use gray_plugin::{
    AuthMethodDecl, PROVIDER_CREDENTIALS, ProviderDecl, ProviderHeaderDecl,
    ProviderRequestPolicyDecl, ProviderTransportDecl,
};

pub const PLUGIN_NAME: &str = "antigravity-sub";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROVIDER_ID: &str = "antigravity-subscription";
pub const AUTH_METHOD_ID: &str = "antigravity-login";
/// The operator command: `/antigravity tools …` owns the tool allowlist.
pub const ANTIGRAVITY_COMMAND: &str = "/antigravity";

/// A protocol-1.2 manifest value. Credentials stay with the user's own
/// `agy` sign-in (OS keyring + browser): the `external-login` method
/// performs no OAuth, it only names the login the chat path probes before
/// spawning.
pub fn manifest() -> gray_plugin::Manifest {
    crate::agy::sync_completion_file(&crate::agy::cached_commands());
    gray_plugin::Manifest {
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        tools: Vec::new(),
        commands: vec![ANTIGRAVITY_COMMAND.to_string()],
        hooks: Vec::new(),
        protocol: Some("1.2".to_string()),
        subcommands: Vec::new(),
        completion: crate::agy::static_completion_args(),
        capabilities: vec![
            PROVIDER_CREDENTIALS.to_string(),
            gray_plugin::capabilities::HOST_SAY.to_string(),
        ],
        providers: vec![provider()],
        provider_errors: Vec::new(),
        ..Default::default()
    }
}

/// `command/run` result for a claimed command, `None` for names this
/// sidecar doesn't answer. A bare `/antigravity` answers nothing (`{}`)
/// so the host falls back to the provider-login shortcut — connect →
/// model picker on Antigravity's rows — while `/antigravity tools …`
/// owns the tool allowlist (see [`crate::settings`]), answered as
/// `{"text": …}`.
fn strip_antigravity_prefix(raw: &str) -> Option<&str> {
    let trimmed = raw.trim_start();
    let rest = trimmed
        .strip_prefix(ANTIGRAVITY_COMMAND)
        .or_else(|| trimmed.strip_prefix("antigravity"))?;
    let next = rest.chars().next()?;
    if !next.is_whitespace() {
        return None;
    }
    let body = rest.trim_start();
    (!body.is_empty()).then_some(body)
}

fn format_commands(commands: &[gray_acp::session::SlashCommand]) -> String {
    if commands.is_empty() {
        return "no Antigravity ACP commands discovered yet".to_string();
    }
    let mut lines =
        vec!["Antigravity ACP commands (run as `/antigravity <command> [args…]`):".to_string()];
    for c in commands {
        if c.description.is_empty() {
            lines.push(format!("  /antigravity {}", c.name));
        } else {
            lines.push(format!("  /antigravity {:<20} {}", c.name, c.description));
        }
    }
    lines.join("\n")
}

pub fn run_command(
    name: &str,
    argv: &[String],
    raw: Option<&str>,
    provider_id: Option<&str>,
    commands: &[gray_acp::session::SlashCommand],
) -> Option<serde_json::Value> {
    if name != ANTIGRAVITY_COMMAND {
        return None;
    }
    if argv.is_empty() {
        return Some(serde_json::json!({}));
    }
    match argv[0].as_str() {
        "account" | "tools" => Some(serde_json::json!({
            "text": crate::settings::command(argv),
        })),
        "commands" => Some(serde_json::json!({
            "text": format_commands(commands),
        })),
        other => {
            let cmd = other.trim_start_matches('/');
            if commands.iter().any(|c| c.name == cmd) {
                if provider_id.is_some_and(|p| {
                    p != PROVIDER_ID && p.rsplit_once(':').map(|(_, id)| id) != Some(PROVIDER_ID)
                }) {
                    return Some(serde_json::json!({
                        "text": format!(
                            "switch to an Antigravity model first (`/antigravity`) to run /{cmd}"
                        ),
                    }));
                }
                let prompt = match raw.and_then(strip_antigravity_prefix) {
                    Some(body) if body.starts_with('/') => body.to_string(),
                    Some(body) => format!("/{body}"),
                    None if argv.len() > 1 => format!("/{cmd} {}", argv[1..].join(" ")),
                    None => format!("/{cmd}"),
                };
                return Some(serde_json::json!({
                    "prompt": prompt,
                }));
            }
            Some(serde_json::json!({
                "text": crate::settings::command(argv),
            }))
        }
    }
}

/// Antigravity subscription provider. Requests go to the loopback relay the
/// sidecar opens per chat turn (see `chat`); the host adds bearer, policy,
/// and every declared header from this declaration. The bearer is a
/// per-turn relay token minted by the sidecar, never the user's OAuth token.
pub fn provider() -> ProviderDecl {
    ProviderDecl {
        id: PROVIDER_ID.to_string(),
        name: "Antigravity subscription".to_string(),
        transport: ProviderTransportDecl {
            kind: "openai-responses".to_string(),
            base_url: "https://127.0.0.1:1/"
                .parse()
                .expect("loopback placeholder"),
            authorization: gray_plugin::ProviderAuthorizationDecl {
                kind: "bearer".to_string(),
                secret_name: "relay_token".to_string(),
            },
            request: ProviderRequestPolicyDecl {
                prompt_cache_key: false,
                // Host-side verbatim warm replay must stay off (the
                // relay admits exactly ONE POST per turn).
                warm_replay: false,
                store: false,
                include_reasoning_encrypted: true,
                previous_response_id: false,
                tool_choice: Some("auto".to_string()),
                parallel_tool_calls: Some(true),
                text_verbosity: Some("low".to_string()),
                // Gemini's implicit cache outlives a turn by ~minutes;
                // live.rs keeps pooled sessions under a ~4min refresh,
                // so ~5min is the honest declared lifetime.
                cache_ttl_secs: Some(300),
            },
            headers: vec![ProviderHeaderDecl {
                name: "session-id".to_string(),
                value: None,
                source: Some(gray_plugin::ProviderHeaderSourceDecl::SessionId),
                required: true,
            }],
        },
        auth_methods: vec![AuthMethodDecl {
            id: AUTH_METHOD_ID.to_string(),
            name: "Antigravity CLI sign-in".to_string(),
            kind: "api_key".to_string(),
            operations: vec!["models".to_string(), "chat".to_string()],
        }],
    }
}

#[path = "manifest_tests.rs"]
#[cfg(test)]
mod tests;
