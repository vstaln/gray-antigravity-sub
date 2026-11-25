//! The host-tool allowlist the session advertises upstream.
//!
//! Default allows `bash`, `toolbox`, the `subagent_*` / `narrow_delegate`
//! family, gray's file tools (both dialects plus `apply_patch`), and core
//! built-ins (`memory`, `skill`, `skills_install`, `web_search`,
//! `web_fetch`, `request_user_input`). A harness may still carry more
//! behind `toolbox` or custom plugins; `/antigravity tools …` in the REPL
//! (the sidecar's `command/run`) and `antigravity-sub tools …` in a shell
//! are the same operation on the same file,
//! `<gray home>/antigravity-sub/tools.json`. `chat::prepare_turn`
//! re-reads it on every request, so a mid-session toggle applies to the
//! next turn with no respawn, and the file is safe to hand-edit —
//! missing or corrupt reads fail closed to the default allowlist.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::{Value, json};

/// `*` in the allowlist stands for every function tool the host sends.
const ALL: &str = "*";
/// The strict `/antigravity tools bash` surface.
const BASH: &str = "bash";
/// The default (and `/antigravity tools default` reset) surface: bash,
/// gray's toolbox meta-tool, subagent delegation, file tools, and core
/// host tools. A tool the host doesn't send is simply absent upstream.
const DEFAULT_TOOLS: &[&str] = &[
    BASH,
    "toolbox",
    "subagent_spawn",
    "subagent_check",
    "subagent_message",
    "subagent_stop",
    "subagent_note",
    "narrow_delegate",
    "read",
    "edit",
    "write",
    "grep",
    "glob",
    "Read",
    "Edit",
    "Write",
    "Grep",
    "Glob",
    "apply_patch",
    "memory",
    "skill",
    "skills_install",
    "web_search",
    "web_fetch",
    "request_user_input",
];

fn default_allow() -> BTreeSet<String> {
    DEFAULT_TOOLS.iter().map(|t| t.to_string()).collect()
}

/// Which host tools upstream may use. `prepare_turn` passes only
/// `allows`-passing tools into `names` and the `finish` funnel's tool
/// manifest, so a name the policy doesn't cover is never visible to the
/// model — and `fold_lines` rewrites any stray call outside `names` to
/// `native__…` so the host's "does not exist" error answers cleanly.
pub struct ToolPolicy {
    allow: BTreeSet<String>,
}

impl Default for ToolPolicy {
    /// Missing/corrupt file lands here: bash + core/toolbox/subagent/file tools.
    fn default() -> Self {
        Self {
            allow: default_allow(),
        }
    }
}

impl ToolPolicy {
    /// `name` may run upstream: it's listed, matches a `prefix*` entry
    /// (e.g. `mcp__*`), or the policy is `all`.
    pub fn allows(&self, name: &str) -> bool {
        self.allow.contains(ALL) || self.allow.contains(name) || self.matches_pattern(name)
    }

    fn matches_pattern(&self, name: &str) -> bool {
        self.allow
            .iter()
            .any(|p| p.len() > 1 && p.ends_with('*') && name.starts_with(&p[..p.len() - 1]))
    }

    /// The persisted policy, or the default. Never errors: a
    /// turn must never fail on a settings read.
    pub fn load() -> Self {
        let Some(path) = file_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        let allow: BTreeSet<String> = value
            .get("allow")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .filter(|name| valid_entry(name) || name == ALL)
                    .collect()
            })
            .unwrap_or_default();
        // An empty stored list is not a policy — keep the default so a
        // truncated write can't silently disarm the toolset.
        if allow.is_empty() {
            Self::default()
        } else {
            Self { allow }
        }
    }

    fn save(&self) -> Result<String, String> {
        let path = file_path().ok_or("cannot resolve GRAY_HOME")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
        }
        let allow: Vec<&String> = self.allow.iter().collect();
        std::fs::write(&path, json!({"allow": allow}).to_string()).map_err(|e| format!("{e}"))?;
        Ok(path.to_string_lossy().into_owned())
    }
}

/// `<gray home>/antigravity-sub/tools.json` — the same GRAY_HOME rule
/// the host uses (`gray_core::paths`), so a repo-local GRAY_HOME scopes
/// the policy.
fn file_path() -> Option<PathBuf> {
    gray_core::paths::gray_home().map(|h| h.join("antigravity-sub").join("tools.json"))
}

