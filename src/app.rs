//! Threads, schedule and the event loop.
//!
//! One thread per CLI fetches limits on its own schedule. The three schedules share one
//! interval and sit 20 s apart so the CLIs' memory peaks don't overlap. One thread polls
//! session logs every 5 s, and one thread blocks on keyboard input. The main thread sleeps
//! until one of them sends an event or the screen needs a redraw for a countdown or spinner.
//!
//! Schedules use wall-clock time: on macOS `Instant` stops while the Mac sleeps, which would
//! leave readings stale for up to one interval after waking.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::text::Span;

use crate::limits;
use crate::model::{Activity, Limits, Provider, State};
use crate::sessions::Collector;
use crate::ui;
use crate::util;

pub struct Config {
    pub interval: u64,
    pub idle_backoff: bool,
}

enum Ev {
    Key(event::KeyEvent),
    Redraw,
    /// The keyboard can no longer be read, so q would not work.
    InputClosed,
}

type Shared = Arc<Mutex<State>>;

/// Seconds between the three CLIs' fetches.
const STAGGER: u64 = 20;
/// Interval used by --idle-backoff while no local session is active.
const IDLE_INTERVAL: u64 = 300;
/// Longest wait after repeated failures, unless the interval itself is longer.
const MAX_BACKOFF: u64 = 600;
/// A press of r within this many seconds of the last fetch start is ignored.
const MIN_MANUAL_GAP_MS: i64 = 10_000;

/// Wakes workers early when the user presses r.
#[derive(Default)]
struct Refresh {
    generation: Mutex<u64>,
    cv: Condvar,
}

impl Refresh {
    fn current(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.cv.notify_all();
    }

    /// Sleeps until the wall-clock `deadline_ms` or a refresh request. Returns true on a
    /// refresh request. It wakes every 5 s to re-read the clock, which jumps after sleep.
    fn wait_until(&self, deadline_ms: i64, seen: u64) -> bool {
        let mut g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *g != seen {
                return true;
            }
            let left = deadline_ms - util::now_ms();
            if left <= 0 {
                return false;
            }
            let slice = Duration::from_millis(left.min(5_000) as u64);
            g = self.cv.wait_timeout(g, slice).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// When one worker fetches next. Fetches fall on a grid that starts at launch plus the
/// worker's offset and steps by the interval, so a slow fetch, a failure or a press of r
/// never moves the worker off its 20 s slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    anchor_ms: i64,
    pub next_ms: i64,
    failures: u32,
    last_start_ms: Option<i64>,
}

impl Schedule {
    pub fn new(start_ms: i64, offset_s: u64) -> Self {
        let anchor_ms = start_ms + offset_s as i64 * 1000;
        Schedule { anchor_ms, next_ms: anchor_ms, failures: 0, last_start_ms: None }
    }

    /// r is ignored while a fetch started less than 10 s ago.
    pub fn accepts_manual(&self, now_ms: i64) -> bool {
        self.last_start_ms.is_none_or(|t| now_ms - t >= MIN_MANUAL_GAP_MS)
    }

    pub fn started(&mut self, now_ms: i64) {
        self.last_start_ms = Some(now_ms);
    }

    /// Plans the next fetch after one that started at `started_ms` and ended at `now_ms`.
    /// Each failure in a row doubles the wait, up to 10 minutes.
    pub fn finished(&mut self, started_ms: i64, now_ms: i64, ok: bool, interval_s: u64) {
        let step = interval_s.max(1) as i64 * 1000;
        self.failures = if ok { 0 } else { self.failures.saturating_add(1) };
        let earliest = if ok {
            now_ms + 1
        } else {
            let cap = MAX_BACKOFF.max(interval_s);
            let wait_s = interval_s.saturating_mul(1u64 << self.failures.min(20)).min(cap);
            (started_ms + wait_s as i64 * 1000).max(now_ms + 1)
        };
        // The first grid point at or after `earliest`.
        let k = (earliest - self.anchor_ms + step - 1).div_euclid(step);
        self.next_ms = self.anchor_ms + k * step;
    }
}

fn fetch(p: Provider, bin: &PathBuf) -> Result<Limits, String> {
    match p {
        Provider::Claude => limits::fetch_claude(bin),
        Provider::Codex => limits::fetch_codex(bin),
        Provider::Agy => limits::fetch_agy(bin),
    }
}

/// Runs `f`, turning a panic into an error so one bad reading cannot stop a worker.
fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        Err(format!("quotatop bug: {}", util::shorten(&msg, 80)))
    })
}

