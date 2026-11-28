use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use gray_acp::acp::{AcpClient, SpawnSpec};
use gray_acp::chat::{PreparedTurn, Usage};
use gray_acp::session::{Driver, PermissionDecision, SlashCommand};
use serde_json::{Map, Value, json};

use crate::{accounts, catalog, settings::ToolPolicy, setup, store, usage};

const SERVER: &str = "gray";
const MCP_CALL_TIMEOUT_SECS: u64 = 24 * 3600;
const TOOL_NOTE: &str = "Tools: use only the tools of the `gray` MCP server (named mcp_gray_<tool>, \
    or reached through call_mcp_tool with ServerName \"gray\"). Antigravity's built-in workspace, \
    shell, file, and external tools are switched off in this harness and every call to one is refused. \
    When an Antigravity slash command (such as /swarm, /plan, /grill-me, /teamwork, /boost, /goal, \
    /schedule, or /run-workflow) is active, you may use its built-in coordination and orchestration \
    tools (swarm, swarm_wait, post_message, read_board, ask_question, update_steps, schedule, \
    run_workflow, invoke_subagent, define_subagent, manage_subagents, send_message), but all \
    workspace, shell, file, and external actions — including inside swarm workers and subagents — \
    must still use only mcp_gray_* tools. The tool descriptions above use the bare names (for \
    example `bash`); they are the same tools.";

const ORCHESTRATION_TOOLS: &[&str] = &[
    "swarm",
    "swarm_wait",
    "post_message",
    "read_board",
    "ask_question",
    "update_steps",
    "schedule",
    "run_workflow",
    "invoke_subagent",
    "define_subagent",
    "manage_subagents",
    "send_message",
];

const DEFAULT_COMMANDS: &[(&str, &str)] = &[
    (
        "swarm",
        "Run a swarm of agents working in parallel through a shared message board.",
    ),
    ("plan", "Plan carefully before executing a task."),
    ("grill-me", "Interview to align on a plan."),
    (
        "boost",
        "Invoke the Boost multi-agent orchestrator for complex tasks.",
    ),
    (
        "teamwork",
        "Invoke a team of agents to autonomously tackle large projects.",
    ),
    (
        "goal",
        "Run until the specified goal is completely finished.",
    ),
    (
        "schedule",
        "Run an instruction on a recurring schedule or as a one-time timer.",
    ),
    ("browser", "Invoke a browser agent for web tasks."),
    (
        "compact",
        "Compact the conversation to free up the context window.",
    ),
    (
        "learn",
        "Reflect on recent successes or corrections to capture reusable skills or rules.",
    ),
    (
        "run-workflow",
        "Write and run a dynamic workflow script orchestrating agents.",
    ),
    (
        "agy-customizations",
        "Guide and reference for the Antigravity customization system.",
    ),
    (
        "antigravity-guide",
        "Guide and quick reference for Google Antigravity.",
    ),
];

#[derive(Default)]
pub struct AgyDriver {
    picks: Mutex<HashMap<PathBuf, Option<accounts::Pick>>>,
}

pub fn credential_file() -> Option<PathBuf> {
    let file = setup::home_dir()?
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");
    file.is_file().then_some(file)
}

pub fn has_auth_store() -> bool {
    credential_file().is_some()
        || accounts::has_token()
        || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty())
        || cfg!(target_os = "macos")
        // Windows keeps the login in Credential Manager; report "maybe" and
        // let the staged probe (`setup::probe_login`) answer for real.
        || cfg!(windows)
}

pub fn use_token(account_home: Option<&Path>) -> Option<PathBuf> {
    match account_home {
        Some(home) => accounts::token_of(home),
        None => accounts::probe_home()
            .and_then(|home| accounts::token_of(&home))
            .or_else(credential_file),
    }
}

pub enum Verdict {
    Unauthorized,
    RateLimited { retry_after: Option<Duration> },
}

