//! Subscription limits from each CLI's own /usage. None of these calls spend tokens.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, NaiveDate, NaiveTime, TimeZone};
use serde_json::{Value, json};

use crate::model::{Limits, Pool, Window, mark_not_started};
use crate::util::{self, command, run_capture};

const FIVE_HOURS: i64 = 5 * 3600;
const WEEK: i64 = 7 * 86_400;

// ------------------------------------------------------------ Claude

/// `claude -p /usage`: about 4 s. It prints text, so the numbers match what /usage shows.
/// `-p` skips the workspace trust prompt, so only user settings and no MCP servers are loaded.
pub fn fetch_claude(bin: &Path) -> Result<Limits, String> {
    let mut cmd = command(bin)?;
    cmd.args(["-p", "/usage", "--output-format", "json", "--no-session-persistence"]);
    cmd.args(["--setting-sources", "user", "--strict-mcp-config"]);
    let out = run_capture(cmd, Duration::from_secs(60))?;
    let v: Value = serde_json::from_str(out.trim()).map_err(|_| "unexpected output from claude".to_string())?;
    let text = v.get("result").and_then(Value::as_str).unwrap_or("");
    if v.get("is_error").and_then(Value::as_bool) == Some(true) {
        return Err(util::shorten(text.lines().next().unwrap_or("claude reported an error"), 100));
    }
    let tz = iana_time_zone::get_timezone().ok();
    parse_claude_usage(text, util::now(), tz.as_deref())
}

pub fn parse_claude_usage(text: &str, now: i64, local_tz: Option<&str>) -> Result<Limits, String> {
    let mut windows = Vec::new();
    let mut notes = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("Current ") else { continue };
        let Some((label, rest)) = rest.split_once(": ") else { continue };
        let Some((num, rest)) = rest.split_once("% used") else { continue };
        let Some(used) = num.trim().parse::<f64>().ok().filter(|u| u.is_finite()) else { continue };
        let (label, length, per_model) = match label {
            "session" => ("5-hour".to_string(), Some(FIVE_HOURS), false),
            "week (all models)" => ("Weekly".to_string(), Some(WEEK), false),
            other => match other.strip_prefix("week (").and_then(|s| s.strip_suffix(')')) {
                Some(model) => (format!("Weekly {model}"), Some(WEEK), true),
                None => (other.to_string(), None, false),
            },
        };
        let reset_text = rest.split_once("resets ").map(|x| x.1.trim());
        let mut resets_at = None;
        if let Some(rt) = reset_text {
            match parse_claude_reset(rt, now, local_tz) {
                Ok(t) => resets_at = t,
                Err(e) => notes.push(e),
            }
        }
        windows.push(Window {
            label,
            used: Some(used),
            resets_at,
            length,
            // Claude leaves out "resets" for a window that has not begun.
            not_started: used == 0.0 && reset_text.is_none(),
            per_model,
        });
    }
    if windows.is_empty() {
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("empty output");
        return Err(format!("could not read /usage: {}", util::shorten(first, 70)));
    }
    notes.dedup();
    Ok(Limits {
        plan: None,
        pools: vec![Pool { name: None, short: "Claude".into(), detail: None, windows }],
        notes,
    })
}