fn find_bin(p: Provider) -> Option<PathBuf> {
    util::which(p.bin(), &format!("QUOTATOP_{}", p.bin().to_uppercase()))
}

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn limit_worker(p: Provider, state: Shared, tx: Sender<Ev>, refresh: Arc<Refresh>, offset: u64, cfg: Arc<Config>) {
    let mut seen = refresh.current();
    // A CLI installed after launch is found on the next minute or the next press of r.
    let bin = loop {
        if let Some(b) = find_bin(p) {
            break b;
        }
        lock(&state).reading_mut(p).missing = true;
        let _ = tx.send(Ev::Redraw);
        if refresh.wait_until(util::now_ms() + 60_000, seen) {
            seen = refresh.current();
        }
    };
    let mut sched = Schedule::new(util::now_ms(), offset);
    {
        let mut st = lock(&state);
        let r = st.reading_mut(p);
        r.missing = false;
        r.interval = cfg.interval;
        r.next_at = Some(sched.next_ms / 1000);
    }
    let _ = tx.send(Ev::Redraw);
    loop {
        while refresh.wait_until(sched.next_ms, seen) {
            seen = refresh.current();
            if sched.accepts_manual(util::now_ms()) {
                break;
            }
        }
        let started = util::now_ms();
        sched.started(started);
        lock(&state).reading_mut(p).loading = true;
        let _ = tx.send(Ev::Redraw);
        let result = guarded(|| fetch(p, &bin));
        let ok = result.is_ok();
        let mut interval = cfg.interval;
        {
            let mut st = lock(&state);
            if cfg.idle_backoff && st.last_local_activity.is_none_or(|t| util::now() - t > 600) {
                interval = interval.max(IDLE_INTERVAL);
            }
            let r = st.reading_mut(p);
            r.loading = false;
            match result {
                Ok(d) => {
                    r.data = Some(d);
                    r.fetched_at = Some(util::now());
                    r.error = None;
                }
                Err(e) => r.error = Some(e),
            }
            sched.finished(started, util::now_ms(), ok, interval);
            r.interval = interval;
            r.next_at = Some(sched.next_ms / 1000);
        }
        // Presses of r during the fetch are already answered by it.
        seen = refresh.current();
        let _ = tx.send(Ev::Redraw);
    }
}

fn session_worker(state: Shared, tx: Sender<Ev>, refresh: Arc<Refresh>) {
    let mut collector = Collector::new();
    let mut seen = refresh.current();
    loop {
        let collected = guarded(|| Ok(collector.collect()));
        let now = util::now();
        let changed = {
            let mut st = lock(&state);
            let before = (st.sessions.clone(), st.sessions_error.clone(), st.codex_hidden);
            match collected {
                Ok(list) => {
                    st.last_local_activity = list
                        .iter()
                        .filter_map(|s| match s.activity {
                            Activity::Working => Some(now),
                            Activity::LastActive(t) => Some(t),
                            Activity::Idle => s.last_request,
                        })
                        .max();
                    st.sessions = list;
                    st.sessions_error = collector.error.clone();
                    st.codex_hidden = collector.codex_hidden;
                }
                Err(e) => {
                    // The panic may have left the collector half-updated: start over.
                    collector = Collector::new();
                    st.sessions_error = Some(e);
                }
            }
            let changed =
                before != (st.sessions.clone(), st.sessions_error.clone(), st.codex_hidden) || st.sessions_at.is_none();
            st.sessions_at = Some(now);
            changed
        };
        if changed {
            let _ = tx.send(Ev::Redraw);
        }
        if refresh.wait_until(util::now_ms() + 5_000, seen) {
            seen = refresh.current();
        }
    }
}

