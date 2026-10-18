//! Builds the screen from the shared state. Every colour comes with a word or symbol.

use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::model::{Activity, Cache, Provider, Reading, Session, State, Tool, Window};
use crate::util::{self, tokens};

pub struct Section {
    pub title: Line<'static>,
    pub status: Line<'static>,
    pub body: Vec<Line<'static>>,
}

pub struct Screen {
    pub header: Vec<Line<'static>>,
    pub limits: Vec<Section>,
    pub sessions: Section,
    pub legend: Vec<Line<'static>>,
    /// The fastest anything on screen changes, in milliseconds.
    pub redraw_ms: u64,
}

impl Screen {
    pub fn full_height(&self) -> u16 {
        let sections: usize = self.limits.iter().map(|s| s.body.len() + 2).sum();
        (self.header.len() + sections + self.sessions.body.len() + 2 + self.legend.len()) as u16
    }
}

// ------------------------------------------------------------ styles

fn no_color() -> bool {
    static NC: OnceLock<bool> = OnceLock::new();
    *NC.get_or_init(|| std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()))
}

fn fg(c: Color) -> Style {
    if no_color() { Style::default() } else { Style::default().fg(c) }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

const GREEN: Color = Color::Green;
const YELLOW: Color = Color::Yellow;
const RED: Color = Color::Red;
const COLD: Color = Color::LightBlue;
const MARK: Color = Color::Magenta;
const TITLE: Color = Color::Cyan;

#[derive(Clone, Copy, PartialEq)]
enum Level {
    Ok,
    High,
    Full,
}

fn level(p: f64) -> Level {
    if p >= 90.0 {
        Level::Full
    } else if p >= 70.0 {
        Level::High
    } else {
        Level::Ok
    }
}

fn level_style(l: Level) -> Style {
    fg(match l {
        Level::Ok => GREEN,
        Level::High => YELLOW,
        Level::Full => RED,
    })
}

fn level_sym(l: Level) -> &'static str {
    match l {
        Level::Ok => "✓",
        Level::High => "▲",
        Level::Full => "✗",
    }
}

fn level_word(l: Level) -> &'static str {
    match l {
        Level::Ok => "ok",
        Level::High => "high",
        Level::Full => "full",
    }
}

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner(now_ms: i64) -> &'static str {
    SPIN[((now_ms / 250) % 10) as usize]
}

fn width(s: &str) -> usize {
    Span::raw(s).width()
}

fn pad(s: &str, n: usize) -> String {
    let w = width(s);
    if w >= n { s.to_string() } else { format!("{s}{}", " ".repeat(n - w)) }
}

fn fit(s: &str, n: usize) -> String {
    pad(&util::shorten(s, n), n)
}

/// A bar of `cells` cells with eighth-block precision.
fn bar(frac: f64, cells: usize, style: Style) -> Vec<Span<'static>> {
    const PART: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (frac.clamp(0.0, 1.0) * cells as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;
    let mut filled = "█".repeat(full);
    filled.push_str(PART[rem]);
    let used = full + usize::from(rem > 0);
    vec![Span::styled(filled, style), Span::styled("░".repeat(cells.saturating_sub(used)), dim())]
}

/// Context bar over the whole window, with ┃ where auto-compact triggers.
fn context_bar(ctx: u64, window: u64, compact: Option<u64>, cells: usize, style: Style) -> Vec<Span<'static>> {
    let mut spans = bar(ctx as f64 / window as f64, cells, style);
    let Some(c) = compact.filter(|c| *c < window) else { return spans };
    let mark = ((c as f64 / window as f64) * cells as f64).round() as usize;
    let mark = mark.min(cells - 1);
    // Rebuild cell by cell so the marker can replace one cell.
    let mut chars: Vec<(String, Style)> = Vec::with_capacity(cells);
    for s in spans.drain(..) {
        for ch in s.content.chars() {
            chars.push((ch.to_string(), s.style));
        }
    }
    if let Some(cell) = chars.get_mut(mark) {
        *cell = ("┃".to_string(), fg(MARK).add_modifier(Modifier::BOLD));
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    for (ch, st) in chars {
        match out.last_mut() {
            Some(last) if last.style == st => last.content.to_mut().push_str(&ch),
            _ => out.push(Span::styled(ch, st)),
        }
    }
    out
}

