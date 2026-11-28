use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{manifest, setup, usage};

const COOL_401: u64 = 5 * 60;
const COOL_429: u64 = 60 * 60;
const SOLE_CAP: u64 = 60;
const MAX_RESET: u64 = 8 * 24 * 3600;
const USAGE_DEADLINE: Duration = Duration::from_secs(20);
const GROUPS: [&str; 3] = ["Gemini", "Claude/GPT", "Argon"];

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct Cooldown {
    pub until: u64,
    pub reason: String,
}

#[derive(Serialize, Deserialize)]
pub struct Account {
    pub label: String,
    pub home: PathBuf,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub org: Option<String>,
    #[serde(default)]
    pub added_at: u64,
    #[serde(default)]
    pub health: Health,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cooldowns: BTreeMap<String, Cooldown>,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub last_used_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Health {
    Cooling {
        until: u64,
        reason: String,
    },
    AuthFailed {
        until: u64,
        msg: String,
    },
    SignedOut {
        msg: String,
    },
    #[default]
    #[serde(other)]
    Ready,
}

#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    LeastUsed,
    #[default]
    #[serde(other)]
    FillFirst,
}

impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Strategy::FillFirst => "fill_first",
            Strategy::LeastUsed => "least_used",
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    accounts: Vec<Account>,
    #[serde(default)]
    selected: Option<String>,
    #[serde(default)]
    strategy: Strategy,
}

#[derive(Clone)]
pub struct Pick {
    pub label: String,
    pub home: Option<PathBuf>,
}

pub enum Serving {
    Account(Pick),
    Ambient,
    AllCooling { until: u64, reason: String },
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn state_dir() -> Option<PathBuf> {
    gray_core::paths::gray_home().map(|h| h.join("antigravity-sub"))
}

fn file_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("accounts.json"))
}

fn homes_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join("homes"))
}

pub(crate) fn token_of(home: &Path) -> Option<PathBuf> {
    let file = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");
    file.is_file().then_some(file)
}

fn identity_of(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(token_of(home)?).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    doc.get("id_token_email")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn load() -> Store {
    load_checked().unwrap_or_default()
}

fn spawn_home(home: &Path) -> Option<&Path> {
    (!home.as_os_str().is_empty()).then_some(home)
}

fn ensure_default(store: &mut Store) {
    if store
        .accounts
        .iter()
        .any(|a| a.home.as_os_str().is_empty() || a.label == "default")
    {
        return;
    }
    let has_ambient = crate::agy::credential_file().is_some()
        || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty())
        || cfg!(target_os = "macos")
        || cfg!(windows);
    if !has_ambient {
        return;
    }
    let email = setup::home_dir().and_then(|h| identity_of(&h));
    if let Some(e) = &email
        && store
            .accounts
            .iter()
            .any(|a| a.email.as_deref() == Some(e.as_str()))
    {
        return;
    }
    store.accounts.insert(
        0,
        Account {
            label: "default".to_string(),
            home: PathBuf::new(),
            email,
            org: None,
            added_at: 0,
            health: Health::Ready,
            cooldowns: BTreeMap::new(),
            request_count: 0,
            last_used_at: 0,
        },
    );
}

fn single_group(m: &str) -> &'static str {
    if m.contains("claude") || m.contains("gpt") || m == "opus" || m == "sonnet" {
        "Claude/GPT"
    } else if m.contains("argon") || m.contains("g1-pro") {
        "Argon"
    } else {
        "Gemini"
    }
}

