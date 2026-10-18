//! Running sessions: context use, auto-compact point and prompt cache state.
//!
//! Claude: the live process list in ~/.claude/sessions, the end of each transcript, and,
//! for terminal sessions, the status line snapshot that `quotatop tap` saves.
//! Codex: rollout logs in ~/.codex/sessions written in the last few hours.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::Value;

use crate::model::{Activity, Cache, Session, Tool};
use crate::util;

/// Bytes read from the end of a log the first time it is seen.
const FIRST_READ: u64 = 2 * 1024 * 1024;
/// Codex sessions written in the last this-many seconds are listed.
const CODEX_RECENT: i64 = 6 * 3600;
/// Codex sessions shown at most; the rest are counted.
const CODEX_SHOWN: usize = 6;
/// Default auto-compact point for models with a native 1M window, from the Claude Code docs
/// ("at about 967K tokens by default").
const CLAUDE_1M_DEFAULT_COMPACT: u64 = 967_000;
/// Settings files in a session's folder come from the repo, so their size is capped.
const SETTINGS_MAX: u64 = 1 << 20;
/// Files the CLIs write for themselves (models_cache.json is about 400 KB).
const OWN_FILE_MAX: u64 = 32 << 20;
/// Above this many cached tokens, Claude Code may compact an idle 1 h session before it
/// goes cold, so the re-cache figure is no longer a lower bound.
const IDLE_COMPACT_ABOVE: u64 = 200_000;

/// Environment variables that change a Claude session's window or compact point.
pub const ENV_KEYS: &[&str] = &[
    "ANTHROPIC_MODEL",
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_DISABLE_1M_CONTEXT",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "DISABLE_AUTO_COMPACT",
    "DISABLE_COMPACT",
];

pub type Env = HashMap<String, String>;

pub struct Collector {
    claude_dir: PathBuf,
    codex_dir: PathBuf,
    spool_dir: PathBuf,
    /// quotatop's own environment, used when a session's cannot be read.
    own_env: Env,
    /// Read each Claude session's environment from its process.
    read_process_env: bool,
    /// Why the Claude session list could not be read, if it could not.
    pub error: Option<String>,
    /// Recent Codex sessions left off the list.
    pub codex_hidden: usize,
    files: FileCache,
    registry: HashMap<PathBuf, Arc<Value>>,
    process_envs: HashMap<u32, Env>,
    claude_logs: HashMap<String, ClaudeLog>,
    transcript_paths: HashMap<String, PathBuf>,
    /// When to list the project folders again for a session with no transcript yet.
    transcript_retry: HashMap<String, i64>,
    codex_logs: HashMap<PathBuf, CodexLog>,
    codex_names: (Option<i64>, HashMap<String, String>),
    /// Recent rollouts from the last walk over every day folder, and when it ran.
    codex_walk: (Option<i64>, Vec<PathBuf>),
}

impl Collector {
    pub fn new() -> Self {
        let own_env = ENV_KEYS
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
            .collect();
        let mut c = Collector::at(util::claude_dir(), util::codex_dir(), util::spool_dir(), own_env);
        c.read_process_env = true;
        c
    }

    /// A collector over the given folders that uses `env` for every session.
    pub fn at(claude_dir: PathBuf, codex_dir: PathBuf, spool_dir: PathBuf, env: Env) -> Self {
        Collector {
            claude_dir,
            codex_dir,
            spool_dir,
            own_env: env,
            read_process_env: false,
            error: None,
            codex_hidden: 0,
            files: FileCache::default(),
            registry: HashMap::new(),
            process_envs: HashMap::new(),
            claude_logs: HashMap::new(),
            transcript_paths: HashMap::new(),
            transcript_retry: HashMap::new(),
            codex_logs: HashMap::new(),
            codex_names: (None, HashMap::new()),
            codex_walk: (None, Vec::new()),
        }
    }

    /// Returns sessions sorted with working ones first, then by last activity.
    pub fn collect(&mut self) -> Vec<Session> {
        let now = util::now();
        let mut out = self.claude_sessions(now);
        out.extend(self.codex_sessions(now));
        self.files.prune();
        out.sort_by_key(|s| {
            let working = s.activity == Activity::Working;
            let last = match s.activity {
                Activity::LastActive(t) => t,
                _ => s.last_request.unwrap_or(0),
            };
            (!working, std::cmp::Reverse(last))
        });
        out
    }

    // -------------------------------------------------------- Claude

