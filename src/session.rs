//! Persisted pool records: a plugin restart used to strand every warm
//! `agy` session — the pool died with the process and the conversation's
//! upstream cache entry lapsed before its next turn, so the next turn
//! re-billed the whole transcript on a cold replay. Two stores fix it:
//!
//! * `resume/<sid>` — a JSON [`Record`] per session, written atomically
//!   (0600, temp+rename) on every settled turn: the `--conversation`
//!   resume point plus the strict-continuation state (`absorbed` prefix,
//!   reply echo) that lets a re-warmed child answer the next turn.
//! * `conversations/` — `agy`'s own per-conversation store, held in this
//!   cache dir and symlinked into every staged HOME (see
//!   [`crate::chat::TurnIsolation`]), so transcripts outlive both the
//!   session and the plugin process.
//!
//! [`crate::live::rewarm`] re-pools records inside the warm window at
//! startup; [`sweep`] bounds both stores to [`TTL`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Records and conversation dbs idle past this are swept.
pub const TTL: Duration = Duration::from_secs(24 * 3600);
/// Minimum gap between sweeps (shared across processes via a marker file).
const SWEEP_EVERY: Duration = Duration::from_secs(3600);

/// `$XDG_CACHE_HOME/antigravity-sub` (or `~/.cache/antigravity-sub`).
pub fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("antigravity-sub")
}

/// The persistent `agy` conversation store every staged HOME symlinks
/// `~/.gemini/antigravity-cli/conversations` at — a record is useless
/// without it: `--conversation <sid>` resolves through this dir.
/// Created (0700) on first use.
pub fn conversations_dir() -> PathBuf {
    let dir = cache_root().join("conversations");
    let _ = private_dir(&dir);
    dir
}

/// A persisted pool entry: the `agy` conversation id (the
/// `--conversation` resume point) plus the continuation state a
/// restarted plugin needs to re-warm the session into the pool (see
/// [`crate::live::rewarm`]). Files holding only a bare conversation id —
/// or any unreadable remnant — still parse: they carry no continuation
/// state, so they are never re-warmed.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct Record {
    /// `agy` conversation id — the `--conversation` resume point and the
    /// record's filename.
    pub sid: String,
    /// Route + funnel text the session ran under — matching requires it.
    #[serde(default)]
    pub system: String,
    /// Native `--model` id (effort is already folded into it).
    #[serde(default)]
    pub model: String,
    /// The request `input` array the session had absorbed at save time
    /// — the strict-continuation prefix the next turn must extend.
    #[serde(default)]
    pub absorbed: Vec<Value>,
    /// Call ids and text of the session's last answer — the echo zone
    /// the next turn replays and `continuation` verifies.
    #[serde(default)]
    pub reply_call_ids: Vec<String>,
    #[serde(default)]
    pub reply_text: String,
    /// Unix seconds when written; restores the idle/keepalive clocks.
    #[serde(default)]
    pub saved_at: u64,
}

/// Parse a record file: JSON, else a bare `sid` line (a legacy point —
/// resumable in principle but carrying no continuation state, so never
/// re-warmed).
fn record_of(text: &str) -> Option<Record> {
    let rec = serde_json::from_str::<Record>(text).unwrap_or_else(|_| Record {
        sid: text.trim().to_string(),
        ..Default::default()
    });
    is_uuid(&rec.sid).then_some(rec)
}

fn resume_file(sid: &str) -> PathBuf {
    cache_root().join("resume").join(sid)
}

/// Every stored record with its file age, freshest first. Rewarm scans
/// these at startup; stale or unreadable files are skipped — worst case
/// a conversation falls back to a cold replay spawn.
pub fn records() -> Vec<(Record, Duration)> {
    let dir = cache_root().join("resume");
    let now = SystemTime::now();
    let mut out: Vec<(Record, Duration)> = Vec::new();
    for p in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let Ok(text) = std::fs::read_to_string(p.path()) else {
            continue;
        };
        let Some(rec) = record_of(&text) else {
            continue;
        };
        let age = p
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or(Duration::MAX);
        out.push((rec, age));
    }
    out.sort_by_key(|(_, age)| *age);
    out
}

/// Write the pool record — atomically (0600, temp+rename) so a crash
/// mid-write never leaves a torn file — and run the periodic sweep.
/// Best-effort: a failed write only costs the next restart its re-warm.
pub fn save_record(rec: &Record) {
    if !is_uuid(&rec.sid) {
        return;
    }
    if let Ok(text) = serde_json::to_string(rec) {
        let _ = write_atomic(&resume_file(&rec.sid), text.as_bytes());
        sweep_if_due(&cache_root(), SystemTime::now());
    }
}

/// Whether `agy`'s conversation db for `sid` is still in the persistent
/// store — a record whose transcript was swept or corrupted can't
/// resume, so rewarm skips it. The SQLite magic check keeps a torn or
/// foreign file from buying a 30s init wait on a child that can never
/// mount.
pub fn session_exists(sid: &str) -> bool {
    if !is_uuid(sid) {
        return false;
    }
    let path = conversations_dir().join(format!("{sid}.db"));
    let mut head = [0u8; 16];
    use std::io::Read;
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|_| &head == b"SQLite format 3\0")
}

/// Canonical 8-4-4-4-12 hex uuid — the shape `agy` conversation ids have.
/// Checked before an id becomes an argv or path component.
pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Write `bytes` to `path` atomically (temp file + rename), mode 0600,
/// creating the parent directory (0700). Concurrent writers never expose
/// a torn file: readers see the old content or the new, whole.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    private_dir(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        let r = f.write_all(bytes).and_then(|_| f.flush());
        drop(f);
        r?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

fn idle_past(path: &Path, now: SystemTime, ttl: Duration) -> bool {
    path.symlink_metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > ttl))
}

/// At most once per [`SWEEP_EVERY`] (marker `root/.swept`), run [`sweep`].
fn sweep_if_due(root: &Path, now: SystemTime) {
    let marker = root.join(".swept");
    if marker.exists() && !idle_past(&marker, now, SWEEP_EVERY) {
        return;
    }
    if write_atomic(&marker, b"").is_ok() {
        sweep(root, now, TTL);
    }
}

/// Delete records and `agy` conversation dbs idle past `ttl`. Only our
/// own store is touched — the `conversations/` dir holds only sessions
/// this plugin spawned (staged HOMEs point it here), and only
/// `<uuid>.db` names count as conversations.
pub fn sweep(root: &Path, now: SystemTime, ttl: Duration) {
    let entries = |d: PathBuf| {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
    };
    for p in entries(root.join("resume")) {
        if idle_past(&p, now, ttl) {
            let _ = std::fs::remove_file(&p);
        }
    }
    for p in entries(root.join("conversations")) {
        // `<sid>.db` plus its SQLite `-wal`/`-shm` siblings.
        let is_ours = p
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|n| n.split('.').next())
            .is_some_and(is_uuid);
        if is_ours && idle_past(&p, now, ttl) {
            let _ = std::fs::remove_file(&p);
        }
    }
}