pub fn model_groups(model: &str) -> Vec<&'static str> {
    let m = model.trim().to_ascii_lowercase();
    let m = m.rsplit_once('/').map_or(m.as_str(), |(_, id)| id);
    if m.is_empty() {
        return Vec::new();
    }
    if let Some(rest) = m.strip_prefix("gemini-pro-orchestrator--") {
        let mut parts = rest.split("--");
        let mut out = Vec::new();
        if let Some(p) = parts.next().filter(|s| !s.is_empty()) {
            out.push(single_group(p));
        }
        if let Some(e) = parts.next().filter(|s| !s.is_empty()) {
            let g = single_group(e);
            if !out.contains(&g) {
                out.push(g);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    vec![single_group(m)]
}

fn adopt_legacy(store: &mut Store) {
    let now = now_secs();
    for a in &mut store.accounts {
        if let Health::Cooling { until, reason } = &a.health {
            if *until > now {
                let g = single_group(&reason.to_ascii_lowercase());
                a.cooldowns
                    .entry(g.to_string())
                    .or_insert_with(|| Cooldown {
                        until: *until,
                        reason: reason.clone(),
                    });
            }
            a.health = Health::Ready;
        }
        a.cooldowns.retain(|_, c| c.until > now);
    }
}

fn load_checked() -> Result<Store, String> {
    let Some(path) = file_path() else {
        let mut s = Store::default();
        ensure_default(&mut s);
        return Ok(s);
    };
    let mut store: Store = match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => Store::default(),
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    adopt_legacy(&mut store);
    ensure_default(&mut store);
    Ok(store)
}

fn mutate<T>(f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let path = file_path().ok_or("cannot resolve GRAY_HOME")?;
    let _lock =
        crate::store::lock_file(&path).map_err(|e| format!("lock {}: {e}", path.display()))?;
    let mut store: Store = match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => Store::default(),
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let before = serde_json::to_string(&store).map_err(|e| e.to_string())?;
    adopt_legacy(&mut store);
    ensure_default(&mut store);
    let out = f(&mut store)?;
    if serde_json::to_string(&store).map_err(|e| e.to_string())? != before {
        let text = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
        crate::store::write_atomic(&path, text.as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(out)
}

fn current_auth(a: &Account, now: u64) -> Health {
    match &a.health {
        Health::AuthFailed { until, .. } if *until > now => a.health.clone(),
        Health::SignedOut { .. } => a.health.clone(),
        _ => Health::Ready,
    }
}

fn group_cooldown<'a>(a: &'a Account, group: &str, now: u64) -> Option<&'a Cooldown> {
    a.cooldowns.get(group).filter(|c| c.until > now)
}

fn active_cooldowns(a: &Account, now: u64) -> Vec<(&'static str, &Cooldown)> {
    GROUPS
        .iter()
        .filter_map(|g| group_cooldown(a, g, now).map(|c| (*g, c)))
        .collect()
}

fn current(a: &Account, now: u64) -> Health {
    let auth = current_auth(a, now);
    if !matches!(auth, Health::Ready) {
        return auth;
    }
    let active = active_cooldowns(a, now);
    if active.len() == GROUPS.len()
        && let Some((_, c)) = active.into_iter().min_by_key(|(_, c)| c.until)
    {
        return Health::Cooling {
            until: c.until,
            reason: c.reason.clone(),
        };
    }
    Health::Ready
}

fn until_of(h: &Health) -> u64 {
    match h {
        Health::Ready | Health::SignedOut { .. } => 0,
        Health::Cooling { until, .. } | Health::AuthFailed { until, .. } => *until,
    }
}

fn reason_of(h: &Health) -> Option<String> {
    match h {
        Health::Ready => None,
        Health::Cooling { reason, .. } => Some(reason.clone()),
        Health::AuthFailed { msg, .. } | Health::SignedOut { msg } => Some(msg.clone()),
    }
}

fn last_error_of(a: &Account, now: u64) -> Option<String> {
    if let Some(msg) = reason_of(&current_auth(a, now)) {
        return Some(msg);
    }
    active_cooldowns(a, now)
        .into_iter()
        .min_by_key(|(_, c)| c.until)
        .map(|(g, c)| format!("[{g}] {}", c.reason))
}

fn usable_for(a: &Account, groups: &[&str], now: u64) -> bool {
    if !matches!(current_auth(a, now), Health::Ready) {
        return false;
    }
    if groups.is_empty() {
        GROUPS.iter().any(|g| group_cooldown(a, g, now).is_none())
    } else {
        groups.iter().all(|g| group_cooldown(a, g, now).is_none())
    }
}

fn until_for(a: &Account, groups: &[&str], now: u64) -> u64 {
    if let Health::AuthFailed { until, .. } = current_auth(a, now) {
        return until;
    }
    if groups.is_empty() {
        GROUPS
            .iter()
            .filter_map(|g| group_cooldown(a, g, now).map(|c| c.until))
            .min()
            .unwrap_or(0)
    } else {
        groups
            .iter()
            .filter_map(|g| group_cooldown(a, g, now).map(|c| c.until))
            .max()
            .unwrap_or(0)
    }
}

fn pick_of(a: &Account) -> Pick {
    Pick {
        label: a.label.clone(),
        home: spawn_home(&a.home).map(Path::to_path_buf),
    }
}

fn serve_for(store: &Store, groups: &[&str]) -> Serving {
    if store.accounts.is_empty() {
        return Serving::Ambient;
    }
    let now = now_secs();
    if let Some(sel) = store.selected.as_deref()
        && let Some(a) = store
            .accounts
            .iter()
            .find(|a| a.label == sel && usable_for(a, groups, now))
    {
        return Serving::Account(pick_of(a));
    }
    let next = match store.strategy {
        Strategy::LeastUsed => store
            .accounts
            .iter()
            .filter(|a| usable_for(a, groups, now))
            .min_by_key(|a| a.last_used_at),
        Strategy::FillFirst => store.accounts.iter().find(|a| usable_for(a, groups, now)),
    };
    if let Some(a) = next {
        return Serving::Account(pick_of(a));
    }
    let Some(soon) = store
        .accounts
        .iter()
        .filter(|a| !matches!(a.health, Health::SignedOut { .. }))
        .min_by_key(|a| until_for(a, groups, now))
    else {
        return Serving::AllCooling {
            until: 0,
            reason: format!(
                "all {} antigravity accounts are signed out; add a login again",
                store.accounts.len()
            ),
        };
    };
    let until = until_for(soon, groups, now);
    let mins = until.saturating_sub(now).div_ceil(60).max(1);
    let scope = if groups.is_empty() {
        String::new()
    } else {
        format!(" for {}", groups.join(" + "))
    };
    Serving::AllCooling {
        until,
        reason: format!(
            "all {} antigravity accounts are cooling down{scope}; {} next at {} \u{b7} resets in ~{mins}m",
            store.accounts.len(),
            soon.label,
            usage::iso(until),
        ),
    }
}

fn serve(store: &Store) -> Serving {
    serve_for(store, &[])
}

pub fn serving() -> Serving {
    serve(&load())
}

fn login_problem(home: &Path, want: &str, requires_file: bool) -> Option<String> {
    if token_of(home).is_none() && !requires_file {
        return None;
    }
    match identity_of(home) {
        None => Some(format!("signed out: no login in {}", home.display())),
        Some(got) if !got.eq_ignore_ascii_case(want) => Some(format!(
            "signed out: {} is logged in as {got}, not {want}",
            home.display()
        )),
        Some(_) => None,
    }
}

pub(crate) fn requires_file_login(home: &Path) -> bool {
    !setup::home_dir().is_some_and(|own| {
        own == home
            || own
                .canonicalize()
                .is_ok_and(|own| home.canonicalize().is_ok_and(|home| own == home))
    })
}

fn problem_of(a: &Account) -> Option<String> {
    let home = spawn_home(&a.home)?;
    login_problem(home, a.email.as_deref()?, requires_file_login(home))
}

/// A signed-out account is never picked, so `verify` never sees it again
/// and a fresh `agy` login could not clear the mark. Re-check them (file
/// reads only) before serving.
fn revive_signed_out() {
    let _ = mutate(|store| {
        for a in &mut store.accounts {
            if matches!(a.health, Health::SignedOut { .. }) && problem_of(a).is_none() {
                a.health = Health::Ready;
            }
        }
        Ok(())
    });
}

fn verify(p: &Pick) -> Result<(), String> {
    let store = load();
    let Some(a) = store.accounts.iter().find(|a| a.label == p.label) else {
        return Ok(());
    };
    let problem = problem_of(a);
    let _ = mutate(|store| {
        if let Some(a) = store.accounts.iter_mut().find(|a| a.label == p.label) {
            match (&problem, &a.health) {
                (Some(msg), _) => a.health = Health::SignedOut { msg: msg.clone() },
                (None, Health::SignedOut { .. }) => a.health = Health::Ready,
                _ => {}
            }
        }
        Ok(())
    });
    problem.map_or(Ok(()), Err)
}

pub fn pick() -> Result<Option<Pick>, String> {
    pick_for("")
}

pub fn pick_for(model: &str) -> Result<Option<Pick>, String> {
    let groups = model_groups(model);
    let n = load_checked()?.accounts.len();
    if n == 0 {
        return Ok(None);
    }
    revive_signed_out();
    for _ in 0..=n {
        match serve_for(&load(), &groups) {
            Serving::Account(p) if verify(&p).is_err() => {}
            _ => break,
        }
    }
    mutate(|store| match serve_for(store, &groups) {
        Serving::Account(p) => {
            if let Some(a) = store.accounts.iter_mut().find(|a| a.label == p.label) {
                a.request_count += 1;
                a.last_used_at = now_secs();
            }
            Ok(Some(p))
        }
        Serving::Ambient => Ok(None),
        Serving::AllCooling { reason, .. } => {
            if groups.is_empty() {
                let now = now_secs();
                if let Some(a) = store
                    .accounts
                    .iter()
                    .find(|a| matches!(current_auth(a, now), Health::Ready))
                {
                    return Ok(Some(pick_of(a)));
                }
            }
            Err(reason)
        }
    })
}

pub fn note_success(label: &str) {
    note_success_for(label, "");
}

pub fn note_success_for(label: &str, model: &str) {
    let groups = model_groups(model);
    let _ = mutate(|store| {
        if let Some(a) = store.accounts.iter_mut().find(|a| a.label == label) {
            if !matches!(a.health, Health::Ready) {
                a.health = Health::Ready;
            }
            for g in &groups {
                a.cooldowns.remove(*g);
            }
        }
        Ok(())
    });
}

pub fn note_failure(label: &str, detail: &str) {
    note_failure_for(label, "", detail);
}

pub fn note_failure_for(label: &str, model: &str, detail: &str) {
    let Some(verdict) = crate::agy::verdict_of(detail) else {
        return;
    };
    let groups = model_groups(model);
    let group = groups.first().copied().unwrap_or("Gemini");
    let _ = mutate(|store| {
        let now = now_secs();
        let lone = store.accounts.len() == 1;
        let cap = if lone { SOLE_CAP } else { MAX_RESET };
        let msg: String = detail.chars().take(200).collect();
        if let Some(a) = store.accounts.iter_mut().find(|a| a.label == label) {
            match verdict {
                crate::agy::Verdict::Unauthorized => {
                    let cur = until_of(&current_auth(a, now));
                    let until = cur.max(now + COOL_401.min(cap).max(1));
                    a.health = Health::AuthFailed { until, msg };
                }
                crate::agy::Verdict::RateLimited { retry_after } => {
                    let ttl = match retry_after {
                        Some(d) => d.as_secs().min(MAX_RESET),
                        None => COOL_429,
                    };
                    let cur = group_cooldown(a, group, now).map_or(0, |c| c.until);
                    let until = cur.max(now + ttl.min(cap).max(1));
                    a.cooldowns
                        .insert(group.to_string(), Cooldown { until, reason: msg });
                }
            }
        }
        Ok(())
    });
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(146097);
    let yoe = y - era * 400;
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as i64;
    era * 146097 + doe - 719468
}

fn parse_iso_secs(s: &str) -> Option<u64> {
    let (date, time) = s.trim().split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let time = time.strip_suffix('Z').unwrap_or(time);
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let se: i64 = t.next().unwrap_or("0").split('.').next()?.parse().ok()?;
    let secs = days_from_civil(y, mo, da) * 86400 + h * 3600 + mi * 60 + se;
    (secs > 0).then_some(secs as u64)
}

pub fn sync_usage_windows(home: Option<&Path>, windows: &[Value]) {
    let want = home.filter(|h| !h.as_os_str().is_empty());
    let _ = mutate(|store| {
        let now = now_secs();
        let Some(a) = store
            .accounts
            .iter_mut()
            .find(|a| spawn_home(&a.home) == want)
        else {
            return Ok(());
        };
        for g in GROUPS {
            let mut max_pct: Option<f64> = None;
            let mut exhausted_until: Option<u64> = None;
            for w in windows {
                let Some((_, grp)) = w
                    .get("label")
                    .and_then(Value::as_str)
                    .and_then(|l| l.split_once(" \u{b7} "))
                else {
                    continue;
                };
                if grp != g {
                    continue;
                }
                let Some(pct) = w.get("used_percent").and_then(Value::as_f64) else {
                    continue;
                };
                max_pct = Some(max_pct.map_or(pct, |cur| cur.max(pct)));
                if pct >= 100.0
                    && let Some(r) = w
                        .get("resets_at")
                        .and_then(Value::as_str)
                        .and_then(parse_iso_secs)
                        .filter(|r| *r > now)
                {
                    exhausted_until = Some(exhausted_until.map_or(r, |cur| cur.max(r)));
                }
            }
            if let Some(until) = exhausted_until {
                let entry = a
                    .cooldowns
                    .entry(g.to_string())
                    .or_insert_with(|| Cooldown {
                        until,
                        reason: format!("Antigravity {g} quota exhausted"),
                    });
                if until > entry.until {
                    entry.until = until;
                }
            } else if max_pct.is_some_and(|p| p < 90.0) {
                a.cooldowns.remove(g);
            }
        }
        Ok(())
    });
}

pub fn probe_home() -> Option<PathBuf> {
    let store = load();
    let first = store.accounts.first()?;
    Some(match serve(&store) {
        Serving::Account(p) => p.home,
        _ => pick_of(first).home,
    })
    .flatten()
}

pub fn has_token() -> bool {
    load().accounts.iter().any(|a| token_of(&a.home).is_some())
}

const USAGE: &str = "\
antigravity-sub account — several Antigravity logins behind one provider.
  antigravity-sub account add <label>          run the agy sign-in into a new
                                               account home (needs a terminal)
  antigravity-sub account use <label>          pin an account (--clear unpins)
  antigravity-sub account remove <label>       drop an account and its home
  antigravity-sub account strategy <fill_first|least_used>
  antigravity-sub account list                 accounts and their health
  antigravity-sub account status               which account serves now
  antigravity-sub account usage [<label>]      quota per account
  any verb accepts --json for the machine contract the host /accounts panel reads
The file is <gray home>/antigravity-sub/accounts.json; absent or empty
`accounts` keeps the single ambient `agy` login.";

pub fn command(args: &[String], interactive: bool) -> Result<String, String> {
    let json = args.iter().any(|a| a == "--json");
    let args: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "--json")
        .cloned()
        .collect();
    match args.first().map(String::as_str) {
        Some("add") => cmd_add(&args[1..], interactive, json),
        Some("use") => cmd_use(&args[1..], json),
        Some("remove") | Some("rm") => cmd_remove(&args[1..], json),
        Some("reset") => cmd_reset(&args[1..], json),
        Some("strategy") => cmd_strategy(&args[1..], json),
        Some("status") => cmd_status(json),
        Some("usage") => cmd_usage(&args[1..], json),
        Some("list") => cmd_list(json),
        None => Ok(USAGE.to_string()),
        Some(other) => Err(format!("unknown account verb {other:?}\n\n{USAGE}")),
    }
}

fn cmd_reset(args: &[String], json: bool) -> Result<String, String> {
    let all = args.iter().any(|a| a == "--all") || args.is_empty();
    let want = args.iter().find(|a| !a.starts_with("--")).cloned();
    let cleared = mutate(|store| {
        let mut cleared = Vec::new();
        for a in &mut store.accounts {
            if !all && want.as_deref() != Some(a.label.as_str()) {
                continue;
            }
            let had_cool = !a.cooldowns.is_empty()
                || matches!(a.health, Health::Cooling { .. } | Health::AuthFailed { .. });
            if had_cool {
                a.cooldowns.clear();
                if !matches!(a.health, Health::SignedOut { .. }) {
                    a.health = Health::Ready;
                }
                cleared.push(a.label.clone());
            }
        }
        Ok(cleared)
    })?;
    if json {
        return ok_json(json!({"cleared": cleared}));
    }
    Ok(if cleared.is_empty() {
        "no cached cooldowns to clear".to_string()
    } else {
        format!("cleared {}", cleared.join(", "))
    })
}

fn store_json(store: &Store) -> Result<String, String> {
    let now = now_secs();
    let accounts: Vec<Value> = store
        .accounts
        .iter()
        .map(|a| {
            let health = current(a, now);
            let cooldowns: BTreeMap<&str, &Cooldown> =
                active_cooldowns(a, now).into_iter().collect();
            json!({
                "label": a.label,
                "home": a.home.display().to_string(),
                "email": a.email,
                "org": a.org,
                "added_at": a.added_at,
                "cooldown_until": until_of(&health),
                "cooling": !matches!(health, Health::Ready),
                "cooldowns": cooldowns,
                "last_error": last_error_of(a, now),
                "health": health,
                "request_count": a.request_count,
                "last_used_at": a.last_used_at,
                "pinned": store.selected.as_deref() == Some(a.label.as_str()),
            })
        })
        .collect();
    let serving = match serve(store) {
        Serving::Account(p) => json!({"kind": "account", "label": p.label}),
        Serving::Ambient => json!({"kind": "ambient"}),
        Serving::AllCooling { until, reason } => {
            json!({"kind": "cooldown", "until": until, "reason": reason})
        }
    };
    serde_json::to_string_pretty(&json!({
        "version": 1,
        "plugin": manifest::PLUGIN_NAME,
        "provider": manifest::PROVIDER_ID,
        "strategy": store.strategy.name(),
        "selected": store.selected,
        "ambient": store.accounts.is_empty(),
        "accounts": accounts,
        "serving": serving,
    }))
    .map_err(|e| e.to_string())
}

fn ok_json(extra: Value) -> Result<String, String> {
    let mut v = extra;
    v["ok"] = json!(true);
    serde_json::to_string(&v).map_err(|e| e.to_string())
}

fn valid_label(label: &str) -> Result<(), String> {
    let ok = !label.is_empty()
        && label.len() <= 40
        && label
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(())
        .ok_or_else(|| format!("bad label {label:?}: 1-40 chars of [A-Za-z0-9_-]"))
}

enum Insert {
    Added { who: String },
    Duplicate { label: String, identity: String },
}

fn admit(label: &str, home: &Path) -> Result<Insert, String> {
    if token_of(home).is_none() {
        return Err(format!(
            "no login in {} — the agy sign-in did not complete",
            home.display()
        ));
    }
    let email = identity_of(home);
    let who = email
        .clone()
        .unwrap_or_else(|| "identity pending".to_string());
    mutate(|store| {
        if store.accounts.iter().any(|a| a.label == label) {
            return Err(format!("account {label:?} already exists"));
        }
        if let Some(e) = &email
            && let Some(dup) = store
                .accounts
                .iter()
                .find(|a| a.email.as_deref() == Some(e.as_str()))
        {
            return Ok(Insert::Duplicate {
                label: dup.label.clone(),
                identity: e.clone(),
            });
        }
        store.accounts.push(Account {
            label: label.to_string(),
            home: home.to_path_buf(),
            email: email.clone(),
            org: None,
            added_at: now_secs(),
            health: Health::Ready,
            cooldowns: BTreeMap::new(),
            request_count: 0,
            last_used_at: 0,
        });
        Ok(Insert::Added { who: who.clone() })
    })
}

fn cmd_add(args: &[String], interactive: bool, json: bool) -> Result<String, String> {
    let Some(label) = args.first() else {
        return Err(format!("account add needs a label\n\n{USAGE}"));
    };
    if args.len() > 1 {
        return Err(USAGE.to_string());
    }
    valid_label(label)?;
    if load().accounts.iter().any(|a| a.label == *label) {
        return Err(format!("account {label:?} already exists"));
    }
    let binary = setup::resolve_command().ok_or(setup::INSTALL_HINT)?;
    if !interactive {
        return Err(format!(
            "account add runs the agy sign-in; use `gray antigravity-sub account add {label}` in a terminal"
        ));
    }
    let dest = homes_dir()
        .ok_or("cannot resolve GRAY_HOME")?
        .join(label.as_str());
    let root = dest.parent().ok_or("cannot resolve GRAY_HOME")?;
    std::fs::create_dir_all(root).map_err(|e| format!("create {}: {e}", root.display()))?;
    std::fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o700));
    }
    eprintln!("signing {label:?} in under {} …", dest.display());
    let mut cmd = std::process::Command::new(&binary);
    cmd.args(["-p", "/usage"])
        .envs(setup::child_env())
        .envs(setup::staged_home_env(&dest))
        // Pin agy to write/read the token file in `dest` rather than reusing
        // the ambient OS credential store (Windows Credential Manager / macOS
        // Keychain / Linux keyring), which would exit 0 with no token file.
        .envs([
            setup::keyring_off(&dest),
            (setup::FILE_TOKEN_STORAGE.to_string(), "1".to_string()),
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    let shim = {
        let real_home = std::env::var("HOME").unwrap_or_default();
        let shim = dest.join("xdg-open");
        if !real_home.is_empty() {
            let quote = |v: &str| format!("'{}'", v.replace('\'', "'\''"));
            let bus = match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
                Ok(v) if !v.is_empty() => {
                    format!("export DBUS_SESSION_BUS_ADDRESS={}\n", quote(&v))
                }
                _ => "unset DBUS_SESSION_BUS_ADDRESS\n".to_string(),
            };
            let script = format!(
                "#!/bin/sh\n{bus}HOME={} exec /usr/bin/xdg-open \"$@\" >/dev/null 2>&1 &\n",
                quote(&real_home)
            );
            if std::fs::write(&shim, script).is_ok() {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755));
            }
        }
        let path_env = format!(
            "{}:{}",
            dest.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.env("PATH", path_env).env("BROWSER", &shim);
        if std::env::var_os("XAUTHORITY").is_none() && !real_home.is_empty() {
            let xauth = Path::new(&real_home).join(".Xauthority");
            if xauth.is_file() {
                cmd.env("XAUTHORITY", xauth);
            }
        }
        shim
    };
    let placed = cmd
        .status()
        .map_err(|e| format!("spawn agy: {e}"))
        .and_then(|s| {
            if s.success() || token_of(&dest).is_some() {
                Ok(())
            } else {
                Err(format!("agy sign-in failed (exited {s})"))
            }
        });
    #[cfg(unix)]
    let _ = std::fs::remove_file(&shim);
    if let Err(e) = placed {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(e);
    }
    match admit(label, &dest) {
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(e)
        }
        Ok(Insert::Duplicate { label, identity }) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(format!(
                "that login is already registered as {label:?} ({identity})"
            ))
        }
        Ok(Insert::Added { who }) => {
            if json {
                return ok_json(json!({
                    "message": format!("account {label:?} added"),
                    "label": label,
                    "email": who,
                }));
            }
            Ok(format!(
                "account {label:?} added ({who}); home {}",
                dest.display()
            ))
        }
    }
}

