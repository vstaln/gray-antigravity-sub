use serde_json::{Value, json};

use crate::{manifest, usage};

const SINGLE_LOGIN: &str = "antigravity: one keyring login only; multi-account waits on agy";

const USAGE: &str = "\
/antigravity account — the Antigravity login (one keyring login; no pool yet)
  account list [--json]                     the ambient login and how it serves
  account usage [--json]                    quota card for the ambient login
  account add|use|remove|strategy|status    refused: one keyring login only";

#[derive(Clone, Copy)]
enum Health {
    Ready,
}

impl Health {
    fn json(self) -> Value {
        match self {
            Self::Ready => json!({"state": "ready"}),
        }
    }
}

#[derive(Clone, Copy)]
enum Serving {
    Ambient,
}

impl Serving {
    fn json(self) -> Value {
        match self {
            Self::Ambient => json!({"kind": "ambient"}),
        }
    }
}

enum Fetch {
    Loaded(Value),
    Failed(String),
}

impl Fetch {
    fn entry(self) -> Value {
        match self {
            Self::Loaded(card) => json!({"label": null, "usage": card, "error": null}),
            Self::Failed(msg) => json!({"label": null, "usage": null, "error": msg}),
        }
    }
}

pub fn command(args: &[String]) -> Result<String, String> {
    let json = args.iter().any(|a| a == "--json");
    let rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json")
        .collect();
    match rest.as_slice() {
        [] => Ok(USAGE.to_string()),
        ["list"] => Ok(list(json)),
        ["usage"] => Ok(usage_report(json)),
        ["usage", _] => Err(SINGLE_LOGIN.to_string()),
        ["add" | "use" | "remove" | "strategy" | "status", ..] => Err(SINGLE_LOGIN.to_string()),
        _ => Err(format!("unknown account command\n{USAGE}")),
    }
}

fn list(json: bool) -> String {
    if !json {
        return "antigravity: ambient keyring login (no pool)".to_string();
    }
    pretty(json!({
        "version": 1,
        "plugin": manifest::PLUGIN_NAME,
        "provider": manifest::PROVIDER_ID,
        "strategy": "fill_first",
        "selected": null,
        "ambient": true,
        "accounts": [],
        "serving": Serving::Ambient.json(),
        "health": Health::Ready.json(),
    }))
}

fn usage_report(json: bool) -> String {
    let fetch = match usage::handle() {
        Ok(card) => Fetch::Loaded(card),
        Err(e) => Fetch::Failed(e.to_string()),
    };
    if json {
        return pretty(json!({
            "version": 1,
            "plugin": manifest::PLUGIN_NAME,
            "provider": manifest::PROVIDER_ID,
            "accounts": [fetch.entry()],
        }));
    }
    match fetch {
        Fetch::Loaded(card) => card_text(&card),
        Fetch::Failed(msg) => format!("ambient: error: {msg}"),
    }
}

fn card_text(card: &Value) -> String {
    let mut out = String::from("ambient");
    if let Some(wins) = card.get("windows").and_then(Value::as_array) {
        for w in wins {
            let label = w.get("label").and_then(Value::as_str).unwrap_or("?");
            let pct = w
                .get("used_percent")
                .and_then(Value::as_f64)
                .map_or("?".to_string(), |p| format!("{p:.0}%"));
            out.push_str(&format!("\n  {label}  {pct}"));
        }
    }
    if let Some(note) = card.get("note").and_then(Value::as_str) {
        out.push_str(&format!("\n  note: {note}"));
    }
    out
}

fn pretty(v: Value) -> String {
    serde_json::to_string_pretty(&v).unwrap_or_default()
}
