mod app;
mod limits;
mod model;
mod sessions;
mod tap;
mod ui;
mod util;

const HELP: &str = "\
quotatop: Claude, Codex and Antigravity usage in one terminal screen.

Usage:
  quotatop                      live dashboard; q quits, r refreshes now, arrow keys scroll
  quotatop --once [--width N]   print one plain-text screen and exit (N from 60 to 300)
  quotatop tap [--then CMD]     Claude Code status line command (see README)

Options:
  --interval N     seconds between limit refreshes, 30 to 86400 (default 60)
  --idle-backoff   refresh every 5 minutes while no local session has been active for 10 minutes
  -h, --help       show this help
  -V, --version    show the version

Environment:
  QUOTATOP_CLAUDE, QUOTATOP_CODEX, QUOTATOP_AGY  full path to a CLI not on PATH
  NO_COLOR                                       turn colours off
";

fn fail(msg: &str) -> ! {
    eprintln!("quotatop: {msg}\n\n{HELP}");
    std::process::exit(2);
}

/// The value after `flag`, as a whole number in `range`. A bad value is an error, not a default.
fn number(args: &[String], flag: &str, range: std::ops::RangeInclusive<u64>) -> Option<u64> {
    let i = args.iter().position(|a| a == flag)?;
    let Some(v) = args.get(i + 1) else { fail(&format!("{flag} needs a value")) };
    match v.parse::<u64>() {
        Ok(n) if range.contains(&n) => Some(n),
        _ => fail(&format!("{flag} must be a whole number from {} to {}, not {v}", range.start(), range.end())),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("tap") {
        std::process::exit(tap::run(&args[1..]));
    }
    let has = |f: &str| args.iter().any(|a| a == f);
    if has("-h") || has("--help") {
        print!("{HELP}");
        return;
    }
    if has("-V") || has("--version") {
        println!("quotatop {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let known = ["--once", "--width", "--interval", "--idle-backoff"];
    for (i, a) in args.iter().enumerate() {
        let is_value = i > 0 && (args[i - 1] == "--width" || args[i - 1] == "--interval");
        if !known.contains(&a.as_str()) && !is_value {
            fail(&format!("unknown argument {a}"));
        }
    }
    let interval = number(&args, "--interval", 30..=86_400);
    let width = number(&args, "--width", 60..=300);
    let result = if has("--once") {
        let width = width
            .map(|w| w as u16)
            .or_else(|| ratatui::crossterm::terminal::size().ok().map(|s| s.0))
            .unwrap_or(80);
        app::once(width)
    } else {
        app::run(app::Config { interval: interval.unwrap_or(60), idle_backoff: has("--idle-backoff") })
    };
    if let Err(e) = result {
        eprintln!("quotatop: {e}");
        std::process::exit(1);
    }
}
