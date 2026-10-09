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
    gray_plugin::Manifest {
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        tools: Vec::new(),
        commands: vec![ANTIGRAVITY_COMMAND.to_string()],
        hooks: Vec::new(),
        protocol: Some("1.2".to_string()),
        subcommands: Vec::new(),
        capabilities: vec![
            PROVIDER_CREDENTIALS.to_string(),
            gray_plugin::capabilities::HOST_SAY.to_string(),
        ],
        providers: vec![provider()],
        provider_errors: Vec::new(),
    }
}

/// `command/run` result for a claimed command, `None` for names this
/// sidecar doesn't answer. A bare `/antigravity` answers nothing (`{}`)
/// so the host falls back to the provider-login shortcut — connect →
/// model picker on Antigravity's rows — while `/antigravity tools …`
/// owns the tool allowlist (see [`crate::settings`]), answered as
/// `{"text": …}`.
pub fn run_command(name: &str, argv: &[String]) -> Option<serde_json::Value> {
    if name == ANTIGRAVITY_COMMAND {
        if argv.is_empty() {
            return Some(serde_json::json!({}));
        }
        return Some(serde_json::json!({
            "text": crate::settings::command(argv),
        }));
    }
    None
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
