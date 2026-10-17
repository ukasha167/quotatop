//! `quotatop tap`: a Claude Code status line command for terminal sessions.
//!
//! It saves the JSON Claude Code sends on stdin to the spool folder, one file per
//! session, then passes the same JSON to your own status line command (`--then`)
//! and prints its output. With no `--then` it prints a short line of its own.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::util;

pub fn run(args: &[String]) -> i32 {
    let then = args.iter().position(|a| a == "--then").and_then(|i| args.get(i + 1)).cloned();
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);
    let parsed: Option<Value> = serde_json::from_slice(&input).ok();
    if let Some(v) = &parsed {
        // A failed save must never break the user's status line, so errors are ignored.
        let _ = save(v, &input);
    }
    match then {
        Some(cmd) => chain(&cmd, &input),
        None => {
            if let Some(v) = parsed {
                println!("{}", default_line(&v));
            }
            0
        }
    }
}

fn save(v: &Value, raw: &[u8]) -> std::io::Result<()> {
    let Some(sid) = v.get("session_id").and_then(Value::as_str) else { return Ok(()) };
    if sid.is_empty() || !sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Ok(());
    }
    let dir = util::spool_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{sid}.json"));
    // With refreshInterval set, Claude Code re-sends the same JSON every few seconds.
    // Only the file time needs to change then.
    if std::fs::read(&path).is_ok_and(|old| old == raw) {
        return std::fs::File::options()
            .write(true)
            .open(&path)?
            .set_modified(std::time::SystemTime::now());
    }
    let tmp = dir.join(format!(".{sid}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, raw)?;
    std::fs::rename(&tmp, &path)
}

fn chain(cmd: &str, input: &[u8]) -> i32 {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    let Ok(mut child) = c.stdin(Stdio::piped()).spawn() else { return 1 };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    child.wait().ok().and_then(|s| s.code()).unwrap_or(1)
}

fn default_line(v: &Value) -> String {
    let model = v.pointer("/model/display_name").and_then(Value::as_str).unwrap_or("Claude");
    match v.pointer("/context_window/used_percentage").and_then(Value::as_f64) {
        Some(p) => format!("{model} · context {}", util::pct(p)),
        None => model.to_string(),
    }
}

/// Deletes snapshots of sessions that have not updated in a week.
pub fn clean_spool() {
    let Ok(rd) = std::fs::read_dir(util::spool_dir()) else { return };
    let cutoff = util::now() - 7 * 86_400;
    for e in rd.flatten() {
        if util::mtime(&e.path()).is_some_and(|m| m < cutoff) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