// ------------------------------------------------------------ screen

pub fn build(s: &State, now_ms: i64, cols: u16) -> Screen {
    let now = now_ms / 1000;
    let inner = (cols as usize).saturating_sub(2);
    let mut redraw_ms: u64 = 5_000;
    let mut fast = |ms: u64| redraw_ms = redraw_ms.min(ms);

    // Header
    let left = " AI subscriptions";
    let right = format!("{}  r refresh  q quit ", util::header_time(now));
    let gap = (cols as usize).saturating_sub(width(left) + width(&right)).max(1);
    let header = vec![
        Line::from(vec![Span::styled(left, bold().patch(fg(TITLE))), Span::raw(" ".repeat(gap)), Span::styled(right, dim())]),
        most_room(s, now, cols as usize),
    ];

    let mut limits = Vec::new();
    for p in Provider::ALL {
        let r = s.reading(p);
        if r.loading || (r.data.is_none() && r.error.is_none() && !r.missing) {
            fast(250);
        }
        limits.push(limit_section(p, r, now, now_ms, inner, &mut fast));
    }

    let sessions = sessions_section(s, now, inner, &mut fast);
    let legend = legend(cols as usize);
    Screen { header, limits, sessions, legend, redraw_ms }
}

fn limit_section(p: Provider, r: &Reading, now: i64, now_ms: i64, inner: usize, fast: &mut impl FnMut(u64)) -> Section {
    let mut title = format!(" {} ", p.title());
    if let Some(plan) = r.data.as_ref().and_then(|d| d.plan.as_ref()) {
        title = format!(" {} · {plan} ", p.title());
    }
    let spin = spinner(now_ms);
    let age = r.fetched_at.map(|t| now - t);
    let stale = age.is_some_and(|a| a > r.stale_after());
    // Before the first fetch: the CLIs start 20 s apart.
    let waiting = r.next_at.filter(|t| !r.loading && *t > now);
    let status = if r.missing {
        Line::styled(" not installed ", dim())
    } else {
        match (&r.data, &r.error) {
            (None, None) if waiting.is_some() => Line::styled(" waiting ", dim()),
            (None, None) => Line::styled(format!(" {spin} loading "), fg(YELLOW)),
            (None, Some(_)) if r.loading => Line::styled(format!(" {spin} trying again "), fg(YELLOW)),
            (None, Some(_)) => Line::styled(" ✗ no reading ", fg(RED)),
            (Some(_), err) => {
                let ago = util::ago(age.unwrap_or(0));
                let (text, style) = if stale {
                    (format!("⚠ stale · updated {ago}"), fg(YELLOW))
                } else if err.is_some() {
                    (format!("⚠ last try failed · updated {ago}"), fg(YELLOW))
                } else {
                    (format!("updated {ago}"), dim())
                };
                if r.loading { Line::styled(format!(" {spin} {text} "), style) } else { Line::styled(format!(" {text} "), style) }
            }
        }
    };

    let mut body = Vec::new();
    if r.missing {
        let var = format!("QUOTATOP_{}", p.bin().to_uppercase());
        body.push(Line::styled(format!(" `{}` was not found on PATH. Set {var} to its full path.", p.bin()), dim()));
    } else if let Some(d) = &r.data {
        let label_w = d
            .pools
            .iter()
            .flat_map(|p| p.windows.iter())
            .map(|w| width(&w.label))
            .max()
            .unwrap_or(7)
            .clamp(7, 12);
        let multi = d.pools.len() > 1;
        for pool in &d.pools {
            if let Some(name) = pool.name.as_ref().filter(|_| multi || pool.detail.is_some()) {
                let mut spans = vec![Span::raw(" "), Span::styled(name.clone(), bold())];
                if let Some(detail) = &pool.detail {
                    let room = inner.saturating_sub(width(name) + 4);
                    spans.push(Span::styled(format!(" ({})", util::shorten(detail, room.saturating_sub(3))), dim()));
                }
                body.push(Line::from(spans));
            }
            for w in &pool.windows {
                body.push(window_row(w, label_w, now, inner, fast));
            }
        }
        for n in &d.notes {
            let style = if n.starts_with('✗') { fg(RED) } else if n.starts_with('⚠') { fg(YELLOW) } else { Style::default() };
            body.push(Line::styled(format!(" {}", util::shorten(n, inner.saturating_sub(2))), style));
        }
        if let Some(e) = &r.error {
            body.push(Line::styled(format!(" ⚠ last try failed: {}", util::shorten(e, inner.saturating_sub(22))), fg(YELLOW)));
        }
    } else if let Some(e) = &r.error {
        body.push(Line::styled(format!(" ✗ could not read limits: {}", util::shorten(e, inner.saturating_sub(29))), fg(RED)));
        let next = match r.next_at {
            _ if r.loading => format!(" {spin} trying again"),
            Some(t) if t > now => format!(" next try at {} (in {})", util::clock(t), util::duration(t - now)),
            _ => " trying again shortly".to_string(),
        };
        body.push(Line::styled(next, dim()));
    } else if let Some(t) = waiting {
        body.push(Line::styled(format!(" first reading in {}; the CLIs start 20 s apart", util::duration(t - now)), dim()));
    } else {
        let secs = match p {
            Provider::Claude => "about 4 s",
            Provider::Codex => "about 3 s",
            Provider::Agy => "7 to 13 s",
        };
        body.push(Line::styled(format!(" {spin} asking {} for /usage ({secs})", p.bin()), fg(YELLOW)));
    }
    Section { title: Line::styled(title, bold().patch(fg(TITLE))), status: status.right_aligned(), body }
}

