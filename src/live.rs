//! A pooled `agy` child per conversation: one persistent process answers
//! consecutive host turns when each request's history is a strict
//! continuation of the one the session last answered.
//!
//! Verified `agy` 1.3.1 stream-json behaviour this relies on:
//! * `agy -p --input-format stream-json --output-format stream-json` reads
//!   one NDJSON `{"event":"user","message":{"content":…}}` per stdin line
//!   and runs a turn for each, staying alive between them (stdin EOF exits
//!   it cleanly). Conversation state persists between lines: turns share
//!   one conversation_id and later turns recall earlier content — so a
//!   continuation prompts with only the delta, the transcript prefix is
//!   already upstream.
//! * one `init` line fires at process start (first turn only); each turn's
//!   terminal line is `{"event":"result",…}` — success or failure.
//! * a failed turn exits the process right after its `result` line (a
//!   quota wall, an undecodable input line, an empty prompt all end the
//!   child) — a result is a settle, an EOF after it is just that death.
//!
//! Failure taxonomy:
//! * a SUCCESS result folds into SSE; the turn's `input` is absorbed with
//!   the emitted reply (call ids + answer text) so the next request's
//!   strict-continuation check recognizes its echo.
//! * a settled failure (a non-SUCCESS result, or a result whose reply
//!   can't fold — no `finish` call, a foreign tool) records the input as
//!   absorbed with NO reply on record — the host's identical retry then
//!   misses (`prefix`: nothing extends it) and respawns, exactly like the
//!   spawn-per-turn days, while a later turn of the same conversation
//!   continues the warm session with only its new tail. The session keeps
//!   only if the child actually survived the error (it usually doesn't).
//! * dead wire — stdin write fails, stdout EOFs or garbles mid-turn, or
//!   the deadline expires with the prompt possibly in flight — leaves
//!   upstream state unknown: kill + drop, never pool a session the next
//!   turn can't trust. ONE exception: a stdin write failure on a pooled
//!   continuation means the delta never landed (a partial line is invalid
//!   NDJSON, which `agy` rejects then exits on), so the turn falls back to
//!   a fresh child + the full transcript — still answerable.
//!
//! Pool rules: at most `MAX_LIVE` idle children; a session leaves the pool
//! for its turn (exclusive use) and returns if the wire stayed
//! clean. Sessions idle past `IDLE_TTL` or whose child exited are reaped
//! on every pool access and by a background sweep; [`shutdown_all`]
//! closes everything on `plugin/shutdown` or host-stdin EOF.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Once, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::chat::{self, PreparedTurn, TurnIsolation, Usage};
use crate::{session, setup};

/// Pool bound: at most this many idle `agy` children.
const MAX_LIVE: usize = 4;
/// A pooled session idle this long (no real turn) is closed.
const IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// Reaper sweep period.
const REAPER_SWEEP: Duration = Duration::from_secs(60);
/// Grace for a child to exit after stdin closes before `close` kills it.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// After a settled-but-failed result the child usually exits: this grace
/// lets the wire prove death (EOF) before the session is called alive.
const ERROR_GRACE: Duration = Duration::from_millis(800);
/// A re-warmed child must announce its `init` line within this window —
/// a silent resume (a refused `--conversation`, a dead spawn) is dropped
/// un-pooled and its record is left for the sweep.
const REWARM_INIT_WAIT: Duration = Duration::from_secs(30);
/// Records fresher than this re-pool at startup. claude-sub bounds its
/// window by the provider's ~1h cache TTL (6h); Gemini's implicit cache
/// lives ~minutes and the reaper closes a session idle past `IDLE_TTL`
/// anyway, so the binding limit here is the pool's own idle lifetime —
/// an older record would only spawn a child the reaper closes on arrival.
const REWARM_WINDOW: Duration = IDLE_TTL;

/// One stdout line, parsed: `Err` is a garbage line (dead wire).
type PumpMsg = Result<Value, String>;

