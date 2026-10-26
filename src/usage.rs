//! `provider/usage`: Antigravity subscription quota, as far as gray can see
//! it without reading agy's credentials or calling any internal API.
//!
//! - **`quota-event` write-through**: when the chat fold sees a quota wall in
//!   the official `agy` CLI's output, it stamps this plugin's own state file
//!   (`~/.cache/antigravity-sub/quota-event.json`). That renders as a 100%
//!   row until the event is six hours old.
//! - Otherwise the card reports the provider honestly: connected, with a note
//!   that gray has no quota feed for this account.
//!
//! There is no live quota probe. The old one read agy's OAuth token from the
//! OS keyring or token file and called Google's internal Code Assist endpoints
//! while sending a spoofed `antigravity` User-Agent. Both were removed.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use gray_plugin::ProviderRpcError;

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

fn event_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cache/antigravity-sub/quota-event.json"))
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

/// `provider/usage` entry: quota-event → honest card. Nothing is fetched
/// from the network, and the plugin holds no agy credential.
pub fn handle() -> Result<Value, ProviderRpcError> {
    if let Some(event) = read_event() {
        return Ok(json!({
            "available": true,
            "title": "Antigravity",
            "windows": [event_window(&event)],
            "checked_at": iso(now_secs()),
            "note": event["detail"].clone(),
        }));
    }
    // Connected, but gray can't read this account's quota (or it is logged
    // out): report the card with no fabricated bars.
    Ok(json!({
        "available": true,
        "title": "Antigravity",
        "windows": [],
        "checked_at": iso(now_secs()),
        "note": "gray can't read this account's quota; limits appear here after agy reports one",
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
    fn iso_formats_utc_and_leap_days() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn quota_event_renders_a_full_window() {
        let w = event_window(&json!({"detail": "Individual quota reached"}));
        assert_eq!(w["id"], "quota_event");
        assert_eq!(w["used_percent"], 100.0);
    }
}
