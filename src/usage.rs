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

/// The `agy` OAuth token: the OS keyring entry `agy` actually signs in
/// with (go-keyring: service `gemini`, user `antigravity`), then the
/// legacy token file. `agy` stopped refreshing that file once it moved to
/// the keyring, so on such installs it only holds an expired bearer.
fn read_token() -> Option<(String, Option<String>)> {
    keyring_token()
        .and_then(|raw| parse_token(&raw))
        .or_else(|| {
            let path = cli_dir()?.join("antigravity-oauth-token");
            parse_token(&std::fs::read_to_string(path).ok()?)
        })
}

/// A locked keyring may prompt; `/usage` must not wait on it.
const KEYRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

fn keyring_token() -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new("secret-tool")
        .args(["lookup", "service", "gemini", "username", "antigravity"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + KEYRING_TIMEOUT;
    while child.try_wait().ok()?.is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out.trim().to_string()).filter(|s| !s.is_empty())
}

fn parse_token(raw: &str) -> Option<(String, Option<String>)> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let access = v["token"]["access_token"].as_str()?.to_string();
    let refresh = v["token"]["refresh_token"].as_str().map(str::to_string);
    Some((access, refresh))
}

/// `retrieveUserQuotaSummary` -> usage windows. The live shape nests
/// buckets under model groups (`groups[].buckets[]`: "Gemini Models" with
/// `gemini-weekly` / `gemini-5h`, a third-party group, ...), each bucket
/// carrying `window`, `remainingFraction` and `resetTime`. A flat
/// `buckets[]` is accepted too.
fn quota_windows(q: &Value) -> Vec<Value> {
    let empty = Vec::new();
    let mut out = Vec::new();
    let mut push = |group: Option<&str>, b: &Value| {
        let id = b
            .get("bucketId")
            .or_else(|| b.get("modelId"))
            .or_else(|| b.get("model"))
            .and_then(Value::as_str)
            .unwrap_or("quota");
        let (kind, mins, span): (&str, Option<u64>, String) =
            match b.get("window").and_then(Value::as_str).unwrap_or("") {
                "weekly" => ("weekly", Some(7 * 24 * 60), "Weekly".into()),
                "5h" => ("session", Some(5 * 60), "5h".into()),
                "daily" => ("session", Some(24 * 60), "Daily".into()),
                "" => ("session", None, id.to_string()),
                other => ("other", None, other.to_string()),
            };
        let label = match group
            .map(|g| g.trim_end_matches(" Models").trim())
            .filter(|g| !g.is_empty())
        {
            Some(g) => format!("{g} · {span}"),
            None => span,
        };
        let remaining = b
            .get("remainingFraction")
            .or_else(|| b.get("remaining_fraction"))
            .and_then(Value::as_f64);
        let mut w = json!({
            "id": format!("quota_{id}"),
            "label": label,
            "kind": kind,
            "used_percent": remaining.map(|r| ((1.0 - r) * 100.0).clamp(0.0, 100.0)),
        });
        if let Some(m) = mins {
            w["duration_mins"] = json!(m);
        }
        if let Some(r) = b
            .get("resetTime")
            .or_else(|| b.get("reset_time"))
            .and_then(Value::as_str)
        {
            w["resets_at"] = json!(r);
        }
        out.push(w);
    };
    for g in q.get("groups").and_then(Value::as_array).unwrap_or(&empty) {
        let name = g.get("displayName").and_then(Value::as_str);
        for b in g.get("buckets").and_then(Value::as_array).unwrap_or(&empty) {
            push(name, b);
        }
    }
    for b in q.get("buckets").and_then(Value::as_array).unwrap_or(&empty) {
        push(None, b);
    }
    out
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
        // The backend keys the account view on the client: under ureq's
        // default agent `loadCodeAssist` answers only the tier catalogue
        // (no current tier, no project), so the quota call never ran.
        .set("User-Agent", "antigravity")
        .send_string(body)
    {
        Ok(r) => serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string()),
        Err(ureq::Error::Status(code, r)) => {
            let b = r.into_string().unwrap_or_default();
            Err(format!(
                "{method} HTTP {code}: {}",
                b.chars().take(160).collect::<String>()
            ))
        }
        Err(e) => Err(format!("{method}: {e}")),
    }
}

/// The full live probe: `loadCodeAssist` resolves the tier/project, then
/// `retrieveUserQuotaSummary` lists per-model buckets. First 401 retry
/// refreshes the bearer once.
fn probe() -> Result<Value, String> {
    let (mut access, _refresh) = read_token().ok_or_else(|| "no agy credential".to_string())?;
    let meta =
        r#"{"metadata":{"ideType":"ANTIGRAVITY","platform":"LINUX_AMD64","pluginType":"GEMINI"}}"#;
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
        .pointer("/paidTier/name")
        .or_else(|| lca.pointer("/currentTier/name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let project = lca
        .get("cloudaicompanionProject")
        .and_then(|p| p.as_str().or_else(|| p.get("id").and_then(Value::as_str)))
        .unwrap_or_default()
        .to_string();

    let windows = if project.is_empty() {
        Vec::new()
    } else {
        let body = json!({"project": project}).to_string();
        post(&access, "retrieveUserQuotaSummary", &body)
            .map(|q| quota_windows(&q))
            .unwrap_or_default()
    };
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
            && limits["windows"]
                .as_array()
                .map(|w| w.is_empty())
                .unwrap_or(true)
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

#[cfg(test)]
mod quota_tests {
    use super::*;

    #[test]
    fn grouped_buckets_become_labelled_windows() {
        let q = json!({"groups": [
            {"displayName": "Gemini Models", "buckets": [
                {"bucketId": "gemini-weekly", "window": "weekly",
                 "resetTime": "2026-10-15T17:49:04Z", "remainingFraction": 0.75},
                {"bucketId": "gemini-5h", "window": "5h",
                 "resetTime": "2026-10-11T16:12:45Z", "remainingFraction": 1}]},
            {"displayName": "Third Party Models", "buckets": [
                {"bucketId": "3p-weekly", "window": "weekly", "remainingFraction": 1}]}
        ]});
        let w = quota_windows(&q);
        let labels: Vec<&str> = w.iter().map(|w| w["label"].as_str().unwrap()).collect();
        assert_eq!(
            labels,
            vec!["Gemini · Weekly", "Gemini · 5h", "Third Party · Weekly"]
        );
        assert_eq!(w[0]["used_percent"], 25.0);
        assert_eq!(w[0]["kind"], "weekly");
        assert_eq!(w[0]["resets_at"], "2026-10-15T17:49:04Z");
        assert_eq!(w[1]["used_percent"], 0.0);
        assert_eq!(w[1]["duration_mins"], 300);
        assert!(w[2].get("resets_at").is_none());
    }

    #[test]
    fn flat_buckets_and_empty_payloads_still_map() {
        let w = quota_windows(&json!({"buckets": [{"modelId": "m", "remainingFraction": 0.5}]}));
        assert_eq!(w[0]["label"], "m");
        assert_eq!(w[0]["used_percent"], 50.0);
        assert!(quota_windows(&json!({})).is_empty());
    }

    #[test]
    fn token_parses_from_the_agy_envelope() {
        let raw = r#"{"auth_method":"consumer","token":{"access_token":"a","refresh_token":"r"}}"#;
        assert_eq!(parse_token(raw), Some(("a".into(), Some("r".into()))));
        assert_eq!(parse_token("not json"), None);
    }

    #[test]
    #[ignore]
    fn live() {
        println!("LIVE {}", handle().unwrap());
    }
}