fn window_row(w: &Window, label_w: usize, now: i64, inner: usize, fast: &mut impl FnMut(u64)) -> Line<'static> {
    const RESET_W: usize = 34;
    let cells = inner.saturating_sub(1 + label_w + 1 + 1 + 4 + 1 + 7 + RESET_W).clamp(6, 20);
    let mut spans = vec![Span::raw(" "), Span::raw(pad(&w.label, label_w)), Span::raw(" ")];
    let Some(used) = w.used else {
        spans.push(Span::styled("? unknown: the CLI did not report this window", dim()));
        return Line::from(spans);
    };
    let l = level(used);
    spans.extend(bar(used / 100.0, cells, level_style(l)));
    spans.push(Span::raw(format!(" {:>4} ", util::pct(used))));
    spans.push(Span::styled(pad(&format!("{} {}", level_sym(l), level_word(l)), 6), level_style(l)));
    spans.push(Span::raw("  "));
    // Longest wording first; the first one that fits the remaining width wins.
    let room = inner.saturating_sub(spans.iter().map(|s| s.width()).sum::<usize>());
    let (options, style): (Vec<String>, Style) = if w.not_started {
        (vec!["not started, begins on first use".into(), "not started".into()], dim())
    } else {
        match w.resets_at {
            None => (vec!["reset time unknown".into(), "reset ?".into()], dim()),
            Some(t) if t <= now => (vec!["reset is due, waiting for a new reading".into(), "reset due".into()], fg(YELLOW)),
            Some(t) => {
                let left = t - now;
                if left < 600 {
                    fast(1_000);
                }
                if left < 86_400 {
                    let when = util::clock(t);
                    let dur = util::duration(left);
                    (vec![format!("resets {when} (in {dur})"), format!("resets {when}"), when], Style::default())
                } else {
                    let when = util::day_clock(t, now);
                    let mut v = Vec::new();
                    if let Some(len) = w.length.filter(|l| *l >= 86_400) {
                        let gone = ((now - (t - len)) as f64 / len as f64 * 100.0).clamp(0.0, 100.0);
                        let unit = if len == 7 * 86_400 { "week" } else { "window" };
                        v.push(format!("resets {when} · {unit} {gone:.0}% gone"));
                    }
                    v.push(format!("resets {when}"));
                    v.push(when);
                    (v, Style::default())
                }
            }
        }
    };
    let text = options.iter().find(|o| width(o) <= room).or(options.last()).cloned().unwrap_or_default();
    let reset = Span::styled(text, style);
    spans.push(reset);
    Line::from(spans)
}