    fn claude_sessions(&mut self, now: i64) -> Vec<Session> {
        let dir = self.claude_dir.clone();
        let sessions_dir = dir.join("sessions");
        let entries = match std::fs::read_dir(&sessions_dir) {
            Ok(e) => {
                self.error = None;
                e
            }
            Err(e) => {
                self.error = (e.kind() != std::io::ErrorKind::NotFound)
                    .then(|| format!("could not read {}: {e}", sessions_dir.display()));
                return Vec::new();
            }
        };
        let user_settings = self.files.json(&dir.join("settings.json"), SETTINGS_MAX);
        let mut out = Vec::new();
        let mut seen = Vec::new();
        let mut paths = Vec::new();
        let mut pids = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            paths.push(p.clone());
            // A file read while Claude Code rewrites it may not parse: use the last good copy.
            let reg = match self.files.json(&p, SETTINGS_MAX) {
                Some(v) => {
                    self.registry.insert(p.clone(), v.clone());
                    v
                }
                None => match self.registry.get(&p) {
                    Some(v) => v.clone(),
                    None => continue,
                },
            };
            let Some(pid) = reg.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()) else { continue };
            // Only interactive sessions; `claude -p` runs (ours included) are not listed.
            if reg.get("kind").and_then(Value::as_str).is_some_and(|k| k != "interactive") {
                continue;
            }
            if !util::pid_alive(pid) {
                continue;
            }
            // A leftover file whose PID now belongs to a newer process is not a session.
            if let (Some(start), Some(written)) = (util::process_start(pid), util::mtime(&p)) {
                if start > written + 5 {
                    continue;
                }
            }
            let Some(sid) = reg.get("sessionId").and_then(Value::as_str).filter(|s| util::safe_id(s)).map(str::to_string)
            else {
                continue;
            };
            seen.push(sid.clone());
            pids.push(pid);
            let cwd = reg.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
            let name = reg
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| last_component(&cwd));
            let activity = match reg.get("status").and_then(Value::as_str) {
                Some("busy") => Activity::Working,
                _ => Activity::Idle,
            };

            let env = self.session_env(pid);
            let settings = self.settings_for(user_settings.as_deref(), &cwd, env);
            let tap = self.read_tap(&sid);
            let log = self.claude_log(&dir, &sid, &cwd, now);
            out.push(claude_session(sid, name, activity, log, tap.as_ref(), &settings));
        }
        self.claude_logs.retain(|k, _| seen.contains(k));
        self.transcript_paths.retain(|k, _| seen.contains(k));
        self.transcript_retry.retain(|k, _| seen.contains(k));
        self.registry.retain(|k, _| paths.contains(k));
        self.process_envs.retain(|k, _| pids.contains(k));
        out
    }

    /// The session's own environment where it can be read, else quotatop's.
    fn session_env(&mut self, pid: u32) -> Env {
        if !self.read_process_env {
            return self.own_env.clone();
        }
        if let Some(e) = self.process_envs.get(&pid) {
            return e.clone();
        }
        let env = util::process_env(pid, ENV_KEYS).unwrap_or_else(|| self.own_env.clone());
        self.process_envs.insert(pid, env.clone());
        env
    }

    fn settings_for(&mut self, user: Option<&Value>, cwd: &str, env: Env) -> Settings {
        let mut layers = Vec::new();
        if !cwd.is_empty() {
            let d = Path::new(cwd).join(".claude");
            for f in ["settings.local.json", "settings.json"] {
                layers.extend(self.files.json(&d.join(f), SETTINGS_MAX).map(|v| (*v).clone()));
            }
        }
        layers.extend(user.cloned());
        Settings::new(layers, env)
    }

    /// The status line snapshot saved by `quotatop tap`, with its file time.
    fn read_tap(&mut self, sid: &str) -> Option<(Value, i64)> {
        let p = self.spool_dir.join(format!("{sid}.json"));
        let m = util::mtime(&p)?;
        Some(((*self.files.json(&p, SETTINGS_MAX)?).clone(), m))
    }

    fn claude_log(&mut self, dir: &Path, sid: &str, cwd: &str, now: i64) -> Option<&ClaudeLog> {
        let path = self.transcript(dir, sid, cwd, now)?;
        let log = self.claude_logs.entry(sid.to_string()).or_insert_with(|| ClaudeLog::new(path.clone()));
        if log.path != path {
            *log = ClaudeLog::new(path);
        }
        log.update();
        Some(log)
    }

    fn transcript(&mut self, dir: &Path, sid: &str, cwd: &str, now: i64) -> Option<PathBuf> {
        if let Some(p) = self.transcript_paths.get(sid).filter(|p| p.is_file()) {
            return Some(p.clone());
        }
        let file = format!("{sid}.jsonl");
        let projects = dir.join("projects");
        // Claude Code names the project folder after the cwd, every other character a dash.
        let folder: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let direct = projects.join(folder).join(&file);
        let found = if direct.is_file() {
            Some(direct)
        } else if self.transcript_retry.get(sid).is_some_and(|t| now < *t) {
            None
        } else {
            // A new session has no transcript until its first message: look again in 30 s.
            self.transcript_retry.insert(sid.to_string(), now + 30);
            std::fs::read_dir(&projects).ok()?.flatten().map(|d| d.path().join(&file)).find(|p| p.is_file())
        };
        let found = found?;
        self.transcript_paths.insert(sid.to_string(), found.clone());
        Some(found)
    }

    // -------------------------------------------------------- Codex

    fn codex_sessions(&mut self, now: i64) -> Vec<Session> {
        self.codex_hidden = 0;
        let home = self.codex_dir.clone();
        let root = home.join("sessions");
        if !root.is_dir() {
            return Vec::new();
        }
        let config = self.files.text(&home.join("config.toml"), OWN_FILE_MAX);
        let compact_limit = config.as_deref().and_then(codex_compact_limit);
        let models = self.files.json(&home.join("models_cache.json"), OWN_FILE_MAX);
        let models = models.as_deref();
        self.refresh_codex_names(&home.join("session_index.jsonl"));

        let files = self.codex_candidates(&root, now);
        self.codex_logs.retain(|k, _| files.contains(k));
        let mut out = Vec::new();
        for f in files {
            let log = self.codex_logs.entry(f.clone()).or_insert_with(|| CodexLog::new(f));
            log.read_meta();
            // Guardian reviews and other sub-agents are threads inside a session, not sessions.
            if log.subagent {
                continue;
            }
            if out.len() >= CODEX_SHOWN {
                if !log.imported() {
                    self.codex_hidden += 1;
                }
                continue;
            }
            log.update();
            let Some(tok) = log.tokens.as_ref() else { continue };
            if tok.total == 0 || log.imported() {
                continue;
            }
            let model = log.model.clone();
            let window = tok.window.or_else(|| codex_window(models, model.as_deref()));
            let model_limit = model.as_deref().and_then(|m| codex_model_field(models, m, "auto_compact_token_limit"));
            let compact_at = match (compact_limit.or(model_limit), window) {
                (Some(l), Some(w)) => Some(l.min(w)),
                (Some(l), None) => Some(l),
                _ => None,
            };
            let id = log.id.clone().unwrap_or_default();
            let mut name = self
                .codex_names
                .1
                .get(&id)
                .cloned()
                .unwrap_or_else(|| last_component(log.cwd.as_deref().unwrap_or("?")));
            if log.exec {
                name.push_str(" (exec)");
            }
            let last = log.last_write.unwrap_or(0);
            out.push(Session {
                tool: Tool::Codex,
                id,
                name,
                model: model.map(|m| codex_display_name(models, &m)),
                activity: if now - last < 90 { Activity::Working } else { Activity::LastActive(last) },
                last_request: Some(last),
                context: Some(tok.total),
                window,
                compact_at,
                compact_approx: false,
                compact_note: if compact_at.is_none() { Some("not set in config.toml".into()) } else { None },
                cache: Cache::NotReported,
            });
        }
        out
    }

    /// Rollouts written in the last 6 hours, newest first. Codex keeps writing to a session's
    /// file in the folder of the day it started, so every day folder is walked once a minute.
    /// Between walks, today's and yesterday's folders catch new sessions.
    fn codex_candidates(&mut self, root: &Path, now: i64) -> Vec<PathBuf> {
        let recent = |p: &Path| util::mtime(p).filter(|m| now - m < CODEX_RECENT);
        if self.codex_walk.0.is_none_or(|t| now - t >= 60) {
            let mut all = Vec::new();
            for dir in subdirs(root).iter().flat_map(|y| subdirs(y)).flat_map(|m| subdirs(&m)) {
                all.extend(rollouts(&dir).into_iter().filter(|p| recent(p).is_some()));
            }
            self.codex_walk = (Some(now), all);
        }
        let mut files = self.codex_walk.1.clone();
        let today = chrono::Local::now().date_naive();
        for back in 0..2 {
            let Some(d) = today.checked_sub_days(chrono::Days::new(back)) else { continue };
            let dir = root.join(d.format("%Y").to_string()).join(d.format("%m").to_string()).join(d.format("%d").to_string());
            files.extend(rollouts(&dir));
        }
        files.sort();
        files.dedup();
        let mut timed: Vec<(i64, PathBuf)> = files.into_iter().filter_map(|p| Some((recent(&p)?, p))).collect();
        timed.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
        timed.into_iter().map(|(_, p)| p).collect()
    }

    fn refresh_codex_names(&mut self, path: &Path) {
        let m = util::mtime(path);
        if m.is_none() || m == self.codex_names.0 {
            return;
        }
        let mut names = HashMap::new();
        let text = util::read_limited(path, OWN_FILE_MAX).unwrap_or_default();
        for line in String::from_utf8_lossy(&text).lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if let (Some(id), Some(n)) = (v.get("id").and_then(Value::as_str), v.get("thread_name").and_then(Value::as_str)) {
                names.insert(id.to_string(), n.to_string());
            }
        }
        self.codex_names = (m, names);
    }
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(p) else { return Vec::new() };
    rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()
}