pub fn verdict_of(detail: &str) -> Option<Verdict> {
    let low = detail.to_lowercase();
    let auth = [
        "unauthenticated",
        "authentication required",
        "authentication failed or timed out",
        "(401)",
        "401 ",
        "code 401",
        "invalid_grant",
    ]
    .iter()
    .any(|w| low.contains(w));
    if auth {
        return Some(Verdict::Unauthorized);
    }
    let quota = [
        "resource_exhausted",
        "(429)",
        "code 429",
        "individual quota reached",
        "quota exceeded",
        "rate limit",
    ]
    .iter()
    .any(|w| low.contains(w));
    quota.then(|| Verdict::RateLimited {
        retry_after: resets_in(&low),
    })
}

fn duration_secs(text: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut any = false;
    let mut n = 0u64;
    let mut digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            n = n
                .saturating_mul(10)
                .saturating_add(u64::from(c as u8 - b'0'));
            digits = true;
            continue;
        }
        let unit = match c {
            'd' => 86_400,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => {
                if digits {
                    return None;
                }
                continue;
            }
        };
        if digits {
            total = total.saturating_add(n.saturating_mul(unit));
            any = true;
            n = 0;
            digits = false;
        }
    }
    (any && total > 0).then_some(total)
}

fn resets_in(low: &str) -> Option<Duration> {
    const MAX_RESET: u64 = 8 * 24 * 3600;
    let (_, rest) = low.split_once("resets in ")?;
    let secs = duration_secs(rest.trim_start())?;
    Some(Duration::from_secs(secs.min(MAX_RESET)))
}

#[cfg(unix)]
pub(crate) fn link(src: &Path, dst: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(src, dst).map_err(|e| format!("staging link: {e}"))
}

/// Windows symlinks need Developer Mode or elevation, so a directory gets
/// a junction and a file a hard link (both unprivileged), with a copy as
/// the fallback for a file on another volume.
#[cfg(windows)]
pub(crate) fn link(src: &Path, dst: &Path) -> Result<(), String> {
    if src.is_dir() {
        return junction::create(src, dst).map_err(|e| format!("staging junction: {e}"));
    }
    std::fs::hard_link(src, dst)
        .or_else(|_| std::fs::copy(src, dst).map(|_| ()))
        .map_err(|e| format!("staging link: {e}"))
}

fn clamp_tier(family: &str, tier: &str) -> &'static str {
    match (family, tier) {
        ("gemini-4-neon" | "gemini-4-hydrogen", "xhigh") => "xhigh",
        (_, "xhigh" | "high") => "high",
        (_, "low") => "low",
        _ => "medium",
    }
}

fn route(model: &str, effort: Option<&str>) -> (String, Option<Value>) {
    let native = catalog::native_model_for_effort(model, effort);
    let parts: Vec<&str> = native.split("--").collect();
    if parts.len() == 4 && parts[0] == "gemini-pro-orchestrator" {
        let (planner, executor, tier) = (parts[1], parts[2], parts[3]);
        let wire = if planner == "gemini-4-argon" || planner == "g1-pro" {
            "gemini-pro-orchestrator-g1-pro"
        } else {
            "gemini-pro-orchestrator-neon"
        };
        let eff = effort.unwrap_or(tier);
        return (
            wire.to_string(),
            Some(json!({
                "planner": format!("{planner}-{}", clamp_tier(planner, eff)),
                "executor": format!("{executor}-{}", clamp_tier(executor, eff)),
            })),
        );
    }
    if native.starts_with("gemini-pro-orchestrator-")
        && let Some(eff) = effort
    {
        let planner = if native.ends_with("g1-pro") || native.ends_with("argon") {
            "gemini-4-argon"
        } else {
            "gemini-4-neon"
        };
        return (
            native,
            Some(json!({
                "planner": format!("{planner}-{}", clamp_tier(planner, eff)),
                "executor": format!("gemini-4-hydrogen-{}", clamp_tier("gemini-4-hydrogen", eff)),
            })),
        );
    }
    (native, None)
}

fn gray_title(title: &str) -> bool {
    let name = title
        .trim()
        .trim_end_matches('?')
        .strip_prefix("Running ")
        .or_else(|| title.trim().strip_prefix("Run "))
        .unwrap_or("");
    name.starts_with("mcp_gray_")
}