fn most_room(s: &State, now: i64, cols: usize) -> Line<'static> {
    let mut items: Vec<(String, f64)> = Vec::new();
    for p in Provider::ALL {
        let r = s.reading(p);
        let Some(d) = &r.data else { continue };
        if r.fetched_at.is_none_or(|t| now - t > r.stale_after()) {
            continue;
        }
        for pool in &d.pools {
            // A per-model limit such as "Weekly Opus" only binds that model, not the pool.
            let used: Option<Vec<f64>> = pool
                .windows
                .iter()
                .filter(|w| !w.per_model)
                .map(|w| if w.not_started { Some(0.0) } else { w.used })
                .collect();
            if let Some(m) = used.and_then(|u| u.into_iter().reduce(f64::max)) {
                items.push((pool.short.clone(), m));
            }
        }
    }
    items.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut spans = vec![Span::styled(" Most room now: ", bold())];
    if items.is_empty() {
        spans.push(Span::styled("waiting for fresh readings", dim()));
        return Line::from(spans);
    }
    let mut used_w = width(" Most room now: ");
    for (i, (name, p)) in items.iter().enumerate() {
        let l = level(*p);
        let text = format!("{} {name} {}", level_sym(l), util::pct(*p));
        let sep = if i == 0 { 0 } else { 3 };
        let left = items.len() - i - 1;
        // Keep room for "· +N more" unless this is the last item.
        let more_w = if left > 0 { 3 + width(&format!("+{left} more")) } else { 0 };
        if used_w + sep + width(&text) + more_w > cols {
            let more = items.len() - i;
            spans.push(Span::styled(if i == 0 { format!("+{more} more") } else { format!(" · +{more} more") }, dim()));
            break;
        }
        if i > 0 {
            spans.push(Span::styled(" · ", dim()));
        }
        used_w += sep + width(&text);
        spans.push(Span::styled(text, level_style(l)));
    }
    Line::from(spans)
}

fn sessions_section(s: &State, now: i64, inner: usize, fast: &mut impl FnMut(u64)) -> Section {
    let mut body = Vec::new();
    for sess in &s.sessions {
        body.extend(session_lines(sess, now, inner, fast));
    }
    if s.sessions.is_empty() {
        let msg = match &s.sessions_at {
            None => " looking for sessions…".to_string(),
            Some(_) => " No running Claude sessions, and no Codex activity in the last 6 h.".to_string(),
        };
        body.push(Line::styled(msg, dim()));
    }
    if s.codex_hidden > 0 {
        let n = s.codex_hidden;
        body.push(Line::styled(format!(" +{n} more Codex session{} active in the last 6 h", if n == 1 { "" } else { "s" }), dim()));
    }
    if let Some(e) = &s.sessions_error {
        body.push(Line::styled(format!(" ⚠ {e}"), fg(YELLOW)));
    }
    if !s.agy.missing {
        let note = if inner >= 76 {
            " Antigravity sessions are not listed: agy keeps no readable context data."
        } else {
            " Antigravity sessions: not readable."
        };
        body.push(Line::styled(note, dim()));
    }
    let status = match s.sessions_at {
        Some(t) => Line::styled(format!(" updated {} ", util::ago(now - t)), dim()),
        None => Line::styled(" loading ", fg(YELLOW)),
    };
    Section { title: Line::styled(" Sessions ", bold().patch(fg(TITLE))), status: status.right_aligned(), body }
}