fn rollouts(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-")))
        .collect()
}

// ------------------------------------------------------------ file cache

/// Parsed files kept until their size or modification time changes, so settings,
/// config.toml and models_cache.json are not re-read and re-parsed every 5 s.
#[derive(Default)]
struct FileCache {
    entries: HashMap<PathBuf, CachedFile>,
}

struct CachedFile {
    stamp: (u64, Option<SystemTime>),
    value: Option<Arc<Value>>,
    used: bool,
}

impl FileCache {
    fn get(&mut self, path: &Path, max: u64, parse: impl FnOnce(Vec<u8>) -> Option<Value>) -> Option<Arc<Value>> {
        let Ok(meta) = std::fs::metadata(path) else {
            self.entries.remove(path);
            return None;
        };
        let stamp = (meta.len(), meta.modified().ok());
        if let Some(c) = self.entries.get_mut(path).filter(|c| c.stamp == stamp) {
            c.used = true;
            return c.value.clone();
        }
        let value = if meta.is_file() { util::read_limited(path, max).ok().and_then(parse).map(Arc::new) } else { None };
        self.entries.insert(path.to_path_buf(), CachedFile { stamp, value: value.clone(), used: true });
        value
    }

    fn json(&mut self, path: &Path, max: u64) -> Option<Arc<Value>> {
        self.get(path, max, |b| serde_json::from_slice(&b).ok())
    }

    fn text(&mut self, path: &Path, max: u64) -> Option<String> {
        let v = self.get(path, max, |b| Some(Value::String(String::from_utf8_lossy(&b).into_owned())))?;
        v.as_str().map(str::to_string)
    }

    /// Drops files not asked for since the last prune, such as settings of ended sessions.
    fn prune(&mut self) {
        self.entries.retain(|_, c| std::mem::take(&mut c.used));
    }
}

// ------------------------------------------------------------ Claude transcript

/// What the end of a Claude transcript says about the last API request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeLog {
    path: PathBuf,
    offset: u64,
    size: Option<(u64, i64)>,
    /// Time of the newest main-chain user entry: the request is sent right after it.
    last_user_ms: Option<i64>,
    msg_id: Option<String>,
    pub request_ms: Option<i64>,
    pub model: Option<String>,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// TTL of the newest cache write, in seconds.
    pub ttl: Option<i64>,
    pub compacted: bool,
    pub has_usage: bool,
    scanned: bool,
}

impl ClaudeLog {
    fn new(path: PathBuf) -> Self {
        ClaudeLog { path, ..Default::default() }
    }

    fn update(&mut self) {
        let Ok(meta) = std::fs::metadata(&self.path) else { return };
        let key = (meta.len(), util::mtime(&self.path).unwrap_or(0));
        if self.size == Some(key) {
            return;
        }
        let first = self.offset == 0;
        if let Ok((lines, off)) = util::read_new_lines(&self.path, self.offset, FIRST_READ) {
            for l in &lines {
                self.feed(l);
            }
            if first && !self.has_usage && !self.scanned {
                // Large tool results can push the last response out of the tail: read it all once.
                *self = ClaudeLog { path: self.path.clone(), scanned: true, ..Default::default() };
                let path = self.path.clone();
                scan_lines(&path, off, |l| self.feed(l));
            }
            self.offset = off;
            self.size = Some(key);
        }
    }