/// One live `agy` child plus the replay state needed to recognize and
/// answer its strict-continuation turns.
pub struct LiveSession {
    child: Child,
    /// `Option` so `close` can drop it: stdin EOF is the clean exit signal.
    stdin: Option<ChildStdin>,
    /// Parsed stream-json lines off the stdout pump thread.
    rx: Receiver<PumpMsg>,
    /// Lines pulled while checking post-error liveness; drained before
    /// the channel on the next turn.
    pending: VecDeque<PumpMsg>,
    pid: u32,
    /// From the `init` line (first turn): observability only — matching
    /// keys off the transcript, never this id.
    conversation_id: String,
    /// Route + funnel text the session runs under: a reused session only
    /// matches a request resolving to the same model and system text.
    native_model: String,
    system: String,
    /// The exact `input` array of the last request this session absorbed.
    absorbed: Vec<Value>,
    /// Call ids emitted in that answer, in order.
    reply_call_ids: Vec<String>,
    /// The (trimmed) answer text emitted in that answer.
    reply_text: String,
    /// User-activity clock for the `IDLE_TTL` reaper: bumped by `absorb`/
    /// `absorb_failed` only.
    last_used: Instant,
    /// Wire went dead or we killed the child: never re-pool.
    dead: bool,
    /// Re-pooled from a persisted record after a plugin restart — its
    /// first real turn is the resume's proof: a dead wire there means a
    /// stale record, so the turn falls back to a fresh child instead of
    /// failing.
    rewarmed: bool,
    account: Option<PathBuf>,
    /// Keeps the staged HOME alive for the child's whole life.
    _isolation: TurnIsolation,
}

#[derive(Default)]
struct Pool {
    /// Idle sessions only — a turn takes its session out for exclusive
    /// use and returns it via [`give_back`].
    live: Vec<Arc<Mutex<LiveSession>>>,
}

static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
/// Set by `shutdown_all`: sessions out of the pool close instead of
/// returning.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn locked_pool() -> MutexGuard<'static, Pool> {
    let pool = POOL.get_or_init(|| Mutex::new(Pool::default()));
    // Tiny reaper sweeper: started on first pool access.
    static START: Once = Once::new();
    START.call_once(|| {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(REAPER_SWEEP);
                let dead = {
                    let Some(pool) = POOL.get() else { continue };
                    let mut pool = pool.lock().unwrap_or_else(|e| e.into_inner());
                    reap(&mut pool.live)
                };
                for s in dead {
                    s.lock().unwrap_or_else(|e| e.into_inner()).close();
                }
            }
        });
    });
    pool.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock(s: &Arc<Mutex<LiveSession>>) -> MutexGuard<'_, LiveSession> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// Pull dead/idle-out sessions out of a locked pool; the caller closes
/// them AFTER releasing the pool lock (`close` waits on the child).
fn reap(live: &mut Vec<Arc<Mutex<LiveSession>>>) -> Vec<Arc<Mutex<LiveSession>>> {
    let mut dead = Vec::new();
    let mut i = 0;
    while i < live.len() {
        // Pooled sessions are idle, so the try_lock always wins; a lost
        // race means someone else holds it — leave it be. A poisoned
        // lock means a panic mid-turn: the absorb state can't be trusted,
        // reap it too.
        let stale = match live[i].try_lock() {
            Ok(mut s) => {
                s.dead
                    || s.last_used.elapsed() > IDLE_TTL
                    || !matches!(s.child.try_wait(), Ok(None))
            }
            Err(std::sync::TryLockError::Poisoned(_)) => true,
            Err(std::sync::TryLockError::WouldBlock) => false,
        };
        if stale {
            dead.push(live.remove(i));
        } else {
            i += 1;
        }
    }
    dead
}