/// "Oct 18 at 1:40am (Asia/Karachi)", "Oct 21 at 8pm (Asia/Karachi)" or "6am".
fn parse_claude_reset(text: &str, now: i64, local_tz: Option<&str>) -> Result<Option<i64>, String> {
    let (body, tz) = match text.rfind(" (") {
        Some(i) if text.ends_with(')') => (&text[..i], Some(&text[i + 2..text.len() - 1])),
        _ => (text, None),
    };
    if let (Some(tz), Some(local)) = (tz, local_tz) {
        if !same_zone(tz, local) {
            return Err(format!("Claude gives reset times in {tz}, this machine uses {local}"));
        }
    }
    let (date_part, time_part) = match body.rsplit_once(" at ") {
        Some((d, t)) => (Some(d.trim()), t.trim()),
        None => (None, body.trim()),
    };
    // A wording change in Claude Code must show up as a note, not as blank reset times.
    let unreadable = || format!("could not read Claude's reset time \"{}\"", util::shorten(text, 40));
    let Some(time) = parse_ampm(time_part) else { return Err(unreadable()) };
    let now_local = Local.timestamp_opt(now, 0).single().ok_or("bad clock")?;
    let to_ts = |d: NaiveDate| Local.from_local_datetime(&d.and_time(time)).earliest().map(|t| t.timestamp());
    match date_part {
        Some(d) => {
            let d = d.replace(',', "");
            let mut parts = d.split_whitespace();
            let month = parts.next().and_then(month_number);
            let day = parts.next().and_then(|s| s.parse::<u32>().ok());
            let year = parts.next().and_then(|s| s.parse::<i32>().ok());
            let (Some(month), Some(day)) = (month, day) else { return Err(unreadable()) };
            let ts = match year {
                Some(y) => NaiveDate::from_ymd_opt(y, month, day).and_then(to_ts),
                None => {
                    // No year given: pick the year that puts the date nearest to now.
                    let y = now_local.year();
                    [y - 1, y, y + 1]
                        .into_iter()
                        .filter_map(|y| NaiveDate::from_ymd_opt(y, month, day).and_then(to_ts))
                        .min_by_key(|t| (t - now).abs())
                }
            };
            Ok(ts)
        }
        None => {
            let today = now_local.date_naive();
            let t = to_ts(today);
            Ok(match t {
                Some(t) if t < now - 60 => today.succ_opt().and_then(to_ts),
                other => other,
            })
        }
    }
}

/// Old and new spellings of the same zone. Claude Code runs on JavaScript, which prints
/// the old name (Asia/Calcutta) while the system reports the new one (Asia/Kolkata).
const ZONE_ALIASES: &[(&str, &str)] = &[
    ("Asia/Calcutta", "Asia/Kolkata"),
    ("Asia/Katmandu", "Asia/Kathmandu"),
    ("Europe/Kiev", "Europe/Kyiv"),
    ("Asia/Saigon", "Asia/Ho_Chi_Minh"),
    ("Asia/Rangoon", "Asia/Yangon"),
    ("Asia/Dacca", "Asia/Dhaka"),
    ("Asia/Thimbu", "Asia/Thimphu"),
    ("Asia/Ulan_Bator", "Asia/Ulaanbaatar"),
    ("Atlantic/Faeroe", "Atlantic/Faroe"),
    ("Europe/Uzhgorod", "Europe/Kyiv"),
    ("Europe/Zaporozhye", "Europe/Kyiv"),
    ("Pacific/Ponape", "Pacific/Pohnpei"),
    ("Pacific/Truk", "Pacific/Chuuk"),
    ("America/Buenos_Aires", "America/Argentina/Buenos_Aires"),
    ("America/Catamarca", "America/Argentina/Catamarca"),
    ("America/Cordoba", "America/Argentina/Cordoba"),
    ("America/Jujuy", "America/Argentina/Jujuy"),
    ("America/Mendoza", "America/Argentina/Mendoza"),
    ("America/Indianapolis", "America/Indiana/Indianapolis"),
    ("America/Louisville", "America/Kentucky/Louisville"),
    ("America/Godthab", "America/Nuuk"),
];

const UTC_NAMES: &[&str] = &[
    "UTC", "GMT", "UCT", "Universal", "Zulu", "Greenwich", "GMT0", "GMT+0", "GMT-0", "Etc/UTC", "Etc/GMT",
    "Etc/UCT", "Etc/Universal", "Etc/Zulu", "Etc/Greenwich", "Etc/GMT0", "Etc/GMT+0", "Etc/GMT-0",
];