    /// Reads one transcript line. Only main-chain entries count.
    pub fn feed(&mut self, line: &str) {
        // Skip the JSON parse for lines that cannot matter (most are attachments and metadata).
        if !(line.contains("\"assistant\"") || line.contains("\"user\"") || line.contains("compact_boundary")) {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "system" && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            self.compacted = true;
            self.ttl = None;
            return;
        }
        if kind == "user" {
            if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(util::parse_iso_ms) {
                self.last_user_ms = Some(ts);
            }
            return;
        }
        if kind == "assistant" {
            let Some(msg) = v.get("message") else { return };
            let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
            let Some(u) = msg.get("usage") else { return };
            if model.starts_with('<') {
                return; // "<synthetic>" entries carry no real usage
            }
            let id = msg.get("id").and_then(Value::as_str).map(str::to_string);
            if id.is_none() || id != self.msg_id {
                // First block of a new response: the request went out after the last user entry.
                self.msg_id = id;
                self.request_ms = self.last_user_ms.or_else(|| v.get("timestamp").and_then(Value::as_str).and_then(util::parse_iso_ms));
            }
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            self.input = n("input_tokens");
            self.cache_read = n("cache_read_input_tokens");
            self.cache_write = n("cache_creation_input_tokens");
            match u.get("cache_creation").filter(|c| !c.is_null()) {
                Some(cc) => {
                    let w1h = cc.get("ephemeral_1h_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let w5m = cc.get("ephemeral_5m_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    if w1h > 0 {
                        self.ttl = Some(3600);
                    } else if w5m > 0 {
                        self.ttl = Some(300);
                    }
                }
                // Responses from before the TTL breakdown existed could only write 5 m entries.
                None if self.cache_write > 0 => self.ttl = Some(300),
                None => {}
            }
            self.model = Some(model.to_string());
            self.compacted = false;
            self.has_usage = true;
        }
    }
}

fn claude_session(
    sid: String,
    name: String,
    activity: Activity,
    log: Option<&ClaudeLog>,
    tap: Option<&(Value, i64)>,
    settings: &Settings,
) -> Session {
    let model = log.and_then(|l| l.model.clone()).or_else(|| {
        tap.and_then(|(v, _)| v.pointer("/model/id").and_then(Value::as_str).map(str::to_string))
    });
    let last_request = log.and_then(|l| l.request_ms).map(|ms| ms / 1000);
    // The tap snapshot wins when it is at least as new as the last request in the transcript.
    let tap = tap.filter(|(_, m)| last_request.is_none_or(|r| *m >= r));
    let nonnull = |v: Option<&Value>| v.filter(|c| !c.is_null()).cloned();
    let tap_usage = tap.and_then(|(v, _)| nonnull(v.pointer("/context_window/current_usage")));
    let tap_cache = tap.and_then(|(v, _)| nonnull(v.get("prompt_cache")));

    let mut window = model.as_deref().and_then(|m| claude_window(m, settings));
    let mut context = log.filter(|l| l.has_usage && !l.compacted).map(|l| l.input + l.cache_read + l.cache_write);
    if let Some((v, _)) = tap {
        if let Some(w) = v.pointer("/context_window/context_window_size").and_then(Value::as_u64) {
            window = Some(w);
        }
    }
    if let Some(cu) = &tap_usage {
        let n = |k: &str| cu.get(k).and_then(Value::as_u64).unwrap_or(0);
        context = Some(n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens"));
    }
    let (compact_at, compact_approx, compact_note) = claude_compact(model.as_deref(), window, settings);

    let cache = match (tap_cache, tap_usage, log) {
        // Right after /compact the snapshot has no usage yet: show nothing rather than "0 cached".
        (Some(pc), Some(cu), _) => {
            let n = |k: &str| cu.get(k).and_then(Value::as_u64).unwrap_or(0);
            let cached = n("cache_read_input_tokens") + n("cache_creation_input_tokens");
            let ttl = match pc.get("ttl").and_then(Value::as_str) {
                Some("1h") => 3600,
                Some("5m") => 300,
                _ => 0,
            };
            match pc.get("expires_at").and_then(Value::as_i64) {
                Some(exp) => Cache::Known {
                    ttl,
                    expires_at: exp,
                    cached,
                    recache: pc.get("recache_tokens_if_cold").and_then(Value::as_u64),
                    recache_at_least: false,
                    exact: true,
                },
                None => Cache::NoData("no cache tokens in the last response".into()),
            }
        }
        (Some(_), None, _) => Cache::NoData("compacted; known after the next message".into()),
        (None, _, Some(l)) if l.compacted => Cache::NoData("compacted; known after the next message".into()),
        (None, _, Some(l)) if l.has_usage => {
            let cached = l.cache_read + l.cache_write;
            match (l.ttl, l.request_ms) {
                (Some(ttl), Some(req)) if cached > 0 => Cache::Known {
                    ttl,
                    expires_at: req / 1000 + ttl,
                    cached,
                    // The next request re-sends at least the prefix that is cached now, unless
                    // Claude Code compacts a large idle 1 h session first.
                    recache: Some(cached),
                    recache_at_least: !(ttl == 3600 && cached > IDLE_COMPACT_ABOVE),
                    exact: false,
                },
                (None, _) if cached > 0 => {
                    Cache::NoData(format!("{} cached; the log does not say for how long", util::tokens(cached)))
                }
                _ => Cache::NoData("the last response used no cache".into()),
            }
        }
        _ => Cache::NoData("no messages yet".into()),
    };
    Session {
        tool: Tool::Claude,
        id: sid,
        name,
        model: model.map(|m| claude_display_name(&normalize_model(&m))),
        activity,
        last_request,
        context,
        window,
        compact_at,
        compact_approx,
        compact_note,
        cache,
    }
}

/// Strips cloud provider wrapping from a model ID: Bedrock's `us.anthropic.` prefix and
/// `-v1:0` suffix, and Vertex's `@20261001` suffix.
pub fn normalize_model(id: &str) -> String {
    let mut s = id.trim();
    if let Some(i) = s.rfind("anthropic.") {
        s = &s[i + "anthropic.".len()..];
    }
    let one_m = s.ends_with("[1m]");
    let mut s = s.trim_end_matches("[1m]");
    if let Some(i) = s.find('@') {
        s = &s[..i];
    }
    if let Some(i) = s.rfind("-v") {
        let tail = &s[i + 2..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == ':') {
            s = &s[..i];
        }
    }
    if one_m { format!("{s}[1m]") } else { s.to_string() }
}

/// The model ID with no provider wrapping, `claude-` prefix, `[1m]` or date: a key for
/// matching `modelSettings` entries.
fn model_key(id: &str) -> String {
    let n = normalize_model(id);
    let n = n.trim_end_matches("[1m]");
    let n = n.trim_start_matches("claude-");
    n.split('-').filter(|p| !(p.len() == 8 && p.chars().all(|c| c.is_ascii_digit()))).collect::<Vec<_>>().join("-")
}

/// claude-opus-5-5 -> Opus 5.5
pub fn claude_display_name(id: &str) -> String {
    let base = id.trim_start_matches("claude-");
    let base = base.split('[').next().unwrap_or(base);
    let parts: Vec<&str> = base.split('-').filter(|p| !(p.len() == 8 && p.chars().all(|c| c.is_ascii_digit()))).collect();
    let Some((family, ver)) = parts.split_first() else { return id.to_string() };
    if family.chars().all(|c| c.is_ascii_alphabetic()) && ver.iter().all(|v| v.chars().all(|c| c.is_ascii_digit())) {
        let mut f = family.to_string();
        if let Some(c) = f.get_mut(..1) {
            c.make_ascii_uppercase();
        }
        if ver.is_empty() { f } else { format!("{f} {}", ver.join(".")) }
    } else {
        id.to_string()
    }
}

/// Window from the model docs: Fable 5+, Sonnet 5+, Haiku 5.5+ and Opus 4.7+ run with 1M by
/// default; older models with 200K unless 1M was chosen. None for a model not in the table.
fn table_window(id: &str, settings: &Settings) -> Option<u64> {
    let disable_1m = settings.flag("CLAUDE_CODE_DISABLE_1M_CONTEXT");
    let one_m = if disable_1m { 200_000 } else { 1_000_000 };
    let id = normalize_model(id);
    let base = id.trim_end_matches("[1m]").trim_start_matches("claude-");
    let parts: Vec<&str> = base.split('-').collect();
    // Old naming: claude-3-5-sonnet-20241022
    if parts.first().is_some_and(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return Some(200_000);
    }
    let family = *parts.first()?;
    let nums: Vec<u32> = parts[1..].iter().filter(|p| p.len() <= 2).filter_map(|p| p.parse().ok()).collect();
    let ver = (nums.first().copied()?, nums.get(1).copied().unwrap_or(0));
    let big = match family {
        "opus" => ver >= (4, 7),
        "sonnet" => ver >= (5, 0),
        "haiku" => ver >= (5, 5),
        "fable" => ver >= (5, 0),
        _ => return None,
    };
    // The transcript records the model without `[1m]`; the chosen model in settings or
    // ANTHROPIC_MODEL keeps it ("opus[1m]" or "claude-opus-4-6[1m]").
    let chose_1m = id.ends_with("[1m]")
        || settings.chosen_model().is_some_and(|m| {
            let m = normalize_model(&m);
            let Some(m) = m.strip_suffix("[1m]") else { return false };
            m == family || model_key(m) == model_key(base)
        });
    Some(if big || chose_1m { one_m } else { 200_000 })
}

pub fn claude_window(id: &str, settings: &Settings) -> Option<u64> {
    let table = table_window(id, settings);
    let max = settings.env("CLAUDE_CODE_MAX_CONTEXT_TOKENS").and_then(|v| parse_tokens(&v));
    // For a model Claude Code knows, the docs say the variable only counts with DISABLE_COMPACT.
    match (table, max) {
        (None, Some(n)) => Some(n),
        (Some(_), Some(n)) if settings.flag("DISABLE_COMPACT") => Some(n),
        (t, _) => t,
    }
}

/// Returns (compact point, approximate?, note).
pub fn claude_compact(model: Option<&str>, window: Option<u64>, s: &Settings) -> (Option<u64>, bool, Option<String>) {
    // `/config` writes autoCompactEnabled to the user settings.
    if s.flag("DISABLE_COMPACT")
        || s.flag("DISABLE_AUTO_COMPACT")
        || s.value("autoCompactEnabled").and_then(Value::as_bool) == Some(false)
    {
        return (None, false, Some("off".into()));
    }
    let Some(window) = window else { return (None, false, None) };
    let env_window = s
        .env("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
        .and_then(|v| parse_tokens(&v))
        .map(|v| v.clamp(100_000, 1_000_000));
    let set = match env_window {
        Some(w) => Some(w),
        None => s.compact_window(model),
    }
    .map(|w| w.min(window));
    let (base, approx) = match set {
        Some(w) => (w, false),
        None if window >= 1_000_000 => (CLAUDE_1M_DEFAULT_COMPACT, true),
        None => (window, true),
    };
    match s.env("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE").and_then(|v| v.trim().parse::<u64>().ok()).filter(|p| (1..100).contains(p)) {
        Some(p) => (Some(base.saturating_mul(p) / 100), true, None),
        None => (Some(base), approx, None),
    }
}

/// 300000, "300000", "500k", "1m" or "1.5M". None for anything else, "auto" included.
fn parse_tokens(v: &str) -> Option<u64> {
    let v = v.trim().to_ascii_lowercase().replace(['_', ','], "");
    let (num, mult) = match v.as_bytes().last()? {
        b'k' => (&v[..v.len() - 1], 1_000.0),
        b'm' => (&v[..v.len() - 1], 1_000_000.0),
        _ => (v.as_str(), 1.0),
    };
    let n: f64 = num.trim().parse().ok()?;
    (n.is_finite() && n > 0.0).then(|| (n * mult).round() as u64)
}

fn token_value(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => parse_tokens(s),
        _ => None,
    }
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

// ------------------------------------------------------------ settings

/// One Claude session's settings: the layers in precedence order (project local, project,
/// user) and the environment the session was started with.
#[derive(Default)]
pub struct Settings {
    layers: Vec<Value>,
    env: Env,
}

impl Settings {
    pub fn new(layers: Vec<Value>, env: Env) -> Self {
        Settings { layers, env }
    }

    fn value(&self, key: &str) -> Option<&Value> {
        self.layers.iter().find_map(|l| l.get(key).filter(|v| !v.is_null()))
    }

    /// An env var from a settings file's "env" block, else from the session's environment.
    fn env(&self, key: &str) -> Option<String> {
        self.layers
            .iter()
            .find_map(|l| l.get("env").and_then(|e| e.get(key)).and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            }))
            .or_else(|| self.env.get(key).cloned())
            .filter(|v| !v.is_empty())
    }

    fn flag(&self, key: &str) -> bool {
        self.env(key).is_some_and(|v| truthy(&v))
    }

    fn chosen_model(&self) -> Option<String> {
        self.env("ANTHROPIC_MODEL").or_else(|| self.value("model").and_then(Value::as_str).map(str::to_string))
    }

    /// The first layer that sets a compact window for this model, or for all models, decides.
    /// "auto" in a layer means the default and hides lower layers.
    fn compact_window(&self, model: Option<&str>) -> Option<u64> {
        let key = model.map(model_key);
        for layer in &self.layers {
            let per_model = key.as_ref().and_then(|k| {
                let ms = layer.get("modelSettings")?.as_object()?;
                ms.iter().find(|(name, _)| model_key(name) == *k).and_then(|(_, e)| e.get("autoCompactWindow"))
            });
            for v in [per_model, layer.get("autoCompactWindow")].into_iter().flatten() {
                if v.is_null() {
                    continue;
                }
                return token_value(v);
            }
        }
        None
    }
}

// ------------------------------------------------------------ Codex rollout

#[derive(Clone, Debug, PartialEq)]
pub struct CodexTokens {
    pub total: u64,
    pub window: Option<u64>,
    /// The last turn sent and received nothing, as in a thread imported from another tool.
    pub empty_turn: bool,
}

#[derive(Debug, Default)]
struct CodexLog {
    path: PathBuf,
    offset: u64,
    size: Option<(u64, i64)>,
    meta_read: bool,
    id: Option<String>,
    cwd: Option<String>,
    exec: bool,
    subagent: bool,
    model: Option<String>,
    tokens: Option<CodexTokens>,
    last_write: Option<i64>,
    scanned: bool,
}

impl CodexLog {
    fn new(path: PathBuf) -> Self {
        CodexLog { path, ..Default::default() }
    }

