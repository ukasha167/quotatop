//! Paths, CLI lookup, child processes with timeouts, and number/time formatting.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn mtime(p: &Path) -> Option<i64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(m.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn claude_dir() -> PathBuf {
    env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude"))
}

pub fn codex_dir() -> PathBuf {
    env_path("CODEX_HOME").unwrap_or_else(|| home().join(".codex"))
}

/// Where `quotatop tap` leaves status line snapshots.
pub fn spool_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(p) = env_path("LOCALAPPDATA") {
        return p.join("quotatop").join("tap");
    }
    env_path("XDG_CACHE_HOME").unwrap_or_else(|| home().join(".cache")).join("quotatop").join("tap")
}

/// Finds a CLI on PATH (with PATHEXT on Windows) or in ~/.local/bin.
/// `override_env` names a variable that holds an explicit path.
pub fn which(name: &str, override_env: &str) -> Option<PathBuf> {
    if let Some(p) = env_path(override_env) {
        return Some(p);
    }
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    };
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.push(home().join(".local").join("bin"));
    for d in dirs {
        for e in &exts {
            let c = d.join(format!("{name}{e}"));
            if is_executable(&c) {
                return Some(c);
            }
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.is_file() && m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        m.is_file()
    }
}

// ------------------------------------------------------------ child processes

/// PIDs of children still running, so quitting can stop them instead of leaving them behind.
static CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

pub fn register_child(pid: u32) {
    if let Ok(mut c) = CHILDREN.lock() {
        c.push(pid);
    }
}

pub fn unregister_child(pid: u32) {
    if let Ok(mut c) = CHILDREN.lock() {
        c.retain(|&p| p != pid);
    }
}

pub fn kill_children() {
    let pids = CHILDREN.lock().map(|c| c.clone()).unwrap_or_default();
    for pid in pids {
        kill_pid(pid);
    }
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // Signal 0 checks existence. EPERM means it exists but belongs to someone else.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE as u32
    }
}

/// A command that runs in the temp folder with no stdin and, on Windows, no console window.
pub fn command(bin: &Path) -> Command {
    let mut c = Command::new(bin);
    c.current_dir(std::env::temp_dir()).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

fn reader(mut src: impl Read + Send + 'static) -> (Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let b = buf.clone();
    let h = thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = src.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if let Ok(mut b) = b.lock() {
                b.extend_from_slice(&chunk[..n]);
            }
        }
    });
    (buf, h)
}

/// Runs a command and returns its stdout. Kills it after `timeout`.
pub fn run_capture(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("could not start: {e}"))?;
    let pid = child.id();
    register_child(pid);
    let (out, h_out) = reader(child.stdout.take().expect("piped stdout"));
    let (err, h_err) = reader(child.stderr.take().expect("piped stderr"));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => {
                unregister_child(pid);
                return Err(format!("wait failed: {e}"));
            }
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            unregister_child(pid);
            return Err(format!("no answer after {} s", timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(100));
    };
    unregister_child(pid);
    // A grandchild (agy's updater, say) can keep the pipes open after the CLI exits.
    // Give the readers a second to drain, then take what arrived.
    let drain = Instant::now();
    while !(h_out.is_finished() && h_err.is_finished()) && drain.elapsed() < Duration::from_secs(1) {
        thread::sleep(Duration::from_millis(20));
    }
    let stdout = String::from_utf8_lossy(&out.lock().map(|b| b.clone()).unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err.lock().map(|b| b.clone()).unwrap_or_default()).into_owned();
    if !status.success() && stdout.trim().is_empty() {
        let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let code = status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "killed".into());
        return Err(if last.is_empty() { code } else { shorten(last, 100) });
    }
    Ok(stdout)
}

// ------------------------------------------------------------ formatting

pub fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 850, 9.5k, 145k, 1M, 1.2M
pub fn tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        let k = n as f64 / 1000.0;
        if (k * 10.0).round() % 10.0 == 0.0 { format!("{:.0}k", k) } else { format!("{:.1}k", k) }
    } else if n < 999_500 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else {
        let m = n as f64 / 1_000_000.0;
        if (m * 10.0).round() % 10.0 == 0.0 { format!("{:.0}M", m) } else { format!("{:.1}M", m) }
    }
}