fn cmd_use(args: &[String], json: bool) -> Result<String, String> {
    match args.first().map(String::as_str) {
        None => {
            let store = load();
            let sel = store.selected.as_deref().unwrap_or("(none)");
            if json {
                return ok_json(json!({"selected": store.selected}));
            }
            Ok(format!("pinned account: {sel}"))
        }
        Some("--clear") | Some("none") | Some("auto") => {
            mutate(|store| {
                store.selected = None;
                Ok(())
            })?;
            if json {
                return ok_json(json!({"selected": null}));
            }
            Ok("pin cleared — selection follows the strategy".to_string())
        }
        Some(label) => {
            mutate(|store| {
                if !store.accounts.iter().any(|a| a.label == label) {
                    return Err(format!("no account {label:?}"));
                }
                store.selected = Some(label.to_string());
                Ok(())
            })?;
            if json {
                return ok_json(json!({"selected": label}));
            }
            Ok(format!("serving account: {label}"))
        }
    }
}

fn cmd_strategy(args: &[String], json: bool) -> Result<String, String> {
    let next = match args.first().map(String::as_str) {
        None => {
            let cur = load().strategy.name();
            if json {
                return ok_json(json!({"strategy": cur}));
            }
            return Ok(format!("strategy: {cur}"));
        }
        Some("fill_first") => Strategy::FillFirst,
        Some("least_used") => Strategy::LeastUsed,
        Some(other) => {
            return Err(format!(
                "unknown strategy {other:?} — fill_first or least_used"
            ));
        }
    };
    let set = mutate(|store| {
        store.strategy = next;
        Ok(store.strategy.name())
    })?;
    if json {
        return ok_json(json!({"strategy": set}));
    }
    Ok(format!("strategy: {set}"))
}

