//! Pinned native routes: the full `agy models` ids, verified live.
//!
//! `agy` couples model and effort in the id itself (`gemini-3.8-flash-low`):
//! full ids run WITHOUT `--effort`, base ids REQUIRE it (and reject a
//! mismatched one). The catalog therefore pins full ids and the spawn path
//! never passes `--effort` — effort levels are separate models, not a knob.
//!
//! Context windows are the values the proxy ecosystem reports for these
//! routes (Gemini ~1M, Claude Sonnet 200K, Claude Opus 1M); `gpt-oss` is
//! unknown so it reports `None` rather than a guess.

/// Canonical full id → context window (`None` = unknown, never guessed).
fn context_windows() -> &'static [(&'static str, Option<u32>)] {
    &[
        ("gemini-3.8-flash-high", Some(1_048_576)),
        ("gemini-3.8-flash-medium", Some(1_048_576)),
        ("gemini-3.8-flash-low", Some(1_048_576)),
        ("gemini-3.7-flash-high", Some(1_048_576)),
        ("gemini-3.7-flash-medium", Some(1_048_576)),
        ("gemini-3.7-flash-low", Some(1_048_576)),
        ("gemini-3.6-flash-high", Some(1_048_576)),
        ("gemini-3.6-flash-medium", Some(1_048_576)),
        ("gemini-3.6-flash-low", Some(1_048_576)),
        ("gemini-3.1-pro-high", Some(1_048_576)),
        ("gemini-3.1-pro-low", Some(1_048_576)),
        ("claude-opus-5-5-low", Some(1_000_000)),
        ("claude-opus-5-5-medium", Some(1_000_000)),
        ("claude-opus-5-5-high", Some(1_000_000)),
        ("claude-sonnet-5-5-low", Some(200_000)),
        ("claude-sonnet-5-5-medium", Some(200_000)),
        ("claude-sonnet-5-5-high", Some(200_000)),
        ("gpt-oss-120b-medium", None),
    ]
}

fn aliases() -> &'static [(&'static str, &'static str)] {
    &[
        ("flash", "gemini-3.8-flash-low"),
        ("pro", "gemini-3.1-pro-low"),
        ("sonnet", "claude-sonnet-5-5-low"),
        ("opus", "claude-opus-5-5-low"),
    ]
}

fn canonical(model: &str) -> &str {
    aliases()
        .iter()
        .find(|(a, _)| *a == model)
        .map(|(_, c)| *c)
        .unwrap_or(model)
}

/// Context window for a route: pinned table, `None` when unknown.
/// Unpinned future ids also report `None` — never a guessed window.
pub fn context_window(model: &str) -> Option<u32> {
    let canon = canonical(model);
    context_windows()
        .iter()
        .find(|(id, _)| *id == canon)
        .and_then(|(_, w)| *w)
}

/// Native `--model` selection: aliases resolve to full ids, everything else
/// passes through untouched so future `agy models` entries keep working.
pub fn native_model(model: &str) -> String {
    canonical(model).to_string()
}

/// Every routable id: full ids plus aliases.
pub fn all_ids() -> Vec<String> {
    let mut out: Vec<String> = context_windows()
        .iter()
        .map(|(id, _)| id.to_string())
        .collect();
    for (alias, _) in aliases() {
        out.push(alias.to_string());
    }
    out.sort();
    out.dedup();
    out
}

/// Display name for a route id.
pub fn display_name(id: &str) -> String {
    let canon = canonical(id);
    let short = canon.replace('-', " ");
    // Title-case each word without pulling in a dependency.
    let titled = short
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if context_windows().iter().any(|(known, _)| *known == canon) {
        format!("{titled} (Antigravity)")
    } else {
        format!("{titled} (Antigravity, unpinned)")
    }
}

#[path = "catalog_tests.rs"]
#[cfg(test)]
mod tests;
