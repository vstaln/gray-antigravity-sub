//! `provider/usage`: Antigravity subscription quota.
//!
//! `agy` exposes no quota command and writes no quota cache, so the only
//! sources are the two the IDE itself uses:
//!
//! - **Live probe** (`daily-cloudcode-pa.googleapis.com/v1internal`):
//!   the OAuth bearer the CLI stores at
//!   `~/.gemini/antigravity-cli/antigravity-oauth-token` signs a
//!   `loadCodeAssist` + `retrieveUserQuotaSummary` pair when the account
//!   holds a cloudaicompanion project. `agy` owns the refresh grant, so
//!   a stale bearer falls through to the card below rather than minting
//!   one itself. Antigravity-subscription accounts
//!   resolve no Code Assist tier — the quota call then 403s and we fall
//!   through cleanly.
//! - **`quota-event` write-through**: the chat fold already recognises
//!   "Individual quota reached"; it stamps
//!   `~/.cache/antigravity-sub/quota-event.json`, which renders as a
//!   100% row until the next probe.
//!
//! Otherwise the card reports the provider honestly: connected, with a
//! note that the service publishes no quota feed for this account.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use gray_plugin::ProviderRpcError;

const CLOUDCODE: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// A quota event younger than this still renders (stale "limit reached"
/// banners would mislead after the window rolled over).
const EVENT_TTL_SECS: u64 = 6 * 3600;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn iso(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn cli_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".gemini/antigravity-cli"))
}

fn event_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cache/antigravity-sub/quota-event.json"))
}

/// `agy`'s stored OAuth credential: `{token: {access_token,
/// refresh_token, expiry}, auth_method, id_token}`.
fn read_token() -> Option<(String, Option<String>)> {
    let path = cli_dir()?.join("antigravity-oauth-token");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let access = v["token"]["access_token"].as_str()?.to_string();
    let refresh = v["token"]["refresh_token"].as_str().map(str::to_string);
    Some((access, refresh))
}

/// The chat fold saw a quota wall: stamp it for `provider/usage`.
/// Never carries the prompt — detail is the backend's quota line only.
pub fn note_quota_event(detail: &str) {
    let Some(path) = event_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = json!({
        "detail": detail.chars().take(200).collect::<String>(),
        "at": now_secs(),
    });
    let _ = std::fs::write(path, body.to_string());
}

fn read_event() -> Option<Value> {
    let path = event_path()?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let at = v.get("at").and_then(Value::as_u64)?;
    if now_secs().saturating_sub(at) > EVENT_TTL_SECS {
        return None;
    }
    Some(v)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(PROBE_TIMEOUT)
        .build()
}

fn post(access: &str, method: &str, body: &str) -> Result<Value, String> {
    match agent()
        .post(&format!("{CLOUDCODE}:{method}"))
        .set("Authorization", &format!("Bearer {access}"))
        .set("Content-Type", "application/json")
        .send_string(body)
    {
        Ok(r) => serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string()),
        Err(ureq::Error::Status(code, r)) => {
            let b = r.into_string().unwrap_or_default();
            Err(format!("{method} HTTP {code}: {}", b.chars().take(160).collect::<String>()))
        }
        Err(e) => Err(format!("{method}: {e}")),
    }
}

/// The full live probe: `loadCodeAssist` resolves the tier/project, then
/// `retrieveUserQuotaSummary` lists per-model buckets. First 401 retry
/// refreshes the bearer once.
fn probe() -> Result<Value, String> {
    let (mut access, _refresh) =
        read_token().ok_or_else(|| "no agy credential".to_string())?;
    let meta = r#"{"metadata":{"ideType":"ANTIGRAVITY","platform":"LINUX_AMD64","pluginType":"GEMINI"}}"#;
    let mut lca = post(&access, "loadCodeAssist", meta);
    // 401 → the stored bearer is stale and `agy` owns the refresh grant;
    // re-read once in case a concurrent `agy` run just rewrote the file.
    if lca.is_err() && lca.as_ref().unwrap_err().contains("401") {
        if let Some((fresh, _)) = read_token()
            && fresh != access
        {
            access = fresh;
            lca = post(&access, "loadCodeAssist", meta);
        }
    }
    let lca = lca?;
    let plan = lca
        .pointer("/currentTier/name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let project = lca
        .get("cloudaicompanionProject")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut windows = Vec::new();
    if !project.is_empty() {
        let body = json!({"project": project}).to_string();
        if let Ok(q) = post(&access, "retrieveUserQuotaSummary", &body) {
            for b in q.get("buckets").and_then(Value::as_array).cloned().unwrap_or_default() {
                let model = b
                    .get("modelId")
                    .or_else(|| b.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or("model");
                let remaining = b
                    .get("remainingFraction")
                    .or_else(|| b.get("remaining_fraction"))
                    .and_then(Value::as_f64);
                windows.push(json!({
                    "id": format!("quota_{model}"),
                    "label": model,
                    "kind": "session",
                    "used_percent": remaining.map(|r| (100.0 - r * 100.0).max(0.0)),
                    "resets_at": b.get("resetTime").or_else(|| b.get("reset_time"))
                        .and_then(Value::as_str).map(str::to_string),
                }));
            }
        }
    }
    Ok(json!({
        "available": true,
        "title": "Antigravity",
        "plan": plan,
        "windows": windows,
        "checked_at": iso(now_secs()),
        "note": if windows.is_empty() {
            json!("no quota feed for this account — the service surfaces limits only on exhaustion")
        } else {
            Value::Null
        },
    }))
}

/// `provider/usage` entry: live probe → quota-event → honest empty.
pub fn handle() -> Result<Value, ProviderRpcError> {
    if let Ok(mut limits) = probe() {
        // A live answer still merges a fresh quota event on top — the
        // probe's buckets don't see a very recent wall hit.
        if let Some(event) = read_event()
            && limits["windows"].as_array().map(|w| w.is_empty()).unwrap_or(true)
        {
            limits["windows"] = json!([event_window(&event)]);
            limits["note"] = event["detail"].clone();
        }
        return Ok(limits);
    }
    if let Some(event) = read_event() {
        return Ok(json!({
            "available": true,
            "title": "Antigravity",
            "windows": [event_window(&event)],
            "checked_at": iso(now_secs()),
            "note": event["detail"].clone(),
        }));
    }
    // Connected but unprobeable (or logged out): report the card with no
    // fabricated bars — the collector still shows the provider exists.
    Ok(json!({
        "available": true,
        "title": "Antigravity",
        "windows": [],
        "checked_at": iso(now_secs()),
        "note": "subscription quota isn't published for this account",
    }))
}

fn event_window(_event: &Value) -> Value {
    json!({
        "id": "quota_event",
        "label": "Quota",
        "kind": "session",
        "used_percent": 100.0,
    })
}