fn input_worker(tx: Sender<Ev>) {
    loop {
        let ev = match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => Ev::Key(k),
            Ok(Event::Resize(..)) => Ev::Redraw,
            Ok(_) => continue,
            Err(_) => Ev::InputClosed,
        };
        let closed = matches!(ev, Ev::InputClosed);
        if tx.send(ev).is_err() || closed {
            break;
        }
    }
}

// ------------------------------------------------------------ signals

/// Set by SIGTERM, SIGHUP or SIGINT so the main loop can restore the terminal before exiting.
static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn catch_signals() {
    for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        unsafe {
            libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
    }
}

#[cfg(not(unix))]
fn catch_signals() {}

/// ratatui's panic hook restores the terminal on a panic in any thread. Worker panics are
/// caught and shown as errors, so only a panic on the main thread may restore and print.
fn main_thread_panics_only() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if thread::current().name() == Some("main") {
            prev(info);
        }
    }));
}

pub fn run(cfg: Config) -> std::io::Result<()> {
    crate::tap::clean_spool();
    util::clean_run_dir();
    catch_signals();
    let cfg = Arc::new(cfg);
    let state: Shared = Arc::default();
    let refresh: Arc<Refresh> = Arc::default();
    let (tx, rx) = mpsc::channel::<Ev>();

    let mut terminal = ratatui::try_init()?;
    main_thread_panics_only();
    for (i, p) in Provider::ALL.into_iter().enumerate() {
        let (s, t, r, c) = (state.clone(), tx.clone(), refresh.clone(), cfg.clone());
        thread::spawn(move || limit_worker(p, s, t, r, i as u64 * STAGGER, c));
    }
    {
        let (s, t, r) = (state.clone(), tx.clone(), refresh.clone());
        thread::spawn(move || session_worker(s, t, r));
    }
    {
        let t = tx.clone();
        thread::spawn(move || input_worker(t));
    }
    let mut scroll = 0usize;
    let mut hidden = 0usize;
    let result = 'main: loop {
        let now_ms = util::now_ms();
        let screen = {
            let st = lock(&state);
            let cols = terminal.size().map(|s| s.width).unwrap_or(80);
            ui::build(&st, now_ms, cols)
        };
        if let Err(e) = terminal.draw(|f| hidden = ui::draw(f, &screen, scroll)) {
            break Err(e);
        }
        scroll = scroll.min(hidden);
        // Sleep until the next event, or until something on screen changes on its own.
        // Wake every 250 ms to notice a signal.
        let deadline = now_ms + (screen.redraw_ms - now_ms as u64 % screen.redraw_ms) as i64;
        loop {
            if STOP.load(Ordering::SeqCst) {
                break 'main Ok(());
            }
            let left = deadline - util::now_ms();
            if left <= 0 {
                continue 'main;
            }
            match rx.recv_timeout(Duration::from_millis(left.min(250) as u64)) {
                Ok(Ev::Key(k)) => {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break 'main Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break 'main Ok(()),
                        KeyCode::Char('r') => refresh.bump(),
                        KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 1).min(hidden),
                        KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                        KeyCode::PageDown => scroll = (scroll + 9).min(hidden),
                        KeyCode::PageUp => scroll = scroll.saturating_sub(9),
                        _ => {}
                    }
                    continue 'main;
                }
                Ok(Ev::Redraw) => continue 'main,
                Ok(Ev::InputClosed) | Err(RecvTimeoutError::Disconnected) => break 'main Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    };
    ratatui::restore();
    util::kill_children();
    result
}