/// Tool names are ASCII identifiers the host's schema already constrains;
/// the policy file honors the same bound so junk can never name a tool.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 50
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// An allowlist entry: either an exact tool name or a `prefix*` wildcard.
fn valid_entry(entry: &str) -> bool {
    match entry.strip_suffix('*') {
        Some(prefix) => valid_name(prefix),
        None => valid_name(entry),
    }
}

/// The `/antigravity` command body: `tools` runs the policy command,
/// anything else prints usage plus the current policy. A bare
/// `/antigravity` never reaches this — `manifest::run_command` leaves it
/// unanswered so the host's provider-login shortcut (connect → model
/// picker) takes it.
pub fn command(argv: &[String]) -> String {
    match argv.first().map(String::as_str) {
        Some("account") => match crate::accounts::command(&argv[1..], false) {
            Ok(text) => text,
            Err(e) => e,
        },
        Some("tools") => tools_command(&argv[1..]),
        _ => format!("{USAGE}\n\n{}", status_line(&ToolPolicy::load())),
    }
}

const USAGE: &str = "\
/antigravity — switch the session to Antigravity (connect → model picker).
/antigravity tools — which harness tools the upstream Antigravity model may call.
  /antigravity tools          show the current policy
  /antigravity tools all      allow every harness tool the host sends
  /antigravity tools default  bash, toolbox, subagents, file & core tools (the default)
  /antigravity tools bash     bash only
  /antigravity tools +name    also allow one tool or prefix*, e.g. +mcp__*
  /antigravity tools -name    remove one
/antigravity account — the Antigravity logins: list · status · usage · use · strategy.
/antigravity commands — list Antigravity ACP slash commands (e.g. `/antigravity swarm …`).
/antigravity <command> [args…] — run an Antigravity ACP slash command under Gray's tools and rules.
Same from a shell: `gray antigravity-sub tools …`.";

fn status_line(policy: &ToolPolicy) -> String {
    if policy.allow.contains(ALL) {
        "upstream tools: all harness tools".to_string()
    } else {
        format!(
            "upstream tools: {}",
            policy.allow.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}

/// `tools` with no args prints the policy; each arg is `all` (allow
/// everything), `default` (reset to default), `bash` (bash only),
/// `+name`/`-name` (edit the list) or a bare name (same as `+name`).
/// Processed left to right; the first invalid arg stops with its
/// message, nothing written.
pub fn tools_command(args: &[String]) -> String {
    if args.is_empty() {
        return format!("{}\n\n{USAGE}", status_line(&ToolPolicy::load()));
    }
    let mut policy = ToolPolicy::load();
    for arg in args {
        match arg.as_str() {
            "all" => policy.allow = BTreeSet::from([ALL.to_string()]),
            "default" => policy.allow = default_allow(),
            "bash" => policy.allow = BTreeSet::from([BASH.to_string()]),
            _ => {
                let (add, name) = match arg.strip_prefix('-') {
                    Some(rest) => (false, rest),
                    None => (true, arg.strip_prefix('+').unwrap_or(arg)),
                };
                if policy.allow.contains(ALL) {
                    return "policy is `all`; reset with `/antigravity tools default` before \
                            editing the list"
                        .to_string();
                }
                if !valid_entry(name) {
                    return format!("bad tool name: {arg}");
                }
                if add {
                    policy.allow.insert(name.to_string());
                } else {
                    policy.allow.remove(name);
                }
            }
        }
    }
    if policy.allow.is_empty() {
        return "refusing to write an empty tool list — `/antigravity tools default` resets"
            .to_string();
    }
    match policy.save() {
        Ok(path) => format!("{}\n(saved to {path})", status_line(&policy)),
        Err(e) => format!("could not save policy: {e}"),
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn default_policy_allows_toolbox_subagents_and_core_tools() {
        let p = ToolPolicy::default();
        for t in DEFAULT_TOOLS {
            assert!(p.allows(t), "{t} should be allowed by default");
        }
        assert!(!p.allows("discord_send"));
        assert!(!p.allows("mcp__apify__search_actors"));
    }

    #[test]
    fn prefix_pattern_matches_and_validates() {
        let p = ToolPolicy {
            allow: BTreeSet::from(["bash".into(), "mcp__*".into()]),
        };
        assert!(p.allows("mcp__treg__call"));
        assert!(!p.allows("discord_send"));
        assert!(valid_entry("mcp__*"));
        assert!(!valid_entry("*"));
        assert!(!valid_entry("mcp*foo"));
    }

    #[test]
    fn default_tool_names_are_valid() {
        for t in DEFAULT_TOOLS {
            assert!(valid_name(t), "{t}");
        }
    }
}