fn commands_file() -> Option<PathBuf> {
    gray_core::paths::gray_home().map(|h| h.join("antigravity-sub").join("commands.json"))
}

pub fn cached_commands() -> Vec<SlashCommand> {
    if let Some(path) = commands_file()
        && let Ok(text) = std::fs::read_to_string(path)
        && let Ok(cmds) = serde_json::from_str::<Vec<SlashCommand>>(&text)
        && !cmds.is_empty()
    {
        return cmds;
    }
    DEFAULT_COMMANDS
        .iter()
        .map(|(name, description)| SlashCommand {
            name: (*name).to_string(),
            description: (*description).to_string(),
        })
        .collect()
}

fn bare_title_name(title: &str) -> &str {
    let trimmed = title.trim().trim_end_matches('?');
    let rest = trimmed
        .strip_prefix("Running ")
        .or_else(|| trimmed.strip_prefix("Run "))
        .unwrap_or(trimmed);
    rest.split_whitespace().next().unwrap_or("")
}

fn is_swarm_orchestration_call(tool_call: &Value) -> bool {
    let raw = tool_call.get("rawInput");
    if raw.and_then(|r| r.get("CommandLine")).is_some() {
        return false;
    }
    let name_match = tool_call
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| ORCHESTRATION_TOOLS.contains(&n));
    let title_match = tool_call
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|t| ORCHESTRATION_TOOLS.contains(&bare_title_name(t)));
    name_match || title_match
}

fn is_gray_call(tool_call: &Value) -> bool {
    let raw = tool_call.get("rawInput");
    if raw
        .and_then(|r| r.get("ServerName"))
        .and_then(Value::as_str)
        .is_some_and(|s| s == SERVER)
    {
        return true;
    }
    if raw.and_then(|r| r.get("CommandLine")).is_some() {
        return false;
    }
    tool_call
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| n.starts_with("mcp_gray_"))
        || tool_call
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(gray_title)
}

impl AgyDriver {
    fn pick_of(&self, stage: &Path) -> Option<accounts::Pick> {
        self.picks
            .lock()
            .ok()
            .and_then(|m| m.get(stage).cloned().flatten())
    }
}

