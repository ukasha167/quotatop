//! Threads, schedule and the event loop.
//!
//! One thread per CLI fetches limits on its own 60 s schedule (staggered by 20 s so the
//! CLIs' memory peaks don't overlap), one thread polls session logs every 5 s, and one
//! thread blocks on keyboard input. The main thread sleeps until one of them sends an
//! event or the screen needs a redraw for a countdown or spinner.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::text::Span;

use crate::limits;
use crate::model::{Activity, Limits, Provider, State};
use crate::sessions::Collector;
use crate::ui::{self, UiState};
use crate::util;

pub struct Config {
    pub interval: u64,
    pub idle_backoff: bool,
}

enum Ev {
    Key(event::KeyEvent),
    Redraw,
}

type Shared = Arc<Mutex<State>>;

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

    /// Sleeps until `deadline` or a refresh request. Returns true on a refresh request.
    fn wait_until(&self, deadline: Instant, seen: u64) -> bool {
        let mut g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *g != seen {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            g = self.cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

fn fetch(p: Provider, bin: &PathBuf) -> Result<Limits, String> {
    match p {
        Provider::Claude => limits::fetch_claude(bin),
        Provider::Codex => limits::fetch_codex(bin),
        Provider::Agy => limits::fetch_agy(bin),
    }
}

fn find_bin(p: Provider) -> Option<PathBuf> {
    util::which(p.bin(), &format!("QUOTATOP_{}", p.bin().to_uppercase()))
}

fn limit_worker(p: Provider, state: Shared, tx: Sender<Ev>, refresh: Arc<Refresh>, offset: u64, cfg: Arc<Config>) {
    let Some(bin) = find_bin(p) else {
        state.lock().unwrap_or_else(|e| e.into_inner()).reading_mut(p).missing = true;
        let _ = tx.send(Ev::Redraw);
        return;
    };
    let mut seen = refresh.current();
    let mut next = Instant::now();
    let mut first = true;
    loop {
        if refresh.wait_until(next, seen) {
            seen = refresh.current();
        }
        let started = Instant::now();
        state.lock().unwrap_or_else(|e| e.into_inner()).reading_mut(p).loading = true;
        let _ = tx.send(Ev::Redraw);
        let result = fetch(p, &bin);
        {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
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
        }
        let _ = tx.send(Ev::Redraw);
        let mut interval = cfg.interval;
        if cfg.idle_backoff {
            let last = state.lock().unwrap_or_else(|e| e.into_inner()).last_local_activity;
            if last.is_none_or(|t| util::now() - t > 600) {
                interval = interval.max(300);
            }
        }
        // The first refresh after startup is offset so the three CLIs don't run together.
        let wait = interval + if first { offset } else { 0 };
        first = false;
        next = started + Duration::from_secs(wait);
    }
}

fn session_worker(state: Shared, tx: Sender<Ev>, refresh: Arc<Refresh>) {
    let mut collector = Collector::default();
    let mut seen = refresh.current();
    loop {
        let list = collector.collect();
        let now = util::now();
        let last = list
            .iter()
            .filter_map(|s| match s.activity {
                Activity::Working => Some(now),
                Activity::LastActive(t) => Some(t),
                Activity::Idle => s.last_request,
            })
            .max();
        let changed = {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            let changed = st.sessions != list || st.sessions_at.is_none();
            st.sessions = list;
            st.sessions_at = Some(now);
            st.last_local_activity = last;
            changed
        };
        if changed {
            let _ = tx.send(Ev::Redraw);
        }
        if refresh.wait_until(Instant::now() + Duration::from_secs(5), seen) {
            seen = refresh.current();
        }
    }
}

fn input_worker(tx: Sender<Ev>) {
    loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if tx.send(Ev::Key(k)).is_err() {
                    break;
                }
            }
            Ok(Event::Resize(..)) => {
                if tx.send(Ev::Redraw).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

pub fn run(cfg: Config) -> std::io::Result<()> {
    crate::tap::clean_spool();
    let cfg = Arc::new(cfg);
    let state: Shared = Arc::default();
    let refresh: Arc<Refresh> = Arc::default();
    let (tx, rx) = mpsc::channel::<Ev>();
    for (i, p) in Provider::ALL.into_iter().enumerate() {
        let (s, t, r, c) = (state.clone(), tx.clone(), refresh.clone(), cfg.clone());
        thread::spawn(move || limit_worker(p, s, t, r, i as u64 * 20, c));
    }
    {
        let (s, t, r) = (state.clone(), tx.clone(), refresh.clone());
        thread::spawn(move || session_worker(s, t, r));
    }

    let mut terminal = ratatui::try_init()?;
    {
        let t = tx.clone();
        thread::spawn(move || input_worker(t));
    }
    let ui_state = UiState { interval: cfg.interval };
    let mut scroll = 0usize;
    let mut hidden = 0usize;
    let result = loop {
        let now_ms = util::now_ms();
        let screen = {
            let st = state.lock().unwrap_or_else(|e| e.into_inner());
            let cols = terminal.size().map(|s| s.width).unwrap_or(80);
            ui::build(&st, &ui_state, now_ms, cols)
        };
        if let Err(e) = terminal.draw(|f| hidden = ui::draw(f, &screen, scroll)) {
            break Err(e);
        }
        scroll = scroll.min(hidden);
        // Sleep until the next event, or until something on screen changes on its own.
        let wait = Duration::from_millis(screen.redraw_ms - now_ms as u64 % screen.redraw_ms);
        match rx.recv_timeout(wait) {
            Ok(Ev::Key(k)) => match k.code {
                KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break Ok(()),
                KeyCode::Char('r') => refresh.bump(),
                KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 1).min(hidden),
                KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => scroll = (scroll + 9).min(hidden),
                KeyCode::PageUp => scroll = scroll.saturating_sub(9),
                _ => {}
            },
            Ok(Ev::Redraw) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
    };
    ratatui::restore();
    util::kill_children();
    result
}

/// `--once`: fetch everything, print one plain-text screen, exit.
pub fn once(width: u16) -> std::io::Result<()> {
    let state: Shared = Arc::default();
    let handles: Vec<_> = Provider::ALL
        .into_iter()
        .map(|p| {
            let s = state.clone();
            thread::spawn(move || {
                let r = match find_bin(p) {
                    None => Err(None),
                    Some(bin) => fetch(p, &bin).map_err(Some),
                };
                let mut st = s.lock().unwrap_or_else(|e| e.into_inner());
                let reading = st.reading_mut(p);
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
    let sessions = Collector::default().collect();
    for h in handles {
        let _ = h.join();
    }
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    st.sessions = sessions;
    st.sessions_at = Some(util::now());
    let ui_state = UiState { interval: 60 };
    let screen = ui::build(&st, &ui_state, util::now_ms(), width);
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
    print!("{out}");
    Ok(())
}

