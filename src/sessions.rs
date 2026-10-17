//! Running sessions: context use, auto-compact point and prompt cache state.
//!
//! Claude: the live process list in ~/.claude/sessions, the end of each transcript, and,
//! for terminal sessions, the status line snapshot that `quotatop tap` saves.
//! Codex: rollout logs in ~/.codex/sessions written in the last few hours.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::{Activity, Cache, Session, Tool};
use crate::util;

/// Bytes read from the end of a log the first time it is seen.
const FIRST_READ: u64 = 2 * 1024 * 1024;
/// Codex sessions written in the last this-many seconds are listed.
const CODEX_RECENT: i64 = 6 * 3600;
/// Default auto-compact point for models with a native 1M window, from the Claude Code docs
/// ("at about 967K tokens by default").
const CLAUDE_1M_DEFAULT_COMPACT: u64 = 967_000;

#[derive(Default)]
pub struct Collector {
    claude_logs: HashMap<String, ClaudeLog>,
    transcript_paths: HashMap<String, PathBuf>,
    codex_logs: HashMap<PathBuf, CodexLog>,
    codex_names: (Option<i64>, HashMap<String, String>),
}

impl Collector {
    /// Returns sessions sorted with working ones first, then by last activity.
    pub fn collect(&mut self) -> Vec<Session> {
        let now = util::now();
        let mut out = self.claude_sessions();
        out.extend(self.codex_sessions(now));
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

    fn claude_sessions(&mut self) -> Vec<Session> {
        let dir = util::claude_dir();
        let Ok(entries) = std::fs::read_dir(dir.join("sessions")) else { return Vec::new() };
        let user_settings = read_json(&dir.join("settings.json"));
        let mut out = Vec::new();
        let mut seen = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(reg) = read_json(&p) else { continue };
            let Some(pid) = reg.get("pid").and_then(Value::as_u64) else { continue };
            // Only interactive sessions; `claude -p` runs (ours included) are not listed.
            if reg.get("kind").and_then(Value::as_str).is_some_and(|k| k != "interactive") {
                continue;
            }
            if !util::pid_alive(pid as u32) {
                continue;
            }
            let Some(sid) = reg.get("sessionId").and_then(Value::as_str).map(str::to_string) else { continue };
            seen.push(sid.clone());
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

            let log = self.claude_log(&dir, &sid);
            let tap = read_tap(&sid);
            let settings = merged_settings(user_settings.as_ref(), &cwd);
            out.push(claude_session(sid, name, activity, log, tap.as_ref(), &settings));
        }
        self.claude_logs.retain(|k, _| seen.contains(k));
        out
    }

    fn claude_log(&mut self, dir: &Path, sid: &str) -> Option<&ClaudeLog> {
        if !self.transcript_paths.contains_key(sid) {
            let file = format!("{sid}.jsonl");
            let found = std::fs::read_dir(dir.join("projects"))
                .ok()?
                .flatten()
                .map(|d| d.path().join(&file))
                .find(|p| p.is_file())?;
            self.transcript_paths.insert(sid.to_string(), found);
        }
        let path = self.transcript_paths.get(sid)?.clone();
        let log = self.claude_logs.entry(sid.to_string()).or_insert_with(|| ClaudeLog::new(path));
        log.update();
        Some(log)
    }

    // -------------------------------------------------------- Codex

    fn codex_sessions(&mut self, now: i64) -> Vec<Session> {
        let home = util::codex_dir();
        let root = home.join("sessions");
        if !root.is_dir() {
            return Vec::new();
        }
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap_or_default();
        let compact_limit = toml_top_level_u64(&config, "model_auto_compact_token_limit");
        let models = read_json(&home.join("models_cache.json"));
        self.refresh_codex_names(&home.join("session_index.jsonl"));

        let mut files = Vec::new();
        let today = chrono::Local::now().date_naive();
        for back in 0..2 {
            let Some(d) = today.checked_sub_days(chrono::Days::new(back)) else { continue };
            let dir = root.join(d.format("%Y").to_string()).join(d.format("%m").to_string()).join(d.format("%d").to_string());
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let is_rollout = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-"));
                if is_rollout && util::mtime(&p).is_some_and(|m| now - m < CODEX_RECENT) {
                    files.push(p);
                }
            }
        }
        self.codex_logs.retain(|k, _| files.contains(k));
        let mut out = Vec::new();
        for f in files {
            let log = self.codex_logs.entry(f.clone()).or_insert_with(|| CodexLog::new(f));
            log.update();
            let Some(tok) = log.tokens.as_ref() else { continue };
            if tok.total == 0 {
                continue;
            }
            let model = log.model.clone();
            let window = tok.window.or_else(|| codex_window(models.as_ref(), model.as_deref()));
            let model_limit = model.as_deref().and_then(|m| codex_model_field(models.as_ref(), m, "auto_compact_token_limit"));
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
                model: model.map(|m| codex_display_name(models.as_ref(), &m)),
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
        out.sort_by_key(|s| std::cmp::Reverse(s.last_request));
        out.truncate(6);
        out
    }