/// The first pooled session this turn is a strict continuation of (same
/// native model, same system text, `continuation` yields a delta): its
/// index plus the delta. On a miss, the deepest reason any candidate
/// reached (`no_session` when the pool was empty).
fn find(
    live: &[Arc<Mutex<LiveSession>>],
    turn: &PreparedTurn,
    account: Option<&Path>,
) -> Result<(usize, String), &'static str> {
    let mut reason = "no_session";
    let mut depth = 0;
    for (i, arc) in live.iter().enumerate() {
        let Ok(s) = arc.try_lock() else {
            if depth == 0 {
                reason = "locked";
            }
            continue;
        };
        let (miss, d) = if s.account.as_deref() != account {
            ("account", 0)
        } else if s.native_model != turn.native_model {
            ("model", 1)
        } else if s.system != turn.system {
            ("system", 2)
        } else {
            match chat::continuation(&s.absorbed, &s.reply_call_ids, &s.reply_text, &turn.input) {
                Ok(delta) => return Ok((i, delta)),
                Err(m) => (
                    m,
                    match m {
                        "prefix" => 3,
                        "echo" => 4,
                        _ => 5,
                    },
                ),
            }
        };
        if d > depth {
            depth = d;
            reason = miss;
        }
    }
    Err(reason)
}

type Taken = Option<(Arc<Mutex<LiveSession>>, String)>;

/// Take the pooled session this turn continues (see [`find`]), plus the
/// delta to prompt with. On a miss the second item is the reason.
fn take(turn: &PreparedTurn, account: Option<&Path>) -> (Taken, &'static str) {
    let mut dead = Vec::new();
    let result = {
        let mut pool = locked_pool();
        dead.extend(reap(&mut pool.live));
        match find(&pool.live, turn, account) {
            Ok((i, delta)) => (Some((pool.live.remove(i), delta)), "reuse"),
            Err(reason) => (None, reason),
        }
    };
    for s in dead {
        lock(&s).close();
    }
    result
}

/// Return a session to the pool after a turn, evicting the
/// least-recently-used session past `MAX_LIVE`. A `dead` session (or a
/// post-shutdown return) is closed instead of pooled.
fn give_back(arc: Arc<Mutex<LiveSession>>) {
    {
        let mut s = lock(&arc);
        if s.dead || SHUTDOWN.load(Ordering::SeqCst) {
            s.close();
            return;
        }
    }
    let mut dead = {
        let mut pool = locked_pool();
        let mut dead = reap(&mut pool.live);
        pool.live.push(arc);
        if pool.live.len() > MAX_LIVE {
            // Evict the LRU *idle* session — a locked entry (shouldn't
            // exist in `live`, but never block on it) isn't a victim.
            let lru = pool
                .live
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.try_lock().ok().map(|g| (i, g.last_used)))
                .min_by_key(|(_, used)| *used)
                .map(|(i, _)| i);
            if let Some(lru) = lru {
                dead.push(pool.live.remove(lru));
            }
        }
        dead
    };
    for s in dead.drain(..) {
        lock(&s).close();
    }
}

/// Close every pooled session. Called on `plugin/shutdown` and when the
/// host's stdin goes away. Sessions out on a turn close on their return
/// path (`SHUTDOWN`); a surviving `agy` without its parent
/// still exits on stdin EOF.
pub fn shutdown_all() {
    SHUTDOWN.store(true, Ordering::SeqCst);
    let dead = {
        let mut pool = locked_pool();
        std::mem::take(&mut pool.live)
    };
    for s in dead {
        lock(&s).close();
    }
}

