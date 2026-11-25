//! antigravity-sub: a protocol-1.2 provider sidecar for Gray.
//!
//! Antigravity subscription through the official `agy` CLI: the host
//! owns tools, approvals and compaction, the sidecar only answers — one
//! upstream request per chat turn, enforced by the admission relay.
//!
//! Wire (NDJSON over stdio, host ids are opaque):
//! - `plugin/manifest` → manifest + the `antigravity-subscription` provider decl.
//! - `provider/models` → live catalog (`agy models` discovery, pinned
//!   table as fallback — see `discover`/`catalog`).
//! - `provider/chat` → one relayed turn (declares the per-turn relay URL +
//!   bearer the host's standard Responses POST must use).
//! - `provider/auth/*` → the user's own `agy` Google sign-in owns
//!   credentials; start/poll report external-login status, refresh/revoke
//!   are unsupported.
//! - `command/run` → a bare `/antigravity` answers nothing so the host's
//!   provider-login shortcut switches the session to Antigravity, and
//!   `/antigravity tools …` owns the upstream tool allowlist
//!   (`{"text": …}`).
//! - `plugin/shutdown` → clean exit. Unknown methods are protocol errors
//!   (provider sidecars must fail loudly, never hang a turn).

use acp_bridge::relay::{self, Intents, TurnFn};
use acp_bridge::session::{self, Pool};
use antigravity_sub::agy::{self, AgyDriver};
use antigravity_sub::{accounts, catalog, manifest, models, setup, usage};

use gray_plugin::{ProviderRefreshRequest, ProviderRevokeRequest, ProviderRpcError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Serialize)]
struct Response {
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

type Relays = Intents;

const MAX_ACCOUNT_RETRIES: usize = 3;

fn pool() -> &'static Arc<Pool> {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    POOL.get_or_init(|| Pool::new(Arc::new(AgyDriver::default())))
}

fn cli_help() -> String {
    "antigravity-sub — Antigravity subscription provider for gray\n\
     usage: antigravity-sub manifest | sidecar | status | tools [all|default|bash|+name|-name|name …] | account <verb>\n\
     slash:  /antigravity · /antigravity tools … · /antigravity account …"
        .to_string()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // `gray install plugin` registers sidecars by running `<bin> manifest`.
    match std::env::args_os().nth(1).as_deref() {
        Some(m) if m == "manifest" => {
            println!("{}", serde_json::to_string(&manifest::manifest())?);
            return Ok(());
        }
        // `gray antigravity-sub tools …` and the REPL's `/antigravity tools …`
        // land on the same file: one allowlist, no sidecar needed.
        Some(t) if t == "tools" => {
            let args: Vec<String> = std::env::args_os()
                .skip(2)
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            println!("{}", antigravity_sub::settings::tools_command(&args));
            return Ok(());
        }
        Some(a) if a == "account" => {
            let args: Vec<String> = std::env::args_os()
                .skip(2)
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            let interactive = std::io::IsTerminal::is_terminal(&std::io::stdout());
            match accounts::command(&args, interactive) {
                Ok(text) => {
                    println!("{text}");
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            }
        }
        Some(s) if s == "status" => {
            println!("{}", antigravity_sub::status::text());
            return Ok(());
        }
        Some(s) if s == "help" || s == "--help" || s == "-h" => {
            println!("{}", cli_help());
            return Ok(());
        }
        Some(s) if s == "sidecar" => {}
        Some(other) => {
            eprintln!("unknown verb {other:?}\n\n{}", cli_help());
            std::process::exit(2);
        }
        None if std::io::IsTerminal::is_terminal(&std::io::stdin()) => {
            println!("{}", cli_help());
            return Ok(());
        }
        None => {}
    }
    // Warm the model-discovery cache in the background (`agy models` is a
    // ~seconds network fetch): the first `provider/models` then answers
    // live instead of blocking on the spawn. Failure just leaves the
    // pinned fallback in charge.
    std::thread::spawn(|| {
        let _ = antigravity_sub::discover::live();
    });
    let relays: Relays = Arc::new(Mutex::new(HashMap::new()));
    let mut lines =
        tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(tokio::io::stdin()));
    let mut stdout = std::io::stdout();
    // Progress lines (`host/say`) need the transport's request path; the
    // binary speaks raw NDJSON, so progress is folded into the turn reply.
    // (A host.say-emitting variant would need the SidecarPlugin transport;
    // the sse text already carries the full answer.)
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        if request.id.is_null() {
            if request.method == "plugin/shutdown" {
                return Ok(());
            }
            continue;
        }
        let outcome = handle(&relays, &request).await;
        let response = match outcome {
            Ok(result) => Response {
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => Response {
                id: request.id,
                result: None,
                error: Some(error_value(error)),
            },
        };
        let frame = serde_json::to_string(&response)?;
        // Protocol frames only: never log credential payloads or upstream bodies.
        let _ = writeln!(stdout, "{frame}");
        stdout.flush()?;
        if request.method == "plugin/shutdown" {
            return Ok(());
        }
    }
    Ok(())
}