fn cmd_remove(args: &[String], json: bool) -> Result<String, String> {
    let Some(label) = args.first() else {
        return Err(format!("account remove needs a label\n\n{USAGE}"));
    };
    if args.len() > 1 {
        return Err(USAGE.to_string());
    }
    let acc = mutate(|store| {
        let Some(i) = store.accounts.iter().position(|a| a.label == *label) else {
            return Err(format!("no account {label:?}"));
        };
        let acc = store.accounts.remove(i);
        if store.selected.as_deref() == Some(label.as_str()) {
            store.selected = None;
        }
        Ok(acc)
    })?;
    let mut msg = format!("account {label:?} removed");
    if let Some(dir) = homes_dir()
        && acc.home.starts_with(&dir)
        && acc.home.is_dir()
    {
        std::fs::remove_dir_all(&acc.home)
            .map_err(|e| format!("removed {label:?} but not {}: {e}", acc.home.display()))?;
        msg.push_str(&format!(" and its login home {}", acc.home.display()));
    }
    if json {
        return ok_json(json!({"message": msg}));
    }
    Ok(msg)
}

fn account_line(a: &Account, now: u64, selected: &Option<String>) -> String {
    let mark = if selected.as_deref() == Some(a.label.as_str()) {
        '*'
    } else {
        ' '
    };
    let state = match current(a, now) {
        Health::Ready => {
            let active = active_cooldowns(a, now);
            if active.is_empty() {
                "ok".to_string()
            } else {
                let groups: Vec<&str> = active.iter().map(|(g, _)| *g).collect();
                let until = active.iter().map(|(_, c)| c.until).min().unwrap_or(0);
                format!(
                    "ok ({} cooled until {})",
                    groups.join(", "),
                    usage::iso(until)
                )
            }
        }
        Health::Cooling { until, .. } => format!("cooled until {}", usage::iso(until)),
        Health::AuthFailed { until, .. } => format!("auth failed until {}", usage::iso(until)),
        Health::SignedOut { .. } => "signed out".to_string(),
    };
    let who = a.email.clone().unwrap_or_else(|| "-".to_string());
    format!(
        "{mark} {:<16} {:<26} {:<30} {}",
        a.label,
        state,
        who,
        a.home.display()
    )
}