impl Driver for AgyDriver {
    fn spawn_spec(&self, stage: &Path) -> SpawnSpec {
        let mut env = setup::child_env();
        env.retain(|(k, _)| {
            !matches!(
                k.as_str(),
                "HOME" | "USERPROFILE" | "XDG_CONFIG_HOME" | "XDG_DATA_HOME" | "XDG_CACHE_HOME"
            )
        });
        env.extend(setup::staged_home_env(stage));
        for (k, sub) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_CACHE_HOME", ".cache"),
        ] {
            env.push((k.into(), stage.join(sub).display().to_string()));
        }
        for pair in setup::account_env(stage) {
            env.retain(|(k, _)| k != &pair.0);
            env.push(pair);
        }
        SpawnSpec {
            command: setup::resolve_command().unwrap_or_else(|| "agy".into()),
            args: vec!["acp".into()],
            env,
            cwd: stage.to_path_buf(),
        }
    }

    fn auth_method_id(&self) -> Option<&'static str> {
        Some("oauth-personal")
    }

    fn client_info(&self) -> (&'static str, &'static str) {
        ("gray-antigravity-sub", env!("CARGO_PKG_VERSION"))
    }

    fn stage(
        &self,
        stage: &Path,
        mcp_url: &str,
        tools: &[Value],
        prep: &PreparedTurn,
    ) -> Result<Option<Value>, String> {
        if let Some(key) = setup::conflicting_env() {
            return Err(format!(
                "subscription provider refuses conflicting {key}: unset it so agy uses your Antigravity login"
            ));
        }
        let pick = accounts::pick_for(&prep.model)?;
        let account_home = pick.as_ref().and_then(|p| p.home.clone());
        let token = match account_home.as_deref() {
            Some(home) => match use_token(Some(home)) {
                None if accounts::requires_file_login(home) => {
                    return Err(format!(
                        "the picked antigravity account has no login in {}",
                        home.display()
                    ));
                }
                login => login,
            },
            None if !has_auth_store() => return Err(setup::LOGIN_HINT.to_string()),
            None => credential_file(),
        };
        let cli = stage.join(".gemini").join("antigravity-cli");
        let config = stage.join(".gemini").join("config");
        for dir in [&cli, &config] {
            std::fs::create_dir_all(dir).map_err(|e| format!("staging dir: {e}"))?;
        }
        if let Some(src) = token {
            link(&src, &cli.join("antigravity-oauth-token"))?;
        }
        if let Some(bin) = setup::home_dir()
            .map(|h| h.join(".gemini/antigravity-cli/bin"))
            .filter(|p| p.is_dir())
        {
            let _ = link(&bin, &cli.join("bin"));
        }
        let mut settings = json!({"toolPermission": "request-review"});
        if let (_, Some(orchestrator)) = route(&prep.model, prep.effort.as_deref()) {
            settings["orchestrator"] = orchestrator;
        }
        std::fs::write(cli.join("settings.json"), settings.to_string())
            .map_err(|e| format!("staging settings.json: {e}"))?;
        // Suppress agy's background auto-updater in a fresh stage: without
        // this stamp agy treats the stage as a first run and spawns
        // `agy --bg-updater` / `agy --version`, flashing console windows on
        // Windows and risking binary swaps mid-session.
        std::fs::write(cli.join("last_check.timestamp"), b"")
            .map_err(|e| format!("staging last_check.timestamp: {e}"))?;
        let policy = ToolPolicy::load();
        let eager: Map<String, Value> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .filter(|name| policy.allows(name))
            .map(|name| (name.to_string(), json!({"eager": true})))
            .collect();
        let mcp = json!({"mcpServers": {SERVER: {
            "url": mcp_url,
            "timeoutSeconds": MCP_CALL_TIMEOUT_SECS,
            "tools": eager,
        }}});
        std::fs::write(config.join("mcp_config.json"), mcp.to_string())
            .map_err(|e| format!("staging mcp_config.json: {e}"))?;
        if let Ok(mut picks) = self.picks.lock() {
            picks.insert(stage.to_path_buf(), pick);
        }
        Ok(Some(json!([])))
    }

    fn session_meta(&self, init: &Value, system: &str) -> Option<Value> {
        let append = init
            .pointer("/agentCapabilities/_meta/antigravity/systemPrompt/append")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        append.then(|| {
            json!({"antigravity": {"systemPrompt": {"append": format!("{system}\n\n{TOOL_NOTE}")}}})
        })
    }

    fn apply_model(
        &self,
        client: &mut AcpClient,
        session_id: &str,
        _session_new_result: &Value,
        model: &str,
        effort: Option<&str>,
        _overrides: &[(String, String)],
    ) -> Result<(), String> {
        let (wire, _) = route(model, effort);
        client
            .set_model(session_id, &wire, Value::Null)
            .map_err(|e| format!("antigravity model `{wire}`: {e}"))
    }

    fn permission_option(&self, params: &Value) -> PermissionDecision {
        let allowed = params
            .get("toolCall")
            .is_some_and(|tc| is_gray_call(tc) || is_swarm_orchestration_call(tc));
        if !allowed {
            return PermissionDecision::Deny;
        }
        let options = params.get("options").and_then(Value::as_array);
        let id = options
            .and_then(|o| {
                o.iter()
                    .find(|opt| opt.get("kind").and_then(Value::as_str) == Some("allow_once"))
                    .or_else(|| {
                        o.iter().find(|opt| {
                            opt.get("kind")
                                .and_then(Value::as_str)
                                .is_some_and(|k| k.starts_with("allow"))
                        })
                    })
            })
            .and_then(|opt| opt.get("optionId").and_then(Value::as_str))
            .unwrap_or("allow_once")
            .to_string();
        PermissionDecision::Allow(id)
    }

    fn park_quiesce(&self) -> Duration {
        Duration::from_millis(1200)
    }

    fn usage(&self, raw: &Value) -> Option<Usage> {
        let get = |k: &str| raw.get(k).and_then(Value::as_u64).unwrap_or(0);
        let cached = get("cachedReadTokens");
        let input = get("inputTokens") + cached;
        let output = get("outputTokens");
        if input == 0 && output == 0 {
            return None;
        }
        Some(Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            extra: json!({"cached_tokens": cached}),
        })
    }

    fn turn_outcome(&self, stage: &Path, model: &str, outcome: Result<(), &str>) -> bool {
        let label = self.pick_of(stage).map(|p| p.label);
        match outcome {
            Ok(()) => {
                if let Some(label) = label {
                    accounts::note_success_for(&label, model);
                }
                false
            }
            Err(detail) => {
                let Some(verdict) = verdict_of(detail) else {
                    return false;
                };
                if matches!(verdict, Verdict::RateLimited { .. }) {
                    usage::note_quota_event(detail);
                }
                if let Some(label) = label {
                    accounts::note_failure_for(&label, model, detail);
                }
                if let Ok(mut picks) = self.picks.lock() {
                    picks.remove(stage);
                }
                true
            }
        }
    }

    fn reusable(&self, stage: &Path, model: &str) -> bool {
        let current = accounts::pick_for(model).ok().flatten().map(|p| p.label);
        let reusable = current == self.pick_of(stage).map(|p| p.label);
        if !reusable && let Ok(mut picks) = self.picks.lock() {
            picks.remove(stage);
        }
        reusable
    }

    fn on_commands(&self, commands: &[SlashCommand]) {
        if commands.is_empty() {
            return;
        }
        if let Some(path) = commands_file()
            && let Ok(bytes) = serde_json::to_vec(commands)
        {
            let _ = store::write_atomic(&path, &bytes);
        }
        sync_completion_file(commands);
    }
}