    /// A thread Codex imported has token counts but never ran a turn of its own.
    fn imported(&self) -> bool {
        self.size.is_some() && self.model.is_none() && self.tokens.as_ref().is_some_and(|t| t.empty_turn)
    }

    /// The first line is session_meta. It can be long (it holds the system prompt).
    fn read_meta(&mut self) {
        if self.meta_read {
            return;
        }
        use std::io::{BufRead, Read};
        let Ok(f) = util::open_regular(&self.path) else { return };
        let mut first = String::new();
        if std::io::BufReader::new(f.take(OWN_FILE_MAX)).read_line(&mut first).is_err() || !first.ends_with('\n') {
            return; // not written yet; try again next poll
        }
        self.meta_read = true;
        if let Ok(v) = serde_json::from_str::<Value>(&first) {
            let p = v.get("payload");
            self.id = p.and_then(|p| p.get("id")).and_then(Value::as_str).map(str::to_string);
            self.cwd = p.and_then(|p| p.get("cwd")).and_then(Value::as_str).map(str::to_string);
            let origin = p.and_then(|p| p.get("originator")).and_then(Value::as_str).unwrap_or("");
            self.exec = origin.contains("exec");
            self.subagent = p.and_then(|p| p.pointer("/source/subagent")).is_some_and(|s| !s.is_null());
        }
    }

    fn update(&mut self) {
        let Ok(meta) = std::fs::metadata(&self.path) else { return };
        let m = util::mtime(&self.path).unwrap_or(0);
        let key = (meta.len(), m);
        if self.size == Some(key) {
            return;
        }
        self.last_write = Some(m);
        let first = self.offset == 0;
        if let Ok((lines, off)) = util::read_new_lines(&self.path, self.offset, FIRST_READ) {
            for l in &lines {
                self.feed(l);
            }
            self.offset = off;
            self.size = Some(key);
        }
        // The model is only named in turn_context lines, which can sit before the tail.
        if first && self.model.is_none() && !self.scanned {
            self.scanned = true;
            let path = self.path.clone();
            scan_lines(&path, self.offset, |l| {
                if l.contains("\"turn_context\"") {
                    self.feed(l);
                }
            });
        }
    }

    fn feed(&mut self, line: &str) {
        if line.contains("\"turn_context\"") {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(m) = v.pointer("/payload/model").and_then(Value::as_str) {
                    self.model = Some(m.to_string());
                }
            }
        } else if line.contains("\"token_count\"") {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                let Some(info) = v.pointer("/payload/info").filter(|i| !i.is_null()) else { return };
                let last = |k: &str| info.pointer(&format!("/last_token_usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
                let window = info.get("model_context_window").and_then(Value::as_u64);
                self.tokens = Some(CodexTokens {
                    total: last("total_tokens"),
                    window,
                    empty_turn: last("input_tokens") == 0 && last("output_tokens") == 0,
                });
            }
        }
    }
}

fn codex_model_field(models: Option<&Value>, slug: &str, field: &str) -> Option<u64> {
    models?
        .get("models")?
        .as_array()?
        .iter()
        .find(|m| m.get("slug").and_then(Value::as_str) == Some(slug))?
        .get(field)?
        .as_u64()
}

/// Codex uses `context_window × effective_context_window_percent` (272000 × 95% = 258400).
fn codex_window(models: Option<&Value>, model: Option<&str>) -> Option<u64> {
    let m = model?;
    let w = codex_model_field(models, m, "context_window")?;
    let pct = codex_model_field(models, m, "effective_context_window_percent").unwrap_or(100);
    Some(w.saturating_mul(pct) / 100)
}

fn codex_display_name(models: Option<&Value>, slug: &str) -> String {
    models
        .and_then(|m| m.get("models"))
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|m| m.get("slug").and_then(Value::as_str) == Some(slug)))
        .and_then(|m| m.get("display_name"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| slug.to_string())
}