/// Spawn an `agy` child under a staged HOME (one staging per session
/// now, not per turn) with a stdout pump feeding parsed stream-json
/// lines. The argv is the old one-shot contract — `--json-schema`
/// forcing the `{answer, calls[]}` envelope — minus the stdin close:
/// this child answers every turn of the conversation. `resume` reopens
/// `agy`'s persisted conversation (`--conversation <sid>`) instead of
/// starting a fresh one — [`rewarm`] passes it, turns never do.
fn spawn(
    turn: &PreparedTurn,
    resume: Option<&str>,
    account: Option<&Path>,
) -> Result<Arc<Mutex<LiveSession>>, String> {
    if let Some(key) = setup::conflicting_env() {
        return Err(format!(
            "subscription provider refuses conflicting {key}: unset it so native uses your Antigravity login"
        ));
    }
    let binary = setup::resolve_command().ok_or_else(|| setup::INSTALL_HINT.to_string())?;
    let schema_str =
        serde_json::to_string(&turn.schema).map_err(|e| format!("schema encode: {e}"))?;
    let isolation = TurnIsolation::stage(account)?;
    let clamp_tier = |family: &str, tier: &str| -> &'static str {
        match (family, tier) {
            ("gemini-4-neon" | "gemini-4-hydrogen", "xhigh") => "xhigh",
            (_, "xhigh" | "high") => "high",
            (_, "low") => "low",
            _ => "medium",
        }
    };
    let mut wire_model = turn.native_model.as_str();
    let mut orch_settings: Option<String> = None;
    let parts: Vec<&str> = turn.native_model.split("--").collect();
    if parts.len() == 4 && parts[0] == "gemini-pro-orchestrator" {
        let (p_id, e_id, tier) = (parts[1], parts[2], parts[3]);
        wire_model = if p_id == "gemini-4-argon" || p_id == "g1-pro" {
            "gemini-pro-orchestrator-g1-pro"
        } else {
            "gemini-pro-orchestrator-neon"
        };
        let eff = turn.effort.as_deref().unwrap_or(tier);
        let p_tier = clamp_tier(p_id, eff);
        let e_tier = clamp_tier(e_id, eff);
        orch_settings = Some(
            json!({
                "orchestrator": {
                    "planner": format!("{p_id}-{p_tier}"),
                    "executor": format!("{e_id}-{e_tier}"),
                }
            })
            .to_string(),
        );
    } else if wire_model.starts_with("gemini-pro-orchestrator-")
        && let Some(eff) = turn.effort.as_deref()
    {
        let p_id = if wire_model.ends_with("g1-pro") || wire_model.ends_with("argon") {
            "gemini-4-argon"
        } else {
            "gemini-4-neon"
        };
        let p_tier = clamp_tier(p_id, eff);
        let e_tier = clamp_tier("gemini-4-hydrogen", eff);
        orch_settings = Some(
            json!({
                "orchestrator": {
                    "planner": format!("{p_id}-{p_tier}"),
                    "executor": format!("gemini-4-hydrogen-{e_tier}"),
                }
            })
            .to_string(),
        );
    }
    let mut args: Vec<&str> = vec![
        "--model",
        wire_model,
        "--disable-slash-commands",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--json-schema",
        schema_str.as_str(),
    ];
    if let Some(settings) = orch_settings.as_deref() {
        args.extend(["--settings", settings]);
    }
    if let Some(sid) = resume {
        args.extend(["--conversation", sid]);
    }
    args.extend(["-p", ""]);
    let mut child = Command::new(&binary)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(setup::child_env())
        // Staged HOME: token symlink (auth) + skeleton settings.json
        // (toolPermission request-review). Cwd stays the real one so the
        // prompt-cache prefix is stable across turns.
        .env("HOME", &isolation.home)
        .envs(setup::account_env(&isolation.home))
        .env("XDG_CONFIG_HOME", isolation.home.join(".config"))
        .env("XDG_DATA_HOME", isolation.home.join(".local/share"))
        .env("XDG_CACHE_HOME", isolation.home.join(".cache"))
        // A turn must never pop a browser: `agy` calls `xdg-open`
        // directly (ignores `BROWSER`), and `xdg-open` with no display
        // and `BROWSER=/bin/true` exits without touching Firefox.
        .env("BROWSER", "/bin/true")
        .env("DISPLAY", "")
        .env("WAYLAND_DISPLAY", "")
        .current_dir(&isolation.cwd)
        .spawn()
        .map_err(|_| setup::INSTALL_HINT.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "native stdout unavailable".to_string())?;
    let (tx, rx) = std::sync::mpsc::channel::<PumpMsg>();
    std::thread::spawn(move || pump(stdout, tx));
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "native stdin unavailable".to_string())?;
    Ok(Arc::new(Mutex::new(LiveSession {
        pid: child.id(),
        child,
        stdin: Some(stdin),
        rx,
        pending: VecDeque::new(),
        conversation_id: String::new(),
        native_model: turn.native_model.clone(),
        system: turn.system.clone(),
        absorbed: Vec::new(),
        reply_call_ids: Vec::new(),
        reply_text: String::new(),
        last_used: Instant::now(),
        dead: false,
        rewarmed: false,
        account: account.map(Path::to_path_buf),
        _isolation: isolation,
    })))
}

