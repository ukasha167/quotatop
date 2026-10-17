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
  quotatop --once [--width N]   print one plain-text screen and exit
  quotatop tap [--then CMD]     Claude Code status line command (see README)

Options:
  --interval N     seconds between limit refreshes (default 60, minimum 30)
  --idle-backoff   refresh every 5 minutes while no local session has been active for 10 minutes
  -h, --help       show this help
  -V, --version    show the version

Environment:
  QUOTATOP_CLAUDE, QUOTATOP_CODEX, QUOTATOP_AGY  full path to a CLI not on PATH
  NO_COLOR                                       turn colours off
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("tap") {
        std::process::exit(tap::run(&args[1..]));
    }
    let has = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u64>().ok());
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
            eprintln!("quotatop: unknown argument {a}\n\n{HELP}");
            std::process::exit(2);
        }
    }
    let result = if has("--once") {
        let width = value("--width")
            .map(|w| w.clamp(60, 300) as u16)
            .or_else(|| ratatui::crossterm::terminal::size().ok().map(|s| s.0))
            .unwrap_or(80);
        app::once(width)
    } else {
        let interval = value("--interval").unwrap_or(60).max(30);
        app::run(app::Config { interval, idle_backoff: has("--idle-backoff") })
    };
    if let Err(e) = result {
        eprintln!("quotatop: {e}");
        std::process::exit(1);
    }
}