fn session_lines(s: &Session, now: i64, inner: usize, fast: &mut impl FnMut(u64)) -> Vec<Line<'static>> {
    let tool = match s.tool {
        Tool::Claude => "Claude",
        Tool::Codex => "Codex",
    };
    let (act, act_style) = match s.activity {
        Activity::Working => ("▶ working".to_string(), fg(GREEN)),
        Activity::Idle => ("‖ idle".to_string(), dim()),
        Activity::LastActive(t) => (format!("‖ {}", util::ago(now - t)), dim()),
    };
    let name_w = inner.saturating_sub(1 + 8 + 2 + 12 + 1 + 11).max(10);
    let first = Line::from(vec![
        Span::raw(" "),
        Span::styled(pad(tool, 8), bold()),
        Span::raw(fit(&s.name, name_w)),
        Span::raw("  "),
        Span::raw(fit(s.model.as_deref().unwrap_or("?"), 12)),
        Span::raw(" "),
        Span::styled(act, act_style),
    ]);

    // Context
    let mut ctx = vec![Span::styled("   context ", dim())];
    let compact_text = match (s.compact_at, s.compact_approx, s.compact_note.as_deref()) {
        (Some(c), false, _) => format!("auto-compacts at {}", tokens(c)),
        (Some(c), true, _) => format!("auto-compacts at ≈{}", tokens(c)),
        (None, _, Some("off")) => "auto-compact is off".to_string(),
        (None, _, Some(n)) => format!("compact point unknown ({n})"),
        (None, _, None) => "compact point unknown".to_string(),
    };
    match (s.context, s.window) {
        (Some(c), Some(w)) if w > 0 => {
            let limit = s.compact_at.unwrap_or(w).max(1);
            let l = level(c as f64 / limit as f64 * 100.0);
            let cells = inner.saturating_sub(11 + 1 + 12 + 3 + 25).clamp(8, 20);
            ctx.extend(context_bar(c, w, s.compact_at, cells, level_style(l)));
            ctx.push(Span::raw(format!(" {} ", pad(&format!("{} of {}", tokens(c), tokens(w)), 12))));
            ctx.push(Span::styled(level_sym(l), level_style(l)));
            ctx.push(Span::styled(format!("  {compact_text}"), dim()));
        }
        (Some(c), _) => {
            ctx.push(Span::raw(format!("{} used · window size unknown", tokens(c))));
            ctx.push(Span::styled(format!(" · {compact_text}"), dim()));
        }
        (None, _) => {
            let why = match &s.cache {
                Cache::NoData(r) => r.clone(),
                _ => "no reading yet".to_string(),
            };
            ctx.push(Span::styled(format!("? unknown: {why}"), dim()));
        }
    }

    // Cache
    let mut cache = vec![Span::styled("   cache   ", dim())];
    match &s.cache {
        Cache::NotReported => cache.push(Span::styled("n/a, Codex does not report cache expiry", dim())),
        Cache::NoData(why) => cache.push(Span::styled(format!("? {why}"), dim())),
        Cache::Known { ttl, expires_at, cached, recache, recache_at_least, exact } => {
            let approx = if *exact { "" } else { "≈" };
            let left = expires_at - now;
            // Long and short forms of the re-cache figure.
            let re = recache.map(|r| {
                let t = tokens(r);
                match (*recache_at_least, *exact) {
                    (true, _) => (format!("at least {t}"), format!("≥{t}")),
                    (false, false) => (format!("≈{t}"), format!("≈{t}")),
                    (false, true) => (t.clone(), t),
                }
            });
            // Longest wording first; the first one that fits the row wins, so a narrow
            // terminal drops whole phrases instead of cutting one in half.
            let mut options: Vec<Vec<Span<'static>>> = Vec::new();
            if left > 0 {
                if left < 600 {
                    fast(1_000);
                }
                let urgent = left < 300 || (*ttl > 0 && left.saturating_mul(5) < *ttl);
                let style = if urgent { fg(YELLOW) } else { fg(GREEN) };
                let state = Span::styled(format!("● warm, {approx}{} left", util::duration(left)), style);
                let size = Span::raw(format!("   {} cached", tokens(*cached)));
                if let Some((long, short)) = &re {
                    options.push(vec![state.clone(), size.clone(), Span::styled(format!("   if it goes cold: {long}"), dim())]);
                    options.push(vec![state.clone(), size.clone(), Span::styled(format!("   cold: {short}"), dim())]);
                }
                options.push(vec![state.clone(), size]);
                options.push(vec![state]);
            } else {
                let since = if now - expires_at < 86_400 { util::clock(*expires_at) } else { util::date(*expires_at) };
                let state = Span::styled(format!("○ cold since {approx}{since}"), fg(COLD));
                if let Some((long, short)) = &re {
                    options.push(vec![state.clone(), Span::raw(format!("   next message re-caches {long}"))]);
                    options.push(vec![state.clone(), Span::raw(format!("   re-caches {short}"))]);
                }
                options.push(vec![state]);
            }
            let room = inner.saturating_sub(11);
            let fits = |o: &Vec<Span<'static>>| o.iter().map(|s| s.width()).sum::<usize>() <= room;
            let n = options.len();
            let pick = options.iter().position(fits).unwrap_or(n - 1);
            cache.extend(options.swap_remove(pick));
        }
    }
    vec![first, Line::from(ctx), Line::from(cache)]
}