/// `--once`: fetch everything, print one plain-text screen, exit.
pub fn once(width: u16) -> std::io::Result<()> {
    util::clean_run_dir();
    let state: Shared = Arc::default();
    let handles: Vec<_> = Provider::ALL
        .into_iter()
        .map(|p| {
            let s = state.clone();
            thread::spawn(move || {
                let r = match find_bin(p) {
                    None => Err(None),
                    Some(bin) => guarded(|| fetch(p, &bin)).map_err(Some),
                };
                let mut st = lock(&s);
                let reading = st.reading_mut(p);
                reading.interval = 60;
                match r {
                    Ok(d) => {
                        reading.data = Some(d);
                        reading.fetched_at = Some(util::now());
                    }
                    Err(None) => reading.missing = true,
                    Err(Some(e)) => reading.error = Some(e),
                }
            })
        })
        .collect();
    let mut collector = Collector::new();
    let sessions = collector.collect();
    for h in handles {
        let _ = h.join();
    }
    let mut st = lock(&state);
    st.sessions = sessions;
    st.sessions_error = collector.error.clone();
    st.codex_hidden = collector.codex_hidden;
    st.sessions_at = Some(util::now());
    let screen = ui::build(&st, util::now_ms(), width);
    let height = screen.full_height();
    // The in-memory backend cannot fail.
    let Ok(mut term) = Terminal::new(TestBackend::new(width, height));
    let _ = term.draw(|f| {
        ui::draw(f, &screen, 0);
    });
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        let mut line = String::new();
        let mut x = 0;
        while x < width {
            let cell = &buf[(x, y)];
            line.push_str(cell.symbol());
            x += Span::raw(cell.symbol()).width().max(1) as u16;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    // `quotatop --once | head` closes the pipe early; that is not an error.
    use std::io::Write;
    match std::io::stdout().lock().write_all(out.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1000;

    #[test]
    fn first_fetch_is_offset() {
        let s = Schedule::new(0, 40);
        assert_eq!(s.next_ms, 40 * S);
    }

    #[test]
    fn slow_fetch_keeps_the_slot() {
        // Codex's slot is 20 s past each minute. A fetch that hangs for 90 s and fails
        // waits two intervals, then lands back on the slot.
        let mut s = Schedule::new(0, 20);
        s.started(20 * S);
        s.finished(20 * S, 110 * S, false, 60);
        assert_eq!(s.next_ms, 140 * S);
        assert!(s.next_ms > 110 * S, "never relaunched at once");
        s.started(140 * S);
        s.finished(140 * S, 143 * S, true, 60);
        assert_eq!(s.next_ms, 200 * S);
    }

    #[test]
    fn manual_refresh_does_not_move_the_slot() {
        let mut s = Schedule::new(0, 20);
        s.started(100 * S);
        s.finished(100 * S, 104 * S, true, 60);
        assert_eq!(s.next_ms, 140 * S, "still on 20, 80, 140, 200...");
    }

    #[test]
    fn failures_back_off_to_ten_minutes() {
        let mut s = Schedule::new(0, 0);
        let mut t = 0;
        let mut waits = Vec::new();
        for _ in 0..6 {
            s.started(t);
            s.finished(t, t + S, false, 60);
            waits.push((s.next_ms - t) / S);
            t = s.next_ms;
        }
        assert_eq!(waits, [120, 240, 480, 600, 600, 600]);
        s.started(t);
        s.finished(t, t + S, true, 60);
        assert_eq!((s.next_ms - t) / S, 60);
    }

    #[test]
    fn manual_refresh_needs_a_gap() {
        let mut s = Schedule::new(0, 0);
        s.started(50 * S);
        assert!(!s.accepts_manual(55 * S));
        assert!(s.accepts_manual(60 * S));
    }

    #[test]
    fn panics_become_errors() {
        let r: Result<(), String> = guarded(|| panic!("boom"));
        assert_eq!(r, Err("quotatop bug: boom".into()));
    }
}