/// True when two zone names describe the same zone: equal, both UTC, a known alias pair,
/// or two names whose tz database files hold the same bytes.
fn same_zone(a: &str, b: &str) -> bool {
    if a == b || (UTC_NAMES.contains(&a) && UTC_NAMES.contains(&b)) {
        return true;
    }
    let canon = |z: &str| ZONE_ALIASES.iter().find(|(old, _)| *old == z).map(|x| x.1).unwrap_or(z).to_string();
    if canon(a) == canon(b) {
        return true;
    }
    match (zone_file(a), zone_file(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn zone_file(name: &str) -> Option<Vec<u8>> {
    let valid = !name.is_empty()
        && !name.starts_with('/')
        && name.split('/').all(|p| !p.is_empty() && p != "..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c));
    if !valid {
        return None;
    }
    let dir = std::env::var_os("TZDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "/usr/share/zoneinfo".into());
    let data = std::fs::read(dir.join(name)).ok()?;
    data.starts_with(b"TZif").then_some(data)
}

fn parse_ampm(s: &str) -> Option<NaiveTime> {
    let s = s.trim().to_ascii_lowercase();
    let (num, pm) = if let Some(n) = s.strip_suffix("pm") {
        (n, true)
    } else if let Some(n) = s.strip_suffix("am") {
        (n, false)
    } else {
        return None;
    };
    let (h, m) = match num.trim().split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (num.trim().parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&h) {
        return None;
    }
    NaiveTime::from_hms_opt(h % 12 + if pm { 12 } else { 0 }, m, 0)
}

fn month_number(s: &str) -> Option<u32> {
    let m = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let key = s.get(..3)?.to_ascii_lowercase();
    m.iter().position(|x| *x == key).map(|i| i as u32 + 1)
}

// ------------------------------------------------------------ Codex

/// Asks `codex app-server` for `account/rateLimits/read`: about 3 s.
pub fn fetch_codex(bin: &Path) -> Result<Limits, String> {
    let mut cmd = command(bin)?;
    cmd.arg("app-server").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("could not start: {e}"))?;
    let pid = child.id();
    util::register_child(pid);
    let result = codex_exchange(&mut child);
    // Closing stdin asks the server to exit; kill it if it does not.
    drop(child.stdin.take());
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if let Ok(None) = child.try_wait() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Anything the server started goes with it, so the reader thread reaches end of file.
    util::kill_group(pid);
    util::unregister_child(pid);
    let fetched = util::now();
    parse_codex(&result?, fetched)
}

fn codex_exchange(child: &mut std::process::Child) -> Result<Value, String> {
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let msgs = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"clientInfo": {"name": "quotatop", "version": env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc": "2.0", "method": "initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read"}),
    ];
    for m in msgs {
        writeln!(stdin, "{m}").map_err(|e| format!("could not talk to codex: {e}"))?;
    }
    stdin.flush().ok();
    child.stdin = Some(stdin);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no answer after 30 s".into());
        }
        let line = match rx.recv_timeout(left) {
            Ok(l) => l,
            Err(mpsc::RecvTimeoutError::Timeout) => return Err("no answer after 30 s".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("codex app-server exited".into()),
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("id").and_then(Value::as_i64) != Some(2) {
            continue;
        }
        if let Some(e) = v.get("error") {
            let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
            return Err(util::shorten(msg, 100));
        }
        return v.get("result").cloned().ok_or_else(|| "empty answer".to_string());
    }
}

pub fn parse_codex(result: &Value, fetched_at: i64) -> Result<Limits, String> {
    let mut snapshots: Vec<&Value> = Vec::new();
    if let Some(map) = result.get("rateLimitsByLimitId").and_then(Value::as_object).filter(|m| !m.is_empty()) {
        // The main "codex" limit first, then any per-model limits.
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort_by_key(|k| (k.as_str() != "codex", k.to_string()));
        snapshots.extend(keys.into_iter().filter_map(|k| map.get(k)));
    } else if let Some(rl) = result.get("rateLimits") {
        snapshots.push(rl);
    }
    if snapshots.is_empty() {
        return Err("codex returned no rate limits".into());
    }
    let mut limits = Limits::default();
    for snap in snapshots {
        if limits.plan.is_none() {
            limits.plan = snap.get("planType").and_then(Value::as_str).map(capitalize);
        }
        let id = snap.get("limitId").and_then(Value::as_str).unwrap_or("codex");
        let name = snap.get("limitName").and_then(Value::as_str).map(str::to_string);
        let mut windows = Vec::new();
        for key in ["primary", "secondary"] {
            let Some(w) = snap.get(key).filter(|w| !w.is_null()) else { continue };
            let mins = w.get("windowDurationMins").and_then(Value::as_i64);
            let mut win = Window {
                label: mins.map(window_label).unwrap_or_else(|| key.to_string()),
                used: w.get("usedPercent").and_then(Value::as_f64),
                resets_at: w.get("resetsAt").and_then(Value::as_i64),
                length: mins.map(|m| m.saturating_mul(60)),
                not_started: false,
                per_model: false,
            };
            mark_not_started(&mut win, fetched_at);
            windows.push(win);
        }
        if let Some(t) = snap.get("rateLimitReachedType").and_then(Value::as_str) {
            limits.notes.push(format!("✗ limit reached ({t})"));
        }
        let pool_name = if id == "codex" { None } else { Some(name.clone().unwrap_or_else(|| id.to_string())) };
        let short = match &pool_name {
            Some(n) => format!("Codex {n}"),
            None => "Codex".into(),
        };
        limits.pools.push(Pool { name: pool_name, short, detail: None, windows });
    }
    if result.get("ordinaryUsageAllowed").and_then(Value::as_bool) == Some(false) {
        limits.notes.push("✗ Codex says ordinary usage is not allowed right now".into());
    }
    if let Some(credits) = result.pointer("/rateLimitResetCredits/credits").and_then(Value::as_array) {
        let mut expiries: Vec<i64> = credits
            .iter()
            .filter(|c| c.get("status").and_then(Value::as_str) == Some("available"))
            .filter_map(|c| c.get("expiresAt").and_then(Value::as_i64))
            .collect();
        expiries.sort();
        match expiries.len() {
            0 => {}
            1 => limits.notes.push(format!("1 free full reset available, expires {}", util::date(expiries[0]))),
            n => limits.notes.push(format!(
                "{n} free full resets available, first one expires {}",
                util::date(expiries[0])
            )),
        }
    }
    Ok(limits)
}

fn window_label(mins: i64) -> String {
    match mins {
        300 => "5-hour".into(),
        10_080 => "Weekly".into(),
        m if m % 1440 == 0 => format!("{}-day", m / 1440),
        m if m % 60 == 0 => format!("{}-hour", m / 60),
        m => format!("{m}-min"),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// ------------------------------------------------------------ Antigravity

/// `agy -p=/usage --output-format json`: 7 to 13 s.
pub fn fetch_agy(bin: &Path) -> Result<Limits, String> {
    // agy writes a log file per run, with the account email in it. Keep it in the private
    // run folder and delete it afterwards; `util::clean_run_dir` removes any left by a kill.
    let mut cmd = command(bin)?;
    let log = util::run_dir()?.join(format!("agy-{}.log", std::process::id()));
    cmd.arg("-p=/usage").args(["--output-format", "json"]).arg("--log-file").arg(&log);
    let out = run_capture(cmd, Duration::from_secs(90));
    let _ = std::fs::remove_file(&log);
    let out = out?;
    let fetched = util::now();
    let v: Value = serde_json::from_str(out.trim()).map_err(|_| "unexpected output from agy".to_string())?;
    parse_agy(&v, fetched)
}

pub fn parse_agy(v: &Value, fetched_at: i64) -> Result<Limits, String> {
    let status = v.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "SUCCESS" {
        let msg = v.get("response").and_then(Value::as_str).unwrap_or(status);
        return Err(format!("agy: {}", util::shorten(msg.lines().next().unwrap_or("error"), 90)));
    }
    // The text response holds the percentages agy prints, so use those where they exist.
    let printed: Vec<(String, String, f64)> = v
        .get("response")
        .and_then(Value::as_str)
        .unwrap_or("")
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let p = f.get(2)?.trim().strip_suffix('%')?.parse::<f64>().ok().filter(|p| p.is_finite())?;
            Some((f[0].to_string(), f[1].to_string(), p))
        })
        .collect();
    let groups = v.pointer("/command/data/groups").and_then(Value::as_array).ok_or("agy returned no usage groups")?;
    let mut limits = Limits::default();
    for g in groups {
        let name = g.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
        let detail = g
            .get("description")
            .and_then(Value::as_str)
            .map(|d| d.trim_start_matches("Models within this group:").trim().to_string());
        let mut windows = Vec::new();
        for b in g.get("buckets").and_then(Value::as_array).into_iter().flatten() {
            let kind = b.get("window").and_then(Value::as_str).unwrap_or("?");
            let bname = b.get("name").and_then(Value::as_str).unwrap_or("");
            let (label, length) = match kind {
                "5h" => ("5-hour".to_string(), Some(FIVE_HOURS)),
                "weekly" => ("Weekly".to_string(), Some(WEEK)),
                other => (other.to_string(), None),
            };
            let remaining = printed
                .iter()
                .find(|(g, n, _)| *g == name && n == bname)
                .map(|x| x.2)
                .or_else(|| b.get("remaining_fraction").and_then(Value::as_f64).map(|f| (f * 100.0).round()));
            let mut w = Window {
                label,
                used: remaining.map(|r| (100.0 - r).max(0.0)),
                resets_at: b.get("reset_time").and_then(Value::as_str).and_then(util::parse_iso),
                length,
                not_started: false,
                per_model: false,
            };
            mark_not_started(&mut w, fetched_at);
            windows.push(w);
        }
        windows.sort_by_key(|w| w.label != "5-hour");
        let short = format!("agy {}", name.replace(" Models", "").replace(" models", "").replace(" and ", "+"));
        limits.pools.push(Pool { name: Some(name), short, detail, windows });
    }
    if limits.pools.is_empty() {
        return Err("agy returned no usage groups".into());
    }
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_ts(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).earliest().unwrap().timestamp()
    }

    #[test]
    fn claude_usage_text() {
        let now = local_ts(2026, 10, 18, 0, 10);
        let tz = iana_time_zone::get_timezone().unwrap();
        let text = format!(
            "You are currently using your subscription to power your Claude Code usage\n\n\
             Current session: 30% used · resets Oct 18 at 1:40am ({tz})\n\
             Current week (all models): 25% used · resets Oct 21 at 8pm ({tz})\n\
             Current week (Opus): 3% used · resets Oct 21 at 8pm ({tz})\n\n\
             What's contributing to your limits usage?\n"
        );
        let l = parse_claude_usage(&text, now, Some(&tz)).unwrap();
        let w = &l.pools[0].windows;
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].label, "5-hour");
        assert_eq!(w[0].used, Some(30.0));
        assert_eq!(w[0].resets_at, Some(local_ts(2026, 10, 18, 1, 40)));
        assert_eq!(w[1].label, "Weekly");
        assert_eq!(w[1].resets_at, Some(local_ts(2026, 10, 21, 20, 0)));
        assert_eq!(w[2].label, "Weekly Opus");
        assert!(l.notes.is_empty());
    }

    #[test]
    fn claude_reset_without_date_rolls_to_tomorrow() {
        let now = local_ts(2026, 10, 18, 22, 0);
        let t = parse_claude_reset("6am", now, None).unwrap();
        assert_eq!(t, Some(local_ts(2026, 10, 19, 6, 0)));
    }

    #[test]
    fn claude_reset_year_boundary() {
        let now = local_ts(2026, 12, 30, 12, 0);
        let t = parse_claude_reset("Jan 2 at 9am", now, None).unwrap();
        assert_eq!(t, Some(local_ts(2027, 1, 2, 9, 0)));
    }

    #[test]
    fn claude_other_time_zone_is_flagged() {
        let now = local_ts(2026, 10, 18, 0, 10);
        let l = parse_claude_usage(
            "Current session: 5% used · resets 3pm (Europe/London)",
            now,
            Some("Asia/Karachi"),
        )
        .unwrap();
        assert_eq!(l.pools[0].windows[0].resets_at, None);
        assert_eq!(l.notes.len(), 1);
    }

    #[test]
    fn claude_zone_aliases_match() {
        let now = local_ts(2026, 10, 18, 0, 10);
        for (claude, system) in [
            ("Asia/Calcutta", "Asia/Kolkata"),
            ("Europe/Kiev", "Europe/Kyiv"),
            ("UTC", "GMT"),
            ("UTC", "Etc/UTC"),
            ("America/Buenos_Aires", "America/Argentina/Buenos_Aires"),
        ] {
            let text = format!("Current session: 5% used · resets 3pm ({claude})");
            let l = parse_claude_usage(&text, now, Some(system)).unwrap();
            assert!(l.notes.is_empty(), "{claude} vs {system}: {:?}", l.notes);
            assert!(l.pools[0].windows[0].resets_at.is_some());
        }
        assert!(!same_zone("Asia/Karachi", "Asia/Kolkata"));
        assert!(!same_zone("../../etc/passwd", "../../etc/passwd/"));
    }

    #[test]
    fn claude_unreadable_reset_is_noted() {
        let l = parse_claude_usage("Current session: 5% used · resets in 3h\n", 0, None).unwrap();
        assert_eq!(l.pools[0].windows[0].resets_at, None);
        assert_eq!(l.notes.len(), 1);
        assert!(parse_claude_usage("Current session: NaN% used\n", 0, None).is_err());
    }

    #[test]
    fn claude_model_weeks_are_marked() {
        let l = parse_claude_usage("Current week (all models): 5% used\nCurrent week (Opus): 50% used\n", 0, None).unwrap();
        let w = &l.pools[0].windows;
        assert!(!w[0].per_model && w[1].per_model);
    }

    #[test]
    fn codex_empty_limit_map_falls_back() {
        let v = json!({"rateLimitsByLimitId": {}, "rateLimits": {"primary": {"usedPercent": 12.0, "windowDurationMins": 300}}});
        let l = parse_codex(&v, 0).unwrap();
        assert_eq!(l.pools[0].windows[0].used, Some(12.0));
    }

    #[test]
    fn claude_unstarted_session() {
        let l = parse_claude_usage("Current session: 0% used\n", 0, None).unwrap();
        assert!(l.pools[0].windows[0].not_started);
    }

    #[test]
    fn claude_garbage_is_an_error() {
        assert!(parse_claude_usage("Unknown command: /usage", 0, None).is_err());
    }

    #[test]
    fn codex_result() {
        let v: Value = serde_json::from_str(include_str!("../tests/data/codex_rate_limits.json")).unwrap();
        let l = parse_codex(&v["result"], 1_792_263_997).unwrap();
        assert_eq!(l.plan.as_deref(), Some("Plus"));
        assert_eq!(l.pools.len(), 1);
        let w = &l.pools[0].windows;
        assert_eq!((w[0].label.as_str(), w[0].used, w[0].resets_at), ("5-hour", Some(0.0), Some(1_792_280_392)));
        assert!(!w[0].not_started, "a 0% window with a reset 4.5 h away has started");
        assert_eq!((w[1].label.as_str(), w[1].used), ("Weekly", Some(17.0)));
        assert!(l.notes[0].starts_with("2 free full resets available"));
    }

    #[test]
    fn agy_result() {
        let v: Value = serde_json::from_str(include_str!("../tests/data/agy_usage.json")).unwrap();
        // Fetched at 2026-10-17T19:05:43Z; both resets sit exactly one window later.
        let l = parse_agy(&v, 1_792_263_943).unwrap();
        assert_eq!(l.pools.len(), 2);
        assert_eq!(l.pools[0].short, "agy Gemini");
        assert_eq!(l.pools[1].short, "agy Claude+GPT");
        let w = &l.pools[0].windows;
        assert_eq!(w[0].label, "5-hour");
        assert_eq!(w[0].used, Some(0.0));
        assert!(w[0].not_started && w[1].not_started);
    }

    #[test]
    fn agy_printed_percent_wins() {
        let v = json!({"status": "SUCCESS",
            "response": "Gemini Models\tFive Hour Limit Remaining\t87%\t2026-10-18T00:05:42Z\n",
            "command": {"data": {"groups": [{"name": "Gemini Models", "buckets": [
                {"window": "5h", "name": "Five Hour Limit Remaining", "remaining_fraction": 0.80,
                 "reset_time": "2026-10-18T00:05:42Z"}]}]}}});
        let l = parse_agy(&v, 0).unwrap();
        assert_eq!(l.pools[0].windows[0].used, Some(13.0));
    }
}