fn cmd_list(json: bool) -> Result<String, String> {
    revive_signed_out();
    let store = load_checked()?;
    if json {
        return store_json(&store);
    }
    if store.accounts.is_empty() {
        return Ok(
            "no accounts configured — turns use the ambient `agy` login\nadd one with `gray antigravity-sub account add <label>`"
                .to_string(),
        );
    }
    let now = now_secs();
    let mut out = format!("{} account(s):\n", store.accounts.len());
    for a in &store.accounts {
        out.push_str(&account_line(a, now, &store.selected));
        out.push('\n');
    }
    out.push_str("`use` pins *; fill_first order is file order.");
    Ok(out)
}

fn cmd_status(json: bool) -> Result<String, String> {
    let store = load_checked()?;
    if json {
        return store_json(&store);
    }
    if store.accounts.is_empty() {
        return Ok("no accounts configured — serving the ambient `agy` login".to_string());
    }
    let now = now_secs();
    let mut out = match serve(&store) {
        Serving::Account(p) => format!(
            "serving account: {} (home {})\n",
            p.label,
            p.home
                .as_deref()
                .map_or("(ambient login)".to_string(), |h| h.display().to_string())
        ),
        Serving::Ambient => "serving account: (none)".to_string(),
        Serving::AllCooling { reason, .. } => format!("serving account: {reason}\n"),
    };
    out.push_str(&format!("strategy: {}\n", store.strategy.name()));
    for a in &store.accounts {
        out.push_str(&account_line(a, now, &store.selected));
        if let Some(e) = last_error_of(a, now) {
            out.push_str(&format!("  last error: {e}"));
        }
        out.push('\n');
    }
    Ok(out)
}