/// Re-pool one persisted session after a plugin restart: spawn `agy`
/// `--conversation`-resumed at the record's point but leave it idle —
/// the transcript already holds the conversation, so nothing is written
/// to stdin. The strict-continuation fields are restored from the record
/// so the next turn matches it like a still-pooled session. `None` when
/// the resume can't mount — the record is simply never pooled.
fn spawn_idle(rec: &session::Record) -> Option<Arc<Mutex<LiveSession>>> {
    if !session::session_exists(&rec.sid) {
        return None;
    }
    let turn = PreparedTurn {
        system: rec.system.clone(),
        content_line: String::new(),
        input: Vec::new(),
        schema: chat::finish_schema(),
        names: Vec::new(),
        native_model: rec.model.clone(),
        effort: None,
    };
    let account = (!rec.home.is_empty()).then(|| Path::new(&rec.home));
    let arc = spawn(&turn, Some(&rec.sid), account).ok()?;
    let mut s = lock(&arc);
    // A mounted child announces `init` then idles on stdin; a refused
    // resume or a dead spawn surfaces as an early line/EOF — nothing
    // pooled, nothing poisoned. The announced conversation must be the
    // record's own.
    let mounted = matches!(
        s.rx.recv_timeout(REWARM_INIT_WAIT),
        Ok(Ok(ref v))
            if v.get("event").or_else(|| v.get("type")).and_then(Value::as_str) == Some("init")
                && chat::conversation_of(v) == rec.sid
    );
    if !mounted {
        s.close();
        return None;
    }
    s.absorbed = rec.absorbed.clone();
    s.reply_call_ids = rec.reply_call_ids.clone();
    s.reply_text = rec.reply_text.clone();
    s.conversation_id = rec.sid.clone();
    s.rewarmed = true;
    // Restore the idle clock from the record's age: the TTL resumes
    // where it left off.
    let age = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(rec.saved_at);
    let ago = Instant::now().checked_sub(Duration::from_secs(age));
    s.last_used = ago.unwrap_or_else(Instant::now);
    drop(s);
    Some(arc)
}

/// Re-pool sessions whose records are still inside the warm window. A
/// plugin restart used to strand them: the pool died with the process
/// and each conversation's upstream cache entry lapsed before its next
/// turn, so a restart re-billed every warm transcript. Spawned idle
/// here, the resumed children rejoin the pool — and their
/// next turn still matches as a strict continuation instead of replaying
/// history cold. Runs once at startup on a background thread; spawn
/// failures stay silent — the request just falls back to a cold spawn.
pub fn rewarm() {
    let mut warmed = Vec::new();
    for (rec, age) in session::records() {
        if warmed.len() >= MAX_LIVE || age > REWARM_WINDOW {
            break;
        }
        // A legacy bare-sid point carries no continuation state (a real
        // record always has the funnel's non-empty system text).
        if rec.system.is_empty() {
            continue;
        }
        if let Some(arc) = spawn_idle(&rec) {
            warmed.push(arc);
        }
    }
    if !warmed.is_empty() {
        locked_pool().live.extend(warmed);
    }
}

/// The stdout pump: one parsed stream-json value per line, a `PumpMsg`
/// error for garbage, and a closed channel at EOF.
fn pump(stdout: ChildStdout, tx: Sender<PumpMsg>) {
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let msg = match serde_json::from_str::<Value>(line) {
                    Ok(v) => Ok(v),
                    Err(_) => Err(format!(
                        "invalid native stream-json output: {:?}",
                        line.chars().take(300).collect::<String>()
                    )),
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = tx.send(Err(format!("native stdout: {e}")));
                return;
            }
        }
    }
}