/// `model_auto_compact_token_limit` from the active profile, else from the top level.
fn codex_compact_limit(config: &str) -> Option<u64> {
    const KEY: &str = "model_auto_compact_token_limit";
    let parse = |v: String| v.replace('_', "").parse::<u64>().ok();
    let profile = toml_value(config, None, "profile").map(|p| p.trim_matches(|c| c == '"' || c == '\'').to_string());
    profile
        .and_then(|p| toml_value(config, Some(&format!("profiles.{p}")), KEY))
        .and_then(parse)
        .or_else(|| toml_value(config, None, KEY).and_then(parse))
}

/// Reads `key = value` from a TOML table (None for the top level, before any [table]).
/// Enough for the flat keys quotatop needs; not a TOML parser.
fn toml_value(text: &str, table: Option<&str>, key: &str) -> Option<String> {
    let mut current: Option<String> = None;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            current = Some(l.trim_start_matches('[').trim_end_matches(']').replace(['"', '\'', ' '], ""));
            continue;
        }
        if current.as_deref() != table {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            if k.trim().trim_matches('"') == key {
                return Some(v.split('#').next()?.trim().to_string());
            }
        }
    }
    None
}

// ------------------------------------------------------------ helpers

/// Calls `f` for each complete line in the first `end` bytes of a file.
fn scan_lines(path: &Path, end: u64, mut f: impl FnMut(&str)) {
    use std::io::{BufRead, Read};
    let Ok(file) = util::open_regular(path) else { return };
    let mut reader = std::io::BufReader::new(file.take(end));
    let mut buf = Vec::new();
    while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
        if buf.last() == Some(&b'\n') {
            f(&String::from_utf8_lossy(&buf));
        }
        buf.clear();
    }
}