type Targets = Vec<(Option<String>, Option<PathBuf>)>;

fn usage_targets(store: &Store, want: Option<&str>) -> Result<Targets, String> {
    if store.accounts.is_empty() {
        return match want {
            None => Ok(vec![(None, None)]),
            Some(w) => Err(format!(
                "no account labelled {w:?}: no pool configured, the ambient login has no label"
            )),
        };
    }
    let picked: Targets = store
        .accounts
        .iter()
        .filter(|a| want.is_none_or(|w| a.label == w))
        .map(|a| {
            (
                Some(a.label.clone()),
                spawn_home(&a.home).map(Path::to_path_buf),
            )
        })
        .collect();
    match want {
        Some(w) if picked.is_empty() => Err(format!("no account labelled {w:?}")),
        _ => Ok(picked),
    }
}

fn usage_text(entry: &Value) -> String {
    let mut out = match entry.get("label").and_then(Value::as_str) {
        Some(l) => format!("{l}\n"),
        None => "ambient\n".to_string(),
    };
    if let Some(e) = entry.get("error").and_then(Value::as_str) {
        out.push_str(&format!("  error: {e}\n"));
    }
    let windows = entry.pointer("/usage/windows").and_then(Value::as_array);
    for w in windows.into_iter().flatten() {
        let label = w.get("label").and_then(Value::as_str).unwrap_or("?");
        let pct = w
            .get("used_percent")
            .and_then(Value::as_f64)
            .map_or("-".to_string(), |p| format!("{p:.0}%"));
        let reset = w.get("resets_at").and_then(Value::as_str).unwrap_or("-");
        out.push_str(&format!("  {label:<26} {pct:>5}  resets {reset}\n"));
    }
    if let Some(note) = entry.pointer("/usage/note").and_then(Value::as_str) {
        out.push_str(&format!("  note: {note}\n"));
    }
    out.trim_end().to_string()
}

