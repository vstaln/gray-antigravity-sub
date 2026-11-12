//! `provider/usage`: Antigravity subscription quota.
//!
//! - **Live**: the official `agy -p /usage --output-format json` report,
//!   one window per quota bucket. gray never touches agy's credentials.
//! - **`quota-event` write-through**: when the live report is unavailable and
//!   the chat fold saw a quota wall, it renders as a 100% row until the event
//!   is six hours old.
//! - Otherwise the card reports the provider as connected with no bars.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    let at = now_secs();
    let body = json!({
        "detail": detail.chars().take(200).collect::<String>(),
        "at": at,
    });
    let _ = locked_update(&path, |current| {
        let stored = current
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v.get("at").and_then(Value::as_u64));
        match stored {
            Some(newer) if newer > at => Update::Keep,
            Some(_) | None => Update::Replace(body.to_string()),
        }
    });
}

enum Update {
    Keep,
    Replace(String),
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

fn locked_update(path: &Path, apply: impl FnOnce(Option<String>) -> Update) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("quota event path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(sidecar(path, "lock"))?;
    lock.lock()?;
    let current = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    match apply(current) {
        Update::Keep => {}
        Update::Replace(text) => write_atomic(path, text.as_bytes())?,
    }
    lock.unlock()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = sidecar(path, &format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
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
    if let Some(windows) = live_windows() {
        return Ok(json!({
            "available": true,
            "title": "Antigravity",
            "windows": windows,
            "checked_at": iso(now_secs()),
        }));
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

const LIVE_TIMEOUT: Duration = Duration::from_secs(10);

fn live_windows() -> Option<Vec<Value>> {
    let out = crate::discover::capture(&["-p", "/usage", "--output-format", "json"], LIVE_TIMEOUT)?;
    parse_live(&serde_json::from_str(&out).ok()?)
}

pub fn parse_live(doc: &Value) -> Option<Vec<Value>> {
    let groups = doc.pointer("/command/data/groups")?.as_array()?;
    let windows: Vec<Value> = groups
        .iter()
        .flat_map(|g| {
            let group = group_short(g["name"].as_str().unwrap_or(""));
            g["buckets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |b| bucket_window(&group, b))
        })
        .collect();
    (!windows.is_empty()).then_some(windows)
}

fn group_short(name: &str) -> String {
    match name {
        "Gemini Models" => "Gemini".to_string(),
        "Claude and GPT models" => "Claude/GPT".to_string(),
        n => n.strip_prefix("Gemini 4 ").unwrap_or(n).to_string(),
    }
}

fn bucket_window(group: &str, b: &Value) -> Option<Value> {
    let remaining = b["remaining_fraction"].as_f64()?;
    let (span, kind) = match b["window"].as_str() {
        Some("5h") => ("5h", "session"),
        Some("weekly") => ("Weekly", "weekly"),
        Some(other) => (other, "other"),
        None => ("Quota", "other"),
    };
    Some(json!({
        "id": b["id"].clone(),
        "label": format!("{span} \u{b7} {group}"),
        "kind": kind,
        "used_percent": ((1.0 - remaining) * 100.0).clamp(0.0, 100.0),
        "resets_at": b["reset_time"].clone(),
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