    fn refresh_codex_names(&mut self, path: &Path) {
        let m = util::mtime(path);
        if m.is_none() || m == self.codex_names.0 {
            return;
        }
        let mut names = HashMap::new();
        for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if let (Some(id), Some(n)) = (v.get("id").and_then(Value::as_str), v.get("thread_name").and_then(Value::as_str)) {
                names.insert(id.to_string(), n.to_string());
            }
        }
        self.codex_names = (m, names);
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
            if first && !lines.iter().any(|l| l.contains("\"usage\"")) && !self.scanned {
                // Large tool results can push the last response out of the tail: read it all once.
                self.scanned = true;
                let path = self.path.clone();
                scan_lines(&path, off, |l| self.feed(l));
            } else {
                for l in &lines {
                    self.feed(l);
                }
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
            let cc = u.get("cache_creation");
            let w1h = cc.and_then(|c| c.get("ephemeral_1h_input_tokens")).and_then(Value::as_u64).unwrap_or(0);
            let w5m = cc.and_then(|c| c.get("ephemeral_5m_input_tokens")).and_then(Value::as_u64).unwrap_or(0);
            if w1h > 0 {
                self.ttl = Some(3600);
            } else if w5m > 0 {
                self.ttl = Some(300);
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

    let mut window = model.as_deref().and_then(|m| claude_window(m, settings));
    let mut context = log.filter(|l| l.has_usage && !l.compacted).map(|l| l.input + l.cache_read + l.cache_write);
    if let Some((v, _)) = tap {
        if let Some(w) = v.pointer("/context_window/context_window_size").and_then(Value::as_u64) {
            window = Some(w);
        }
        if let Some(cu) = v.pointer("/context_window/current_usage").filter(|c| !c.is_null()) {
            let n = |k: &str| cu.get(k).and_then(Value::as_u64).unwrap_or(0);
            context = Some(n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens"));
        }
    }
    let (compact_at, compact_approx, compact_note) = claude_compact(model.as_deref(), window, settings);

    let cache = match (tap.and_then(|(v, _)| v.get("prompt_cache").filter(|c| !c.is_null())), log) {
        (Some(pc), _) => {
            let cu = tap.and_then(|(v, _)| v.pointer("/context_window/current_usage").filter(|c| !c.is_null()));
            let cached = cu
                .map(|c| {
                    let n = |k: &str| c.get(k).and_then(Value::as_u64).unwrap_or(0);
                    n("cache_read_input_tokens") + n("cache_creation_input_tokens")
                })
                .unwrap_or(0);
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
        (None, Some(l)) if l.compacted => Cache::NoData("compacted; known after the next message".into()),
        (None, Some(l)) if l.has_usage => {
            let cached = l.cache_read + l.cache_write;
            match (l.ttl, l.request_ms) {
                (Some(ttl), Some(req)) if cached > 0 => Cache::Known {
                    ttl,
                    expires_at: req / 1000 + ttl,
                    cached,
                    // The next request re-sends at least the prefix that is cached now.
                    recache: Some(cached),
                    recache_at_least: true,
                    exact: false,
                },
                _ => Cache::NoData("the last response used no cache".into()),
            }
        }
        _ => Cache::NoData("no messages yet".into()),
    };
    Session {
        tool: Tool::Claude,
        id: sid,
        name,
        model: model.map(|m| claude_display_name(&m)),
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

/// Context window by model, from the Claude Code model docs: Fable 5+, Sonnet 5+, Haiku 5.5+
/// and Opus 4.7+ run with 1M by default; older models with 200K unless they carry `[1m]`.
pub fn claude_window(id: &str, settings: &Settings) -> Option<u64> {
    if let Some(n) = settings.env("CLAUDE_CODE_MAX_CONTEXT_TOKENS").and_then(|v| v.parse::<u64>().ok()) {
        return Some(n);
    }
    let disable_1m = settings.env("CLAUDE_CODE_DISABLE_1M_CONTEXT").is_some_and(|v| v == "1" || v == "true");
    let one_m = if disable_1m { 200_000 } else { 1_000_000 };
    if id.contains("[1m]") {
        return Some(one_m);
    }
    let base = id.trim_start_matches("claude-");
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
    Some(if big { one_m } else { 200_000 })
}

/// Returns (compact point, approximate?, note).
pub fn claude_compact(model: Option<&str>, window: Option<u64>, s: &Settings) -> (Option<u64>, bool, Option<String>) {
    if s.env("DISABLE_AUTO_COMPACT").is_some_and(|v| v == "1" || v == "true") {
        return (None, false, Some("off".into()));
    }
    let Some(window) = window else { return (None, false, None) };
    let env_window = s
        .env("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.clamp(100_000, 1_000_000));
    let model_window = model.and_then(|m| {
        let ms = s.value("modelSettings")?;
        let entry = ms.get(m).or_else(|| ms.get(m.trim_start_matches("claude-")))?;
        entry.get("autoCompactWindow").and_then(Value::as_u64)
    });
    let top_window = s.value("autoCompactWindow").and_then(Value::as_u64);
    let set = env_window.or(model_window).or(top_window).map(|w| w.min(window));
    let (base, approx) = match set {
        Some(w) => (w, false),
        None if window >= 1_000_000 => (CLAUDE_1M_DEFAULT_COMPACT, true),
        None => (window, true),
    };
    match s.env("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE").and_then(|v| v.parse::<u64>().ok()).filter(|p| (1..100).contains(p)) {
        Some(p) => (Some(base * p / 100), true, None),
        None => (Some(base), approx, None),
    }
}

// ------------------------------------------------------------ settings

/// Claude settings merged in precedence order: project local, project, user.
#[derive(Default)]
pub struct Settings {
    layers: Vec<Value>,
}

impl Settings {
    #[cfg(test)]
    pub fn from_layers(layers: Vec<Value>) -> Self {
        Settings { layers }
    }

    fn value(&self, key: &str) -> Option<&Value> {
        self.layers.iter().find_map(|l| l.get(key).filter(|v| !v.is_null()))
    }

    /// An env var from a settings file's "env" block, else from this process.
    fn env(&self, key: &str) -> Option<String> {
        self.layers
            .iter()
            .find_map(|l| l.get("env").and_then(|e| e.get(key)).and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }))
            .or_else(|| std::env::var(key).ok().filter(|v| !v.is_empty()))
    }
}

fn merged_settings(user: Option<&Value>, cwd: &str) -> Settings {
    let mut layers = Vec::new();
    if !cwd.is_empty() {
        let d = Path::new(cwd).join(".claude");
        layers.extend(read_json(&d.join("settings.local.json")));
        layers.extend(read_json(&d.join("settings.json")));
    }
    layers.extend(user.cloned());
    Settings { layers }
}

/// The status line snapshot saved by `quotatop tap`, with its file time.
fn read_tap(sid: &str) -> Option<(Value, i64)> {
    let p = util::spool_dir().join(format!("{sid}.json"));
    let m = util::mtime(&p)?;
    Some((read_json(&p)?, m))
}

// ------------------------------------------------------------ Codex rollout

#[derive(Clone, Debug, PartialEq)]
pub struct CodexTokens {
    pub total: u64,
    pub window: Option<u64>,
}

#[derive(Debug, Default)]
struct CodexLog {
    path: PathBuf,
    offset: u64,
    size: Option<(u64, i64)>,
    id: Option<String>,
    cwd: Option<String>,
    exec: bool,
    model: Option<String>,
    tokens: Option<CodexTokens>,
    last_write: Option<i64>,
    scanned: bool,
}

impl CodexLog {
    fn new(path: PathBuf) -> Self {
        CodexLog { path, ..Default::default() }
    }

    fn update(&mut self) {
        let Ok(meta) = std::fs::metadata(&self.path) else { return };
        let m = util::mtime(&self.path).unwrap_or(0);
        let key = (meta.len(), m);
        if self.size == Some(key) {
            return;
        }
        self.last_write = Some(m);
        if self.id.is_none() {
            // The first line is session_meta. It can be long (it holds the system prompt).
            if let Ok(f) = std::fs::File::open(&self.path) {
                use std::io::BufRead;
                let mut first = String::new();
                if std::io::BufReader::new(f).read_line(&mut first).is_ok() {
                    if let Ok(v) = serde_json::from_str::<Value>(&first) {
                        let p = v.get("payload");
                        self.id = p.and_then(|p| p.get("id")).and_then(Value::as_str).map(str::to_string);
                        self.cwd = p.and_then(|p| p.get("cwd")).and_then(Value::as_str).map(str::to_string);
                        let origin = p.and_then(|p| p.get("originator")).and_then(Value::as_str).unwrap_or("");
                        self.exec = origin.contains("exec");
                    }
                }
            }
        }
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
                let total = info.pointer("/last_token_usage/total_tokens").and_then(Value::as_u64).unwrap_or(0);
                let window = info.get("model_context_window").and_then(Value::as_u64);
                self.tokens = Some(CodexTokens { total, window });
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
    Some(w * pct / 100)
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

/// Reads `key = 123` from the top level of a TOML file, before any [table].
fn toml_top_level_u64(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            return None;
        }
        if let Some((k, v)) = l.split_once('=') {
            if k.trim() == key {
                let v = v.split('#').next()?.trim().replace('_', "");
                return v.parse().ok();
            }
        }
    }
    None
}

// ------------------------------------------------------------ helpers

/// Calls `f` for each complete line in the first `end` bytes of a file.
fn scan_lines(path: &Path, end: u64, mut f: impl FnMut(&str)) {
    use std::io::{BufRead, Read};
    let Ok(file) = std::fs::File::open(path) else { return };
    let mut reader = std::io::BufReader::new(file.take(end));
    let mut buf = Vec::new();
    while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
        if buf.last() == Some(&b'\n') {
            f(&String::from_utf8_lossy(&buf));
        }
        buf.clear();
    }
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn last_component(path: &str) -> String {
    Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(layers: Vec<Value>) -> Settings {
        Settings::from_layers(layers)
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
    fn display_names() {
        assert_eq!(claude_display_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(claude_display_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(claude_display_name("claude-sonnet-5"), "Sonnet 5");
    }

    #[test]
    fn compact_points() {
        let user = s(vec![json!({"modelSettings": {"claude-opus-5-5": {"autoCompactWindow": 500000}}})]);
        assert_eq!(claude_compact(Some("claude-opus-5-5"), Some(1_000_000), &user), (Some(500_000), false, None));
        let none = s(vec![]);
        assert_eq!(claude_compact(Some("claude-opus-5-5"), Some(1_000_000), &none), (Some(967_000), true, None));
        // A project setting beats the user one; the window caps it.
        let layered = s(vec![json!({"autoCompactWindow": 300000}), json!({"autoCompactWindow": 900000})]);
        assert_eq!(claude_compact(Some("claude-sonnet-4-6"), Some(200_000), &layered), (Some(200_000), false, None));
        let env = s(vec![json!({"env": {"CLAUDE_CODE_AUTO_COMPACT_WINDOW": "400000"}}), json!({"autoCompactWindow": 300000})]);
        assert_eq!(claude_compact(Some("claude-opus-5-5"), Some(1_000_000), &env).0, Some(400_000));
    }

    fn line(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn transcript_cache_timing() {
        let mut log = ClaudeLog::default();
        log.feed(&line(json!({"type": "user", "timestamp": "2026-10-17T19:26:38.614Z", "isSidechain": false,
            "message": {"content": [{"type": "tool_result", "content": "\"timestamp\":\"2030-01-01T00:00:00Z\""}]}})));
        // A response that took two minutes: two blocks of one message, a tool result between them.
        let usage = json!({"input_tokens": 2, "cache_creation_input_tokens": 2102, "cache_read_input_tokens": 103209,
            "output_tokens": 540, "cache_creation": {"ephemeral_1h_input_tokens": 2102, "ephemeral_5m_input_tokens": 0}});
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:28:52.181Z", "isSidechain": false,
            "message": {"id": "msg_1", "model": "claude-opus-5-5", "usage": usage}})));
        log.feed(&line(json!({"type": "user", "timestamp": "2026-10-17T19:28:52.467Z", "isSidechain": false})));
        log.feed(&line(json!({"type": "assistant", "timestamp": "2026-10-17T19:28:55.006Z", "isSidechain": false,
            "message": {"id": "msg_1", "model": "claude-opus-5-5", "usage": usage}})));
        // Subagent traffic is ignored.
        log.feed(&line(json!({"type": "user", "timestamp": "2026-10-17T19:40:00.000Z", "isSidechain": true})));
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
    fn toml_key() {
        let t = "model = \"gpt\"\nmodel_auto_compact_token_limit = 200_000 # note\n[profiles.x]\nmodel_auto_compact_token_limit = 5\n";
        assert_eq!(toml_top_level_u64(t, "model_auto_compact_token_limit"), Some(200_000));
        assert_eq!(toml_top_level_u64("[a]\nmodel_auto_compact_token_limit = 5", "model_auto_compact_token_limit"), None);
    }
}