fn cmd_usage(args: &[String], json: bool) -> Result<String, String> {
    let want = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(String::as_str);
    let targets = usage_targets(&load_checked()?, want)?;
    let mut entries: Vec<Value> = targets
        .iter()
        .map(|(label, _)| json!({"label": label, "usage": null, "error": null}))
        .collect();
    let (tx, rx) = std::sync::mpsc::channel();
    for (i, (_, home)) in targets.into_iter().enumerate() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let card = usage::account_card(home.as_deref());
            let _ = tx.send((i, card));
        });
    }
    drop(tx);
    let deadline = Instant::now() + USAGE_DEADLINE;
    while let Ok((i, card)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        entries[i]["usage"] = card.clone().unwrap_or(Value::Null);
        entries[i]["error"] = card.err().map(Value::String).unwrap_or(Value::Null);
    }
    for e in &mut entries {
        if e["usage"].is_null() && e["error"].is_null() {
            e["error"] = json!(format!(
                "no usage report within {}s",
                USAGE_DEADLINE.as_secs()
            ));
        }
    }
    if json {
        return serde_json::to_string_pretty(&json!({
            "version": 1,
            "plugin": manifest::PLUGIN_NAME,
            "provider": manifest::PROVIDER_ID,
            "accounts": entries,
        }))
        .map_err(|e| e.to_string());
    }
    Ok(entries
        .iter()
        .map(usage_text)
        .collect::<Vec<_>>()
        .join("\n\n"))
}