/// 30%, 7.5%
pub fn pct(p: f64) -> String {
    if (p - p.round()).abs() < 0.05 { format!("{:.0}%", p.round()) } else { format!("{:.1}%", p) }
}

/// 42 s, 4 m 12 s, 45 m, 1 h 30 m, 2 d 4 h
pub fn duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s} s")
    } else if s < 600 {
        format!("{} m {} s", s / 60, s % 60)
    } else if s < 3600 {
        format!("{} m", s / 60)
    } else if s < 86_400 {
        let m = (s % 3600) / 60;
        if m == 0 { format!("{} h", s / 3600) } else { format!("{} h {} m", s / 3600, m) }
    } else {
        let h = (s % 86_400) / 3600;
        if h == 0 { format!("{} d", s / 86_400) } else { format!("{} d {} h", s / 86_400, h) }
    }
}

pub fn ago(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s} s ago")
    } else if s < 3600 {
        format!("{} m ago", s / 60)
    } else if s < 86_400 {
        format!("{} h ago", s / 3600)
    } else {
        format!("{} d ago", s / 86_400)
    }
}

fn local(ts: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single()
}

/// 01:40
pub fn clock(ts: i64) -> String {
    local(ts).map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|| "?".into())
}

/// Wed 20:00 within the next 6 days, else 29 Oct 20:00.
pub fn day_clock(ts: i64, now: i64) -> String {
    let Some(t) = local(ts) else { return "?".into() };
    if (ts - now).abs() < 6 * 86_400 { t.format("%a %H:%M").to_string() } else { t.format("%-d %b %H:%M").to_string() }
}

/// 29 Oct
pub fn date(ts: i64) -> String {
    local(ts).map(|t| t.format("%-d %b").to_string()).unwrap_or_else(|| "?".into())
}

pub fn header_time(ts: i64) -> String {
    local(ts).map(|t| t.format("%a %-d %b %H:%M").to_string()).unwrap_or_default()
}

/// RFC 3339 / ISO 8601 timestamp to epoch seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp())
}

pub fn parse_iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp_millis())
}

/// Reads bytes from `offset` to the end. On the first read of a large file it starts
/// `first_read` bytes from the end and drops the partial first line.
pub fn read_new_lines(path: &Path, offset: u64, first_read: u64) -> std::io::Result<(Vec<String>, u64)> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    if size < offset {
        // The file was replaced or truncated: read it again from the tail.
        return read_new_lines(path, 0, first_read);
    }
    let (start, skip_partial) =
        if offset == 0 && size > first_read { (size - first_read, true) } else { (offset, false) };
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((size - start) as usize);
    f.take(size - start).read_to_end(&mut buf)?;
    // Only consume complete lines; a writer may be midway through the last one.
    let end = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    let mut text = String::from_utf8_lossy(&buf[..end]).into_owned();
    if skip_partial {
        text = text.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default();
    }
    let lines = text.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect();
    Ok((lines, start + end as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_format() {
        assert_eq!(tokens(850), "850");
        assert_eq!(tokens(9_500), "9.5k");
        assert_eq!(tokens(145_440), "145k");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(967_000), "967k");
        assert_eq!(tokens(258_400), "258k");
    }

    #[test]
    fn duration_format() {
        assert_eq!(duration(42), "42 s");
        assert_eq!(duration(252), "4 m 12 s");
        assert_eq!(duration(5400), "1 h 30 m");
        assert_eq!(duration(3540), "59 m");
        assert_eq!(duration(2 * 86_400 + 4 * 3600 + 59), "2 d 4 h");
    }

    #[test]
    fn pct_format() {
        assert_eq!(pct(30.0), "30%");
        assert_eq!(pct(7.5), "7.5%");
        assert_eq!(pct(12.98), "13%");
    }
}