fn last_component(path: &str) -> String {
    Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(layers: Vec<Value>) -> Settings {
        Settings::new(layers, Env::new())
    }

    fn with_env(layers: Vec<Value>, env: &[(&str, &str)]) -> Settings {
        Settings::new(layers, env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    #[test]
    fn windows_by_model() {
        let none = s(vec![]);
        assert_eq!(claude_window("claude-opus-5-5", &none), Some(1_000_000));
        assert_eq!(claude_window("claude-opus-4-7", &none), Some(1_000_000));
        assert_eq!(claude_window("claude-opus-4-6", &none), Some(200_000));
        assert_eq!(claude_window("claude-opus-4-6[1m]", &none), Some(1_000_000));
        assert_eq!(claude_window("claude-sonnet-4-6", &none), Some(200_000));
        assert_eq!(claude_window("claude-sonnet-5", &none), Some(1_000_000));
        assert_eq!(claude_window("claude-haiku-4-5-20251001", &none), Some(200_000));
        assert_eq!(claude_window("claude-haiku-5-5", &none), Some(1_000_000));
        assert_eq!(claude_window("claude-fable-5-1", &none), Some(1_000_000));
        assert_eq!(claude_window("claude-3-5-sonnet-20241022", &none), Some(200_000));
        assert_eq!(claude_window("gpt-x", &none), None);
        let off = s(vec![json!({"env": {"CLAUDE_CODE_DISABLE_1M_CONTEXT": "1"}})]);
        assert_eq!(claude_window("claude-opus-5-5", &off), Some(200_000));
    }

    #[test]
    fn provider_model_ids() {
        let none = s(vec![]);
        assert_eq!(claude_window("claude-haiku-5-5@20261001", &none), Some(1_000_000));
        assert_eq!(claude_window("us.anthropic.claude-opus-5-5-v1:0", &none), Some(1_000_000));
        assert_eq!(claude_window("anthropic.claude-sonnet-4-6-v1", &none), Some(200_000));
        assert_eq!(claude_display_name(&normalize_model("us.anthropic.claude-opus-5-5-v1:0")), "Opus 5.5");
    }

    #[test]
    fn one_m_chosen_in_settings() {
        // The transcript says claude-opus-4-6; the session was started on opus[1m].
        let chosen = s(vec![json!({"model": "opus[1m]"})]);
        assert_eq!(claude_window("claude-opus-4-6", &chosen), Some(1_000_000));
        let env = with_env(vec![], &[("ANTHROPIC_MODEL", "claude-opus-4-6[1m]")]);
        assert_eq!(claude_window("claude-opus-4-6", &env), Some(1_000_000));
        let other = s(vec![json!({"model": "sonnet[1m]"})]);
        assert_eq!(claude_window("claude-opus-4-6", &other), Some(200_000));
    }

    #[test]
    fn max_context_tokens_rule() {
        let max = with_env(vec![], &[("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "300000")]);
        assert_eq!(claude_window("claude-opus-5-5", &max), Some(1_000_000), "ignored for a known model");
        assert_eq!(claude_window("my-gateway-model", &max), Some(300_000), "used for an unknown one");
        let both = with_env(vec![], &[("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "300000"), ("DISABLE_COMPACT", "1")]);
        assert_eq!(claude_window("claude-opus-5-5", &both), Some(300_000));
    }

    #[test]
    fn display_names() {
        assert_eq!(claude_display_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(claude_display_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(claude_display_name("claude-sonnet-5"), "Sonnet 5");
    }

    #[test]
    fn compact_points() {
        let opus = Some("claude-opus-5-5");
        let user = s(vec![json!({"modelSettings": {"claude-opus-5-5": {"autoCompactWindow": 500000}}})]);
        assert_eq!(claude_compact(opus, Some(1_000_000), &user), (Some(500_000), false, None));
        let none = s(vec![]);
        assert_eq!(claude_compact(opus, Some(1_000_000), &none), (Some(967_000), true, None));
        // A project setting beats the user one. Both sit under the 1M window, so order matters.
        let layered = s(vec![json!({"autoCompactWindow": 300000}), json!({"autoCompactWindow": 900000})]);
        assert_eq!(claude_compact(opus, Some(1_000_000), &layered), (Some(300_000), false, None));
        // The window caps it.
        assert_eq!(claude_compact(Some("claude-sonnet-4-6"), Some(200_000), &layered), (Some(200_000), false, None));
        let env = s(vec![json!({"env": {"CLAUDE_CODE_AUTO_COMPACT_WINDOW": "400000"}}), json!({"autoCompactWindow": 300000})]);
        assert_eq!(claude_compact(opus, Some(1_000_000), &env).0, Some(400_000));
    }

    #[test]
    fn compact_layers_in_precedence_order() {
        let opus = Some("claude-opus-5-5");
        // /autocompact 500k in user settings, autoCompactWindow 300000 in the repo: the repo wins.
        let repo_over_user = s(vec![
            json!({"autoCompactWindow": 300000}),
            json!({"modelSettings": {"claude-opus-5-5": {"autoCompactWindow": "500k"}}}),
        ]);
        assert_eq!(claude_compact(opus, Some(1_000_000), &repo_over_user).0, Some(300_000));
        // A layer with modelSettings for another model does not hide a lower layer.
        let other_model = s(vec![
            json!({"modelSettings": {"claude-sonnet-5": {"autoCompactWindow": 200000}}}),
            json!({"autoCompactWindow": 400000}),
        ]);
        assert_eq!(claude_compact(opus, Some(1_000_000), &other_model).0, Some(400_000));
        // Date-suffixed IDs match the plain entry.
        let dated = s(vec![json!({"modelSettings": {"claude-haiku-4-5": {"autoCompactWindow": 150000}}})]);
        assert_eq!(claude_compact(Some("claude-haiku-4-5-20251001"), Some(200_000), &dated).0, Some(150_000));
        // "auto" means the default and hides lower layers.
        let auto = s(vec![json!({"autoCompactWindow": "auto"}), json!({"autoCompactWindow": 300000})]);
        assert_eq!(claude_compact(opus, Some(1_000_000), &auto), (Some(967_000), true, None));
    }

    #[test]
    fn compact_off_switches() {
        let off = (None, false, Some("off".to_string()));
        let config = s(vec![json!({"autoCompactEnabled": false})]);
        assert_eq!(claude_compact(Some("claude-opus-5-5"), Some(1_000_000), &config), off);
        let disable = with_env(vec![], &[("DISABLE_COMPACT", "1")]);
        assert_eq!(claude_compact(Some("claude-opus-5-5"), Some(1_000_000), &disable), off);
        let auto = with_env(vec![], &[("DISABLE_AUTO_COMPACT", "true")]);
        assert_eq!(claude_compact(Some("claude-opus-5-5"), Some(1_000_000), &auto), off);
        // A project turning it back on beats the user turning it off.
        let back_on = s(vec![json!({"autoCompactEnabled": true}), json!({"autoCompactEnabled": false})]);
        assert_eq!(claude_compact(Some("claude-opus-5-5"), Some(1_000_000), &back_on).0, Some(967_000));
    }

    #[test]
    fn token_values() {
        assert_eq!(parse_tokens("500k"), Some(500_000));
        assert_eq!(parse_tokens("1M"), Some(1_000_000));
        assert_eq!(parse_tokens("1.5m"), Some(1_500_000));
        assert_eq!(parse_tokens("300_000"), Some(300_000));
        assert_eq!(parse_tokens("auto"), None);
        assert_eq!(parse_tokens("NaN"), None);
    }

    fn line(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn transcript_cache_timing() {
        let mut log = ClaudeLog::default();
        log.feed(&line(json!({"type": "user", "timestamp": "2026-10-17T19:26:38.614Z", "isSidechain": false,
            "message": {"content": [{"type": "tool_result", "content": "\"timestamp\":\"2030-01-01T00:00:00Z\""}]}})));
        // Subagent traffic before the response is ignored; counting it would move the send time.
        log.feed(&line(json!({"type": "user", "timestamp": "2026-10-17T19:27:30.000Z", "isSidechain": true})));
        // A response that took two minutes: two blocks of one message, a tool result between them.
        let usage = json!({"input_tokens": 2, "cache_creation_input_tokens": 2102, "cache_read_input_tokens": 103209,
            "output_tokens": 540, "cache_creation": {"ephemeral_1h_input_tokens": 2102, "ephemeral_5m_input_tokens": 0}});
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:28:52.181Z", "isSidechain": false,
            "message": {"id": "msg_1", "model": "claude-opus-5-5", "usage": usage}})));
        log.feed(&line(json!({"type": "user", "timestamp": "2026-10-17T19:28:52.467Z", "isSidechain": false})));
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:28:55.006Z", "isSidechain": false,
            "message": {"id": "msg_1", "model": "claude-opus-5-5", "usage": usage}})));
        assert_eq!(log.request_ms, util::parse_iso_ms("2026-10-17T19:26:38.614Z"), "uses the user entry, not the tool result text");
        assert_eq!(log.ttl, Some(3600));
        assert_eq!(log.input + log.cache_read + log.cache_write, 105_313);

        let sess = claude_session("x".into(), "n".into(), Activity::Idle, Some(&log), None,
            &s(vec![json!({"modelSettings": {"claude-opus-5-5": {"autoCompactWindow": 500000}}})]));
        assert_eq!(sess.window, Some(1_000_000));
        assert_eq!(sess.compact_at, Some(500_000));
        match sess.cache {
            Cache::Known { expires_at, cached, recache, recache_at_least, exact, .. } => {
                assert_eq!(expires_at, util::parse_iso("2026-10-17T19:26:38Z").unwrap() + 3600);
                assert_eq!(cached, 105_311);
                assert_eq!(recache, Some(105_311));
                assert!(recache_at_least && !exact);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cache_without_ttl_breakdown() {
        // Older responses have no cache_creation object: a write then was a 5 m write.
        let mut log = ClaudeLog::default();
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:00:00Z",
            "message": {"id": "a", "model": "claude-opus-5-5", "usage": {"input_tokens": 5, "cache_creation_input_tokens": 100}}})));
        assert_eq!(log.ttl, Some(300));
        // A read with no write and no earlier TTL: say so instead of "no cache".
        let mut log = ClaudeLog::default();
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:00:00Z",
            "message": {"id": "a", "model": "claude-opus-5-5", "usage": {"input_tokens": 5, "cache_read_input_tokens": 100000}}})));
        let sess = claude_session("x".into(), "n".into(), Activity::Idle, Some(&log), None, &s(vec![]));
        assert_eq!(sess.cache, Cache::NoData("100k cached; the log does not say for how long".into()));
    }

    #[test]
    fn large_idle_session_is_not_a_lower_bound() {
        let mut log = ClaudeLog::default();
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:00:00Z",
            "message": {"id": "a", "model": "claude-opus-5-5", "usage": {"input_tokens": 5, "cache_read_input_tokens": 300000,
                "cache_creation_input_tokens": 10, "cache_creation": {"ephemeral_1h_input_tokens": 10}}}})));
        let sess = claude_session("x".into(), "n".into(), Activity::Idle, Some(&log), None, &s(vec![]));
        assert!(matches!(sess.cache, Cache::Known { recache_at_least: false, .. }));
    }

    #[test]
    fn compact_boundary_clears_context() {
        let mut log = ClaudeLog::default();
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:00:00Z",
            "message": {"id": "a", "model": "claude-opus-5-5", "usage": {"input_tokens": 5, "cache_read_input_tokens": 100}}})));
        log.feed(&line(json!({"type": "system", "subtype": "compact_boundary", "timestamp": "2026-10-17T19:01:00Z"})));
        let sess = claude_session("x".into(), "n".into(), Activity::Idle, Some(&log), None, &s(vec![]));
        assert_eq!(sess.context, None);
        assert!(matches!(sess.cache, Cache::NoData(_)));
    }

    #[test]
    fn tap_snapshot_wins_when_newer() {
        let mut log = ClaudeLog::default();
        log.feed(&line(json!({"type": "user", "timestamp": "2026-10-17T19:00:00Z"})));
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:00:05Z",
            "message": {"id": "a", "model": "claude-opus-5-5", "usage": {"input_tokens": 5, "cache_read_input_tokens": 100,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}}}})));
        let req = util::parse_iso("2026-10-17T19:00:00Z").unwrap();
        let tap = json!({"model": {"id": "claude-opus-5-5"},
            "context_window": {"context_window_size": 1000000, "current_usage": {"input_tokens": 5,
                "cache_read_input_tokens": 120000, "cache_creation_input_tokens": 3000}},
            "prompt_cache": {"warm": true, "ttl": "1h", "expires_at": req + 3600, "recache_tokens_if_cold": 124500}});
        let sess = claude_session("x".into(), "n".into(), Activity::Idle, Some(&log), Some(&(tap, req + 10)), &s(vec![]));
        assert_eq!(sess.context, Some(123_005));
        assert_eq!(sess.cache, Cache::Known { ttl: 3600, expires_at: req + 3600, cached: 123_000,
            recache: Some(124_500), recache_at_least: false, exact: true });
    }

    #[test]
    fn tap_snapshot_after_compact() {
        let req = util::parse_iso("2026-10-17T19:00:00Z").unwrap();
        let tap = json!({"model": {"id": "claude-opus-5-5"}, "context_window": {"current_usage": null},
            "prompt_cache": {"ttl": "1h", "expires_at": req + 3600}});
        let sess = claude_session("x".into(), "n".into(), Activity::Idle, None, Some(&(tap, req)), &s(vec![]));
        assert!(matches!(sess.cache, Cache::NoData(_)), "no \"0 cached\"");
    }

    #[test]
    fn codex_config_keys() {
        let t = "model = \"gpt\"\nmodel_auto_compact_token_limit = 200_000 # note\n[profiles.x]\nmodel_auto_compact_token_limit = 5\n";
        assert_eq!(codex_compact_limit(t), Some(200_000));
        assert_eq!(codex_compact_limit("[a]\nmodel_auto_compact_token_limit = 5"), None);
        let with_profile = format!("profile = \"x\"\n{t}");
        assert_eq!(codex_compact_limit(&with_profile), Some(5));
        let missing_in_profile = "profile = \"y\"\nmodel_auto_compact_token_limit = 7\n[profiles.y]\nmodel = \"z\"\n";
        assert_eq!(codex_compact_limit(missing_in_profile), Some(7));
    }

    // ------------------------------------------------------------ collectors

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("quotatop-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempHome(p)
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
            p
        }

        fn collector(&self) -> Collector {
            Collector::at(self.0.join("claude"), self.0.join("codex"), self.0.join("tap"), Env::new())
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dead_pid() -> u32 {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    #[test]
    fn claude_collector() {
        let h = TempHome::new("claude");
        let cwd = h.0.join("proj");
        let me = std::process::id();
        let reg = |pid: u32, sid: &str| json!({"pid": pid, "sessionId": sid, "cwd": cwd, "kind": "interactive"}).to_string();
        h.write(&format!("claude/sessions/{me}.json"), &reg(me, "live"));
        h.write("claude/sessions/1.json", &reg(dead_pid(), "dead"));
        h.write("claude/sessions/2.json", &reg(me, "../../escape"));
        h.write("claude/sessions/3.json", &reg(0, "zero"));
        // The usage line sits before 2 MB of tool output, outside the first tail read.
        let folder: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let mut transcript = line(json!({"type": "user", "timestamp": "2026-10-17T19:00:00Z"})) + "\n";
        transcript += &(line(json!({"type": "assistant", "timestamp": "2026-10-17T19:00:05Z",
            "message": {"id": "a", "model": "claude-opus-5-5", "usage": {"input_tokens": 5, "cache_read_input_tokens": 1000,
                "cache_creation_input_tokens": 10, "cache_creation": {"ephemeral_1h_input_tokens": 10}}}})) + "\n");
        let filler = line(json!({"type": "attachment", "content": "x".repeat(1000)})) + "\n";
        transcript += &filler.repeat(2200);
        h.write(&format!("claude/projects/{folder}/live.jsonl"), &transcript);
        // The repo turns auto-compact off.
        h.write("proj/.claude/settings.json", r#"{"autoCompactEnabled": false}"#);

        let mut c = h.collector();
        let list = c.collect();
        assert_eq!(list.len(), 1, "{list:?}");
        let s = &list[0];
        assert_eq!(s.id, "live");
        assert_eq!(s.context, Some(1015));
        assert_eq!(s.compact_note.as_deref(), Some("off"));
        assert!(c.error.is_none());

        // A half-written registry file keeps the session for that poll.
        h.write(&format!("claude/sessions/{me}.json"), "{\"pid\": ");
        assert_eq!(c.collect().len(), 1);
    }

    #[test]
    fn codex_collector() {
        let h = TempHome::new("codex");
        let meta = |id: &str, source: Value| {
            line(json!({"type": "session_meta", "payload": {"id": id, "cwd": "/w/app", "originator": "codex_cli", "source": source}}))
        };
        let turn = line(json!({"type": "turn_context", "payload": {"model": "gpt-x"}}));
        let count = |input: u64, output: u64, total: u64| {
            line(json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
                "last_token_usage": {"input_tokens": input, "output_tokens": output, "total_tokens": total},
                "model_context_window": 475000}}}))
        };
        // Started 16 days ago, still written today.
        h.write("codex/sessions/2026/10/02/rollout-a.jsonl", &format!("{}\n{turn}\n{}\n", meta("a", json!("cli")), count(90, 10, 100_000)));
        h.write("codex/sessions/2026/10/18/rollout-g.jsonl",
            &format!("{}\n{turn}\n{}\n", meta("g", json!({"subagent": "guardian"})), count(5, 5, 10)));
        h.write("codex/sessions/2026/10/18/rollout-i.jsonl", &format!("{}\n{}\n", meta("i", json!("cli")), count(0, 0, 179_000)));
        h.write("codex/config.toml", "profile = \"p\"\n[profiles.p]\nmodel_auto_compact_token_limit = 300000\n");

        let mut c = h.collector();
        let list = c.collect();
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].id, "a");
        assert_eq!(list[0].context, Some(100_000));
        assert_eq!(list[0].compact_at, Some(300_000));
        assert_eq!(c.codex_hidden, 0);
    }

    #[test]
    fn settings_files_must_be_regular() {
        let h = TempHome::new("fifo");
        let p = h.0.join("settings.json");
        let ok = std::process::Command::new("mkfifo").arg(&p).status().is_ok_and(|s| s.success());
        if ok {
            assert!(util::read_limited(&p, SETTINGS_MAX).is_err(), "a FIFO must not block or be read");
        }
        let big = h.write("big.json", &" ".repeat(2 << 20));
        assert!(util::read_limited(&big, SETTINGS_MAX).is_err());
    }

    #[test]
    fn first_tail_read_keeps_a_whole_first_line() {
        let h = TempHome::new("tail");
        let p = h.write("log.jsonl", "aaaa\nbbbb\ncccc\n");
        // A cut right after a newline starts on a whole line.
        let (lines, off) = util::read_new_lines(&p, 0, 10).unwrap();
        assert_eq!(lines, ["bbbb", "cccc"]);
        assert_eq!(off, 15);
        // A cut inside a line drops the partial piece.
        let (lines, _) = util::read_new_lines(&p, 0, 8).unwrap();
        assert_eq!(lines, ["cccc"]);
    }
}
