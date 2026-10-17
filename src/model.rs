//! Data shared between the collectors and the screen.

/// One limit window, such as "5-hour" or "Weekly".
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub label: String,
    /// Percent used, 0 to 100. None when the CLI did not say.
    pub used: Option<f64>,
    pub resets_at: Option<i64>,
    /// Window length in seconds, when known. Used for "week 45% gone".
    pub length: Option<i64>,
    /// The window has not begun: nothing used, and the reset time is "now + length".
    pub not_started: bool,
}

/// A group of windows that share one quota. Antigravity has two.
#[derive(Clone, Debug, PartialEq)]
pub struct Pool {
    /// None for a provider with a single pool.
    pub name: Option<String>,
    /// Short name for the "Most room" line.
    pub short: String,
    pub detail: Option<String>,
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub plan: Option<String>,
    pub pools: Vec<Pool>,
    pub notes: Vec<String>,
}

/// The latest reading from one CLI, with its age and the last error.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    pub data: Option<Limits>,
    pub fetched_at: Option<i64>,
    pub loading: bool,
    pub error: Option<String>,
    /// The CLI is not installed.
    pub missing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
    Agy,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Agy];

    pub fn title(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Agy => "Antigravity",
        }
    }

    pub fn bin(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Agy => "agy",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Codex,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Activity {
    Working,
    Idle,
    /// Last write to the session log, for tools with no live process list.
    LastActive(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cache {
    /// The tool does not report cache data (Codex).
    NotReported,
    /// No API response yet, or the session was just compacted.
    NoData(String),
    Known {
        ttl: i64,
        expires_at: i64,
        /// Tokens in the cached prefix after the last request.
        cached: u64,
        /// Tokens the next request re-caches if the cache is cold.
        recache: Option<u64>,
        /// `recache` is a lower bound rather than Claude Code's own figure.
        recache_at_least: bool,
        /// From Claude Code's status line (exact) rather than worked out from the transcript.
        exact: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub tool: Tool,
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub activity: Activity,
    pub last_request: Option<i64>,
    pub context: Option<u64>,
    pub window: Option<u64>,
    pub compact_at: Option<u64>,
    pub compact_approx: bool,
    pub compact_note: Option<String>,
    pub cache: Cache,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub claude: Reading,
    pub codex: Reading,
    pub agy: Reading,
    pub sessions: Vec<Session>,
    pub sessions_at: Option<i64>,
    pub sessions_error: Option<String>,
    /// Newest activity across all local sessions, for --idle-backoff.
    pub last_local_activity: Option<i64>,
}

impl State {
    pub fn reading(&self, p: Provider) -> &Reading {
        match p {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
            Provider::Agy => &self.agy,
        }
    }

    pub fn reading_mut(&mut self, p: Provider) -> &mut Reading {
        match p {
            Provider::Claude => &mut self.claude,
            Provider::Codex => &mut self.codex,
            Provider::Agy => &mut self.agy,
        }
    }
}

/// Marks a window as not started when nothing is used and the reset sits one full
/// window length after the fetch. Antigravity reports unused windows this way.
pub fn mark_not_started(w: &mut Window, fetched_at: i64) {
    if let (Some(used), Some(reset), Some(len)) = (w.used, w.resets_at, w.length) {
        if used == 0.0 && (reset - (fetched_at + len)).abs() <= 30 {
            w.not_started = true;
        }
    }
}