#[cfg(test)]
mod login_tests {
    use super::login_problem;

    fn home_with_token(email: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join(".gemini").join("antigravity-cli");
        std::fs::create_dir_all(&cli).unwrap();
        let doc = match email {
            Some(e) => serde_json::json!({ "id_token_email": e }),
            None => serde_json::json!({}),
        };
        std::fs::write(cli.join("antigravity-oauth-token"), doc.to_string()).unwrap();
        dir
    }

    #[test]
    fn own_home_without_token_file_is_not_signed_out() {
        // The Windows build keeps the login in Credential Manager: no file.
        let home = tempfile::tempdir().unwrap();
        assert_eq!(login_problem(home.path(), "a@x.com", false), None);
    }

    #[test]
    fn staged_home_without_token_file_is_signed_out() {
        let home = tempfile::tempdir().unwrap();
        let msg = login_problem(home.path(), "a@x.com", true).unwrap();
        assert!(msg.starts_with("signed out: no login in"), "{msg}");
    }

    #[test]
    fn token_identity_is_checked_in_any_home() {
        for staged in [false, true] {
            let home = home_with_token(Some("A@x.com"));
            assert_eq!(login_problem(home.path(), "a@x.com", staged), None);
            let msg = login_problem(home.path(), "b@x.com", staged).unwrap();
            assert!(msg.contains("logged in as A@x.com, not b@x.com"), "{msg}");
        }
    }
}