async fn handle(relays: &Relays, request: &Request) -> Result<Value, ProviderRpcError> {
    let params = request.params.clone().unwrap_or_else(|| json!({}));
    match request.method.as_str() {
        "plugin/manifest" => Ok(serde_json::to_value(manifest::manifest()).unwrap()),
        "provider/auth/start" => {
            ensure_provider(
                params
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                params
                    .get("auth_method")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )?;
            // External login: the CLI owns credentials. Probe real login
            // state (staged-HOME `agy -p`, browser neutralized, ~seconds when
            // logged in); report where to fix it.
            if setup::resolve_command().is_none() {
                return Err(ProviderRpcError::Unavailable(setup::INSTALL_HINT.into()));
            }
            if let Some(key) = setup::conflicting_env() {
                return Err(ProviderRpcError::Unavailable(format!(
                    "subscription provider refuses conflicting {key}: unset it so native uses your Antigravity login"
                )));
            }
            // Sync probe, seconds on a logged-in machine; run it inline
            // (spawn_blocking needs 'static + Send on the future's captures,
            // which the borrowed relays map cannot satisfy here).
            match setup::probe_login() {
                setup::LoginState::LoggedIn => Ok(json!({"status": "authenticated"})),
                setup::LoginState::LoggedOut | setup::LoginState::Unknown => {
                    Err(ProviderRpcError::Unavailable(
                        "Antigravity sign-in lives in your terminal: run `agy` once and complete the Google sign-in, then retry.".into(),
                    ))
                }
            }
        }
        "provider/auth/poll" => Err(ProviderRpcError::Protocol(
            "external login has no pollable operation".into(),
        )),
        "provider/auth/cancel" => Ok(json!({})),
        "provider/auth/refresh" => {
            let req: ProviderRefreshRequest = serde_json::from_value(params).map_err(|e| {
                ProviderRpcError::Protocol(format!("bad provider/auth/refresh params: {e}"))
            })?;
            ensure_provider(&req.provider, &req.auth_method)?;
            Ok(json!({"status": "unsupported"}))
        }
        "provider/auth/revoke" => {
            let req: ProviderRevokeRequest = serde_json::from_value(params).map_err(|e| {
                ProviderRpcError::Protocol(format!("bad provider/auth/revoke params: {e}"))
            })?;
            ensure_provider(&req.provider, &req.auth_method)?;
            Ok(json!({"status": "unsupported"}))
        }
        "provider/usage" => usage::handle(),
        "provider/models" => {
            // External login owns no credential material: only the
            // provider/auth-method pair is checked, the envelope (if any)
            // is ignored.
            let provider = params
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let auth_method = params
                .get("auth_method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            ensure_provider(provider, auth_method)?;
            Ok(serde_json::to_value(models::catalog()).unwrap())
        }
        "provider/chat" => chat_turn(relays, params).await,
        "command/run" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let argv: Vec<String> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            manifest::run_command(name, &argv)
                .ok_or_else(|| ProviderRpcError::Protocol(format!("unknown command: {name}")))
        }
        "plugin/shutdown" => Ok(json!({})),
        other => Err(ProviderRpcError::Protocol(format!(
            "method not found: {other}"
        ))),
    }
}

/// One relayed turn: park the intent, open the loopback relay, and hand
/// the host the relay URL + per-turn bearer its standard Responses POST
/// uses. The admitted POST translates, spawns, folds and streams — the
/// sidecar answers from the relay, never from this method.
async fn chat_turn(relays: &Relays, params: Value) -> Result<Value, ProviderRpcError> {
    let provider = params
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if provider != manifest::PROVIDER_ID {
        return Err(ProviderRpcError::Protocol("unknown provider".into()));
    }
    let raw_model = params
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if crate::setup::resolve_command().is_none() {
        return Err(ProviderRpcError::Unavailable(setup::INSTALL_HINT.into()));
    }
    if let Some(key) = setup::conflicting_env() {
        return Err(ProviderRpcError::Unavailable(format!(
            "subscription provider refuses conflicting {key}: unset it so native uses your Antigravity login"
        )));
    }
    if !antigravity_sub::agy::has_auth_store() {
        return Err(ProviderRpcError::Unavailable(setup::LOGIN_HINT.into()));
    }
    let model = if raw_model.is_empty() {
        "flash".to_string()
    } else {
        raw_model
    };
    let turn: TurnFn = Arc::new(|raw: &[u8], cancel: &std::sync::atomic::AtomicBool| {
        let mut attempt = 0;
        loop {
            let out = session::run_turn(pool(), raw, cancel);
            match &out {
                Err(e) if attempt < MAX_ACCOUNT_RETRIES && agy::verdict_of(e).is_some() => {
                    attempt += 1;
                }
                _ => return out,
            }
        }
    });
    let (relay_url, relay_token) = relay::mint_turn(relays, &model, turn, "antigravity-sub-")
        .map_err(ProviderRpcError::Unavailable)?;
    Ok(json!({
        "relay_url": relay_url,
        "relay_token": relay_token,
        "native_model": catalog::native_model(&model),
    }))
}

fn ensure_provider(provider: &str, auth_method: &str) -> Result<(), ProviderRpcError> {
    if provider == manifest::PROVIDER_ID && auth_method == manifest::AUTH_METHOD_ID {
        Ok(())
    } else {
        Err(ProviderRpcError::Protocol(
            "unknown provider or auth method".into(),
        ))
    }
}

fn error_value(error: ProviderRpcError) -> Value {
    match error {
        ProviderRpcError::Rpc(failure) => json!({
            "code": failure.code,
            "message": failure.message,
            "retryable": failure.retryable,
            "terminal": failure.terminal,
        }),
        ProviderRpcError::Protocol(message) => {
            json!({"code": "protocol", "message": message})
        }
        ProviderRpcError::Unavailable(message) => {
            json!({"code": "unavailable", "message": message})
        }
        ProviderRpcError::CapabilityMissing(message) => {
            json!({"code": "capability_missing", "message": message})
        }
    }
}