impl LiveSession {
    /// Write one `user` funnel line: the pending stash first, then stale
    /// channel lines are cleared — except an `init`, which fires once at
    /// process start and may arrive before the first send. Anything else
    /// queued is post-`result` noise that must not leak into this turn.
    fn send_content(&mut self, content: &str) -> Result<(), String> {
        self.pending.clear();
        while let Ok(m) = self.rx.try_recv() {
            if m.as_ref()
                .ok()
                .and_then(|v| v.get("event").or_else(|| v.get("type")))
                .and_then(Value::as_str)
                == Some("init")
            {
                self.pending.push_back(m);
            }
        }
        let line = serde_json::to_string(&json!({"event": "user",
            "message": {"content": content}}))
        .map_err(|e| format!("frame encode: {e}"))?
            + "\n";
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "native stdin closed".to_string())?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|_| "native stdin closed".to_string())
    }

    /// Read this turn's lines until its `result` — the per-turn terminal
    /// line. The channel gives the deadline teeth: a silent child (quota
    /// backoff, a stuck retry) can't park the turn forever.
    fn drain(&mut self, deadline: Instant) -> Result<Vec<Value>, String> {
        let mut lines = Vec::new();
        loop {
            let msg = if let Some(m) = self.pending.pop_front() {
                m
            } else {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err("Antigravity request timed out".into());
                }
                match self.rx.recv_timeout(left) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => {
                        return Err("Antigravity request timed out".into());
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err("native stdout closed".into());
                    }
                }
            };
            let v = msg?;
            if self.conversation_id.is_empty() {
                self.conversation_id = chat::conversation_of(&v);
            }
            let terminal = v
                .get("event")
                .or_else(|| v.get("type"))
                .and_then(Value::as_str)
                == Some("result");
            lines.push(v);
            if terminal {
                return Ok(lines);
            }
        }
    }

    /// After a settled-but-failed result: `agy` exits on turn errors, so
    /// the child is usually already dead — give the wire a beat to prove
    /// it before the session is called alive. A late line is stashed, not
    /// lost.
    fn settled_alive(&mut self) -> bool {
        if self.dead || !matches!(self.child.try_wait(), Ok(None)) {
            return false;
        }
        match self.rx.recv_timeout(ERROR_GRACE) {
            Ok(m) => {
                self.pending.push_back(m);
                true
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    /// Record what this session just answered so the next request can be
    /// recognized as its strict continuation.
    fn absorb(&mut self, turn: &PreparedTurn, text: &str, calls: &[(String, String, String)]) {
        self.system = turn.system.clone();
        self.absorbed = turn.input.clone();
        self.reply_call_ids = calls.iter().map(|(id, _, _)| id.clone()).collect();
        self.reply_text = text.trim().to_string();
        self.last_used = Instant::now();
    }

    /// Record a failed turn whose prompt still settled upstream: the
    /// input counts as absorbed (its text may already sit in the child's
    /// transcript) but no reply is on record, so a later request
    /// continues with only its own tail. An identical retry still misses
    /// by design — nothing extends `absorbed` — and respawns.
    fn absorb_failed(&mut self, turn: &PreparedTurn) {
        self.system = turn.system.clone();
        self.absorbed = turn.input.clone();
        self.reply_call_ids = Vec::new();
        self.reply_text = String::new();
        self.last_used = Instant::now();
    }

    /// Trace tag: `pid` plus the conversation's short id once known —
    /// the live-test hook for "same child, same conversation".
    fn tag(&self) -> String {
        let short: String = self.conversation_id.chars().take(8).collect();
        if short.is_empty() {
            format!("{}", self.pid)
        } else {
            format!("{}:{}", self.pid, short)
        }
    }

    /// Best-effort teardown: stdin closes (agy exits on EOF), a short
    /// grace, then kill. Idempotent.
    fn close(&mut self) {
        if self.dead {
            return;
        }
        self.dead = true;
        drop(self.stdin.take());
        let until = Instant::now() + CLOSE_GRACE;
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveSession {
    /// Backstop: a session value must never drop with its child alive.
    fn drop(&mut self) {
        self.close();
    }
}

/// Run one turn: prompt a pooled continuation session with only the
/// delta when this request's history strictly extends what it last
/// answered, else spawn a fresh `agy` under one staged HOME and send
/// `system` + the whole transcript. Returns the folded Responses SSE
/// (the fold runs here because `absorb` needs its answer + call ids).
/// `effort` is the host's `reasoning.effort` pick, resolved onto pinned
/// route ids only. `timeout` is the whole-turn budget.
pub fn run_turn(
    body: &Value,
    model: &str,
    effort: Option<&str>,
    timeout: Duration,
    isolated: bool,
    account: Option<&crate::accounts::Pick>,
) -> Result<Vec<u8>, String> {
    let home = account.and_then(|p| p.home.as_deref());
    let out = run_turn_on(body, model, effort, timeout, isolated, home);
    if let Some(p) = account {
        match &out {
            Ok(_) => crate::accounts::note_success(&p.label),
            Err(e) => crate::accounts::note_failure(&p.label, e),
        }
    }
    out
}

fn run_turn_on(
    body: &Value,
    model: &str,
    effort: Option<&str>,
    timeout: Duration,
    isolated: bool,
    account: Option<&Path>,
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    let mut turn = chat::prepare_turn(body, model)?;
    turn.native_model = crate::catalog::native_model_for_effort(model, effort);
    let say: Arc<dyn Fn(String) + Send + Sync> = Arc::new(|_| {});
    if isolated {
        return run_isolated(&turn, &say, deadline, account);
    }
    let (claimed, mut reason) = take(&turn, account);
    if let Some((arc, delta)) = claimed {
        let chars = delta.chars().count();
        let mut s = lock(&arc);
        match s.send_content(&chat::fit_prompt("", &delta)) {
            Ok(()) => {
                let rewarmed = s.rewarmed;
                let out = settle(&mut s, &turn, &say, deadline);
                trace_result("reuse", &s, chars, &out);
                let died = s.dead;
                drop(s);
                if !died {
                    give_back(arc);
                }
                match out {
                    // A re-warmed child's first drain proves the resume:
                    // a dead wire there means a stale record, not a bad
                    // request — replay the transcript on a fresh child.
                    Err(_) if rewarmed && died => reason = "rewarm_dead",
                    out => return out,
                }
            }
            Err(_) => {
                // The delta never reached the child (a partial line is
                // invalid NDJSON — `agy` errors on it and exits anyway):
                // close it and replay the whole transcript on a fresh
                // session — the request is still answerable.
                s.close();
                drop(s);
                drop(arc);
                reason = "send_failed";
            }
        }
    }
    let mode = format!("fresh:{reason}");
    let content = chat::fit_prompt(&turn.system, &turn.content_line);
    let chars = content.chars().count();
    let arc = match spawn(&turn, None, account) {
        Ok(arc) => arc,
        Err(e) => {
            trace(&mode, "-", chars, None, Some(&e));
            return Err(e);
        }
    };
    let mut s = lock(&arc);
    if let Err(e) = s.send_content(&content) {
        // The prompt never reached the child: nothing to pool, and no
        // fresh fallback left — the turn fails.
        trace(&mode, "-", chars, None, Some(&e));
        s.close();
        return Err(e);
    }
    let out = settle(&mut s, &turn, &say, deadline);
    trace_result(&mode, &s, chars, &out);
    let poolable = !s.dead;
    drop(s);
    if poolable {
        give_back(arc);
    }
    out
}

fn run_isolated(
    turn: &PreparedTurn,
    say: &Arc<dyn Fn(String) + Send + Sync>,
    deadline: Instant,
    account: Option<&Path>,
) -> Result<Vec<u8>, String> {
    let content = chat::fit_prompt(&turn.system, &turn.content_line);
    let chars = content.chars().count();
    let arc = spawn(turn, None, account)
        .inspect_err(|e| trace("isolated", "-", chars, None, Some(e.as_str())))?;
    let mut s = lock(&arc);
    let out = match s.send_content(&content) {
        Ok(()) => s
            .drain(deadline)
            .and_then(|lines| chat::fold_lines(&lines, &turn.names, say).map(|f| f.0)),
        Err(e) => Err(e),
    };
    trace_result("isolated", &s, chars, &out);
    s.close();
    out
}

/// Drain a turn to its `result` line and fold it. On success the session
/// absorbs input + reply; on a settled-but-failed result it absorbs the
/// input with no reply and pools only if the child survived; on dead
/// wire (timeout, EOF, garbage) the session is killed — the error return
/// carries the detail either way.
fn settle(
    s: &mut LiveSession,
    turn: &PreparedTurn,
    say: &Arc<dyn Fn(String) + Send + Sync>,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    let lines = match s.drain(deadline) {
        Ok(lines) => lines,
        Err(e) => {
            s.close();
            return Err(e);
        }
    };
    match chat::fold_lines(&lines, &turn.names, say) {
        Ok((sse, _, text, calls, _usage, _)) => {
            s.absorb(turn, &text, &calls);
            record(s);
            Ok(sse)
        }
        Err(e) => {
            if s.settled_alive() {
                // The turn settled upstream — the session's state is
                // known, so the host's identical retry continues it on
                // this warm child (well, misses and respawns — but a
                // LATER distinct turn continues with only its tail).
                s.absorb_failed(turn);
                record(s);
            } else {
                s.close();
            }
            Err(e)
        }
    }
}

/// Persist the session's pool record — the `--conversation` resume point
/// plus the continuation state a restarted plugin re-pools from (see
/// [`rewarm`]). Called on every settled turn: `s` has just absorbed the
/// turn, so its fields are exactly what `continuation` verifies next.
/// Best-effort — a lost write only costs the next restart its re-warm.
fn record(s: &LiveSession) {
    if s.conversation_id.is_empty() {
        return;
    }
    session::save_record(&session::Record {
        sid: s.conversation_id.clone(),
        system: s.system.clone(),
        model: s.native_model.clone(),
        absorbed: s.absorbed.clone(),
        reply_call_ids: s.reply_call_ids.clone(),
        reply_text: s.reply_text.clone(),
        home: s
            .account
            .as_deref()
            .map(|h| h.display().to_string())
            .unwrap_or_default(),
        saved_at: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    });
}

/// Trace one turn outcome under `ANTIGRAVITY_SUB_DEBUG`.
fn trace_result(mode: &str, s: &LiveSession, chars: usize, out: &Result<Vec<u8>, String>) {
    match out {
        Ok(_) => trace(mode, &s.tag(), chars, None, None),
        Err(e) => trace(mode, &s.tag(), chars, None, Some(e)),
    }
}

/// One line per turn when `ANTIGRAVITY_SUB_DEBUG` is set: mode,
/// `pid:conversation` session tag, prompt size and usage — never prompt
/// content. Appends (mode 0600) to `<tempdir>/antigravity-sub-<pid>.log`.
fn trace(mode: &str, tag: &str, chars: usize, usage: Option<&Usage>, err: Option<&str>) {
    if std::env::var_os("ANTIGRAVITY_SUB_DEBUG").is_none() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}.{:03}", d.as_secs(), d.subsec_millis()))
        .unwrap_or_default();
    let outcome = match (usage, err) {
        (Some(u), _) => format!(
            "usage={}/{}/{}",
            u.input_tokens, u.cached_tokens, u.output_tokens
        ),
        (None, Some(e)) => format!("error:{}", e.chars().take(120).collect::<String>()),
        (None, None) => "ok".to_string(),
    };
    let line = format!("{ts}\t{mode}\tsession={tag}\tchars={chars}\t{outcome}\n");
    let path = std::env::temp_dir().join(format!("antigravity-sub-{}.log", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}