fn legend(cols: usize) -> Vec<Line<'static>> {
    let items: Vec<(&str, Style, &str)> = vec![
        ("✓", fg(GREEN), "ok under 70%"),
        ("▲", fg(YELLOW), "high 70-89%"),
        ("✗", fg(RED), "full 90%+"),
        ("┃", fg(MARK), "auto-compact"),
        ("●", fg(GREEN), "warm"),
        ("○", fg(COLD), "cold"),
        ("≈", Style::default(), "estimate"),
    ];
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    let mut w = 1;
    for (sym, style, word) in items {
        let need = width(sym) + 1 + width(word) + 2;
        if w + need > cols && w > 1 {
            lines.push(Line::from(std::mem::replace(&mut spans, vec![Span::raw(" ")])));
            w = 1;
        }
        spans.push(Span::styled(sym, style));
        spans.push(Span::styled(format!(" {word}  "), dim()));
        w += need;
    }
    lines.push(Line::from(spans));
    lines
}

// ------------------------------------------------------------ drawing

fn section_block(sec: &Section) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title_top(sec.title.clone())
        .title_top(sec.status.clone())
}

/// Draws the screen. Returns how many session lines did not fit, for scrolling.
pub fn draw(f: &mut Frame, screen: &Screen, scroll: usize) -> usize {
    let area = f.area();
    let mut constraints = vec![Constraint::Length(screen.header.len() as u16)];
    for s in &screen.limits {
        constraints.push(Constraint::Length(s.body.len() as u16 + 2));
    }
    constraints.push(Constraint::Min(3));
    constraints.push(Constraint::Length(screen.legend.len() as u16));
    let rects = Layout::vertical(constraints).split(area);

    f.render_widget(Paragraph::new(screen.header.clone()), rects[0]);
    for (i, s) in screen.limits.iter().enumerate() {
        f.render_widget(Paragraph::new(s.body.clone()).block(section_block(s)), rects[i + 1]);
    }
    let sess_rect: Rect = rects[screen.limits.len() + 1];
    let room = sess_rect.height.saturating_sub(2) as usize;
    let total = screen.sessions.body.len();
    let hidden = total.saturating_sub(room);
    let scroll = scroll.min(hidden);
    let mut block = section_block(&screen.sessions);
    if hidden > 0 {
        let below = hidden - scroll;
        let text = if below > 0 { format!(" ↑↓ scroll · {below} more lines below ") } else { " ↑↓ scroll ".to_string() };
        block = block.title_bottom(Line::styled(text, fg(YELLOW)).right_aligned());
    }
    let lines: Vec<Line> = screen.sessions.body.iter().skip(scroll).take(room).cloned().collect();
    f.render_widget(Paragraph::new(lines).block(block), sess_rect);
    f.render_widget(Paragraph::new(screen.legend.clone()), rects[screen.limits.len() + 2]);
    hidden
}