const OPERATOR_COMPLETIONS: &[(&str, &str)] = &[
    ("commands", "list Antigravity ACP slash commands"),
    ("tools", "show or set upstream harness tool allowlist"),
    ("tools all", "allow every harness tool the host sends"),
    (
        "tools default",
        "bash, toolbox, subagents, file and core tools",
    ),
    ("tools bash", "allow bash only"),
    ("account", "manage Antigravity logins"),
    ("account list", "list Antigravity accounts and health"),
    (
        "account status",
        "show which Antigravity account serves now",
    ),
    ("account usage", "show Antigravity account quota usage"),
    ("account use", "pin an Antigravity account"),
    ("account strategy", "set account selection strategy"),
];

pub fn static_completion_args() -> Vec<String> {
    let mut out: Vec<String> = DEFAULT_COMMANDS
        .iter()
        .map(|(n, _)| (*n).to_string())
        .collect();
    for (arg, _) in OPERATOR_COMPLETIONS {
        if !out.iter().any(|existing| existing == arg) {
            out.push((*arg).to_string());
        }
    }
    out
}

pub fn sync_completion_file(commands: &[SlashCommand]) {
    let Some(home) = gray_core::paths::gray_home() else {
        return;
    };
    let path = home.join("plugins").join("antigravity-sub-completion.json");
    let mut entries: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for cmd in commands {
        let arg = cmd.name.trim_start_matches('/').trim();
        if arg.is_empty() || seen.iter().any(|s| s == arg) {
            continue;
        }
        seen.push(arg.to_string());
        entries.push(json!({"arg": arg, "description": cmd.description}));
    }
    for (arg, desc) in OPERATOR_COMPLETIONS {
        if seen.iter().any(|s| s == arg) {
            continue;
        }
        seen.push((*arg).to_string());
        entries.push(json!({"arg": arg, "description": desc}));
    }
    if let Ok(bytes) = serde_json::to_vec(&json!({"completion": entries})) {
        let _ = store::write_atomic(&path, &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_account_pins_its_own_login() {
        let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::Builder::new()
            .prefix("agy-stage-pin-")
            .tempdir()
            .unwrap();
        let gray_home = root.path().join("gray");
        let account_home = gray_home.join("antigravity-sub/homes/work");
        let cli = account_home.join(".gemini/antigravity-cli");
        std::fs::create_dir_all(&cli).unwrap();
        let token = cli.join("antigravity-oauth-token");
        let payload = r#"{"token":{"access_token":"a"},"id_token_email":"work@example.com"}"#;
        std::fs::write(&token, payload).unwrap();

        let store_dir = gray_home.join("antigravity-sub");
        std::fs::create_dir_all(&store_dir).unwrap();
        let accounts_json = json!({
            "version": 1,
            "strategy": "fill_first",
            "selected": "work",
            "accounts": [{
                "label": "work",
                "home": account_home,
                "email": "work@example.com",
                "org": null,
                "added_at": 1,
                "health": {"state": "ready"},
                "request_count": 0,
                "last_used_at": 0
            }]
        });
        std::fs::write(
            store_dir.join("accounts.json"),
            serde_json::to_string(&accounts_json).unwrap(),
        )
        .unwrap();

        let old_gray_home = std::env::var_os("GRAY_HOME");
        unsafe { std::env::set_var("GRAY_HOME", &gray_home) };

        let stage = tempfile::Builder::new()
            .prefix("agy-stage-")
            .tempdir()
            .unwrap();
        let driver = AgyDriver::default();
        let prep = PreparedTurn {
            system: String::new(),
            input: Vec::new(),
            tail: Vec::new(),
            outputs: Vec::new(),
            tail_prompt: Vec::new(),
            tools: Vec::new(),
            model: "gemini-3.8-flash-low".to_string(),
            effort: None,
            overrides: Vec::new(),
        };
        let stage_res = driver.stage(stage.path(), "http://127.0.0.1:1/mcp", &[], &prep);
        let spec = driver.spawn_spec(stage.path());

        match old_gray_home {
            Some(v) => unsafe { std::env::set_var("GRAY_HOME", v) },
            None => unsafe { std::env::remove_var("GRAY_HOME") },
        }

        stage_res.expect("stage pinned account");

        let staged_cli = stage.path().join(".gemini/antigravity-cli");
        let staged_token = staged_cli.join("antigravity-oauth-token");
        assert!(staged_token.is_file(), "token must be carried into stage");
        assert_eq!(std::fs::read_to_string(&staged_token).unwrap(), payload);
        assert!(
            staged_cli.join("last_check.timestamp").is_file(),
            "stage must stamp last_check.timestamp to suppress auto-updater"
        );

        let env: HashMap<_, _> = spec.env.into_iter().collect();
        assert_eq!(
            env.get(setup::FILE_TOKEN_STORAGE).map(String::as_str),
            Some("1"),
            "must pin agy to the staged token file rather than the OS credential store"
        );
        assert!(
            env.contains_key("DBUS_SESSION_BUS_ADDRESS"),
            "must steer Linux agy away from the session keyring"
        );
        assert_eq!(
            env.get("HOME").map(String::as_str),
            Some(stage.path().to_string_lossy().as_ref())
        );
        if cfg!(windows) {
            assert_eq!(
                env.get("USERPROFILE").map(String::as_str),
                Some(stage.path().to_string_lossy().as_ref()),
                "Windows agy requires USERPROFILE to point at the stage"
            );
        }
    }

    #[test]
    fn link_dir_teardown_preserves_target() {
        let root = tempfile::Builder::new()
            .prefix("agy-link-dir-")
            .tempdir()
            .unwrap();
        let target = root.path().join("target_bin");
        std::fs::create_dir_all(&target).unwrap();
        let sentinel = target.join("helper.txt");
        std::fs::write(&sentinel, b"keep").unwrap();

        let stage = root.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let dst = stage.join("bin");
        link(&target, &dst).expect("link directory");
        assert!(dst.join("helper.txt").is_file());

        std::fs::remove_dir_all(&stage).expect("remove stage");
        assert!(
            sentinel.is_file(),
            "tearing down a staged dir link must never delete target contents"
        );
    }
}
