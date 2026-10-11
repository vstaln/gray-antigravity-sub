//! `/antigravity status` — one glanceable panel: `agy` login state, the
//! Google account, subscription quota. Renders `usage::handle()`'s
//! ProviderUsageLimits so the panel and `/usage` can never disagree.

use serde_json::Value;

use crate::{setup, usage};

/// `~/.gemini/google_accounts.json` — Google's own "active account" file,
/// metadata only (no tokens): `{active: email, old: [emails]}`.
fn google_account() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(
        std::path::PathBuf::from(home).join(".gemini/google_accounts.json"),
    )
    .ok()?;
    serde_json::from_str::<Value>(&text).ok()?["active"]
        .as_str()
        .map(str::to_string)
}

fn window_line(w: &Value) -> Option<String> {
    let label = w.get("label")?.as_str()?;
    let mut line = format!("  {label}:");
    if let Some(p) = w.get("used_percent").and_then(Value::as_f64) {
        line.push_str(&format!(" {p:.0}% used"));
    }
    if let Some(r) = w.get("resets_at").and_then(Value::as_str) {
        line.push_str(&format!(" · resets {r}"));
    }
    Some(line)
}

pub fn text() -> String {
    let mut out = "Antigravity (unofficial)".to_string();
    match setup::probe_login() {
        setup::LoginState::LoggedIn => {
            out.push_str("\n  login: agy oauth (staged token) · logged in")
        }
        setup::LoginState::LoggedOut => {
            out.push_str("\n  login: logged out — run `agy` once to sign in")
        }
        setup::LoginState::Unknown => out.push_str("\n  login: unknown (probe inconclusive)"),
    }
    if let Some(email) = google_account() {
        out.push_str(&format!("\n  acct:  {email}"));
    }
    match usage::handle() {
        Ok(v) => {
            if let Some(plan) = v.get("plan").and_then(Value::as_str) {
                out.push_str(&format!("\n  plan:  {plan}"));
            }
            if let Some(wins) = v.get("windows").and_then(Value::as_array) {
                for w in wins {
                    if let Some(line) = window_line(w) {
                        out.push('\n');
                        out.push_str(&line);
                    }
                }
            }
            if let Some(note) = v.get("note").and_then(Value::as_str)
                && !note.is_empty()
            {
                out.push_str(&format!("\n  note:  {note}"));
            }
        }
        Err(e) => out.push_str(&format!("\n  usage: ({e})")),
    }
    out
}
