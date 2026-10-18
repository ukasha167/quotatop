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

fn cache_root() -> PathBuf {
    #[cfg(windows)]
    if let Some(p) = env_path("LOCALAPPDATA") {
        return p.join("quotatop");
    }
    env_path("XDG_CACHE_HOME").unwrap_or_else(|| home().join(".cache")).join("quotatop")
}

/// Where `quotatop tap` leaves status line snapshots.
pub fn spool_dir() -> PathBuf {
    cache_root().join("tap")
}

/// Creates `dir` readable only by this user, or checks that an existing one is.
pub fn private_dir(dir: &Path) -> Result<(), String> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("could not check {}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(format!("{} belongs to another user", dir.display()));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("could not make {} private: {e}", dir.display()))?;
        }
    }
    Ok(())
}

/// The private folder the CLIs run in. A shared folder such as /tmp could hold a
/// `.claude/settings.json` or `.mcp.json` planted by another user, which `claude -p` would trust.
pub fn run_dir() -> Result<PathBuf, String> {
    let dir = cache_root().join("run");
    private_dir(&dir)?;
    Ok(dir)
}

/// Deletes agy logs (`agy-<pid>.log`) left behind by a quotatop that was killed mid-fetch.
/// They hold the account email.
pub fn clean_run_dir() {
    let Ok(rd) = std::fs::read_dir(cache_root().join("run")) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let pid = name.to_str().and_then(|n| n.strip_prefix("agy-")?.strip_suffix(".log")?.parse::<u32>().ok());
        if pid.is_some_and(|p| p != std::process::id() && !pid_alive(p)) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Finds a CLI on PATH (with PATHEXT on Windows) or in ~/.local/bin.
/// `override_env` names a variable that holds an explicit path.
pub fn which(name: &str, override_env: &str) -> Option<PathBuf> {
    if let Some(p) = env_path(override_env) {
        // The CLIs run in another folder, so a relative path is resolved here.
        return std::path::absolute(&p).ok();
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
    // A relative entry such as `.` would find whatever sits in the current folder.
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).collect())
        .unwrap_or_default();
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
        kill_group(pid);
    }
}

/// Stops a child started by `command` and everything it started. On Unix the child leads
/// its own process group, so the group gets the signal.
#[cfg(unix)]
pub fn kill_group(pid: u32) {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return };
    if pid > 0 {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
pub fn kill_group(pid: u32) {
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
    // kill(0, 0) and kill(-1, 0) address process groups, so only real PIDs are checked.
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // Signal 0 checks existence. EPERM means it exists but belongs to someone else.
    let r = unsafe { libc::kill(pid, 0) };
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

/// A command that runs in the private run folder with no stdin, in its own process group
/// on Unix (so a timeout or quit stops its children too) and, on Windows, with no console window.
pub fn command(bin: &Path) -> Result<Command, String> {
    let mut c = Command::new(bin);
    c.current_dir(run_dir()?).stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    Ok(c)
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
            kill_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            unregister_child(pid);
            return Err(format!("no answer after {} s", timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(100));
    };
    // A grandchild (agy's updater, say) can keep the pipes open after the CLI exits.
    // Give the readers a second to drain, then stop the group so they reach end of file.
    let drain = Instant::now();
    while !(h_out.is_finished() && h_err.is_finished()) && drain.elapsed() < Duration::from_secs(1) {
        thread::sleep(Duration::from_millis(20));
    }
    if !(h_out.is_finished() && h_err.is_finished()) {
        kill_group(pid);
        let killed = Instant::now();
        while !(h_out.is_finished() && h_err.is_finished()) && killed.elapsed() < Duration::from_secs(1) {
            thread::sleep(Duration::from_millis(20));
        }
    }
    unregister_child(pid);
    // Only a grandchild that moved to its own process group can still hold the pipes.
    // Its reader is left to finish when it exits rather than block this fetch.
    for h in [h_out, h_err] {
        if h.is_finished() {
            let _ = h.join();
        }
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

/// Cuts `s` to `max` terminal columns, ending in "…" when cut. Counts display width, so
/// Chinese, Japanese and emoji names (two columns each) do not overflow.
pub fn shorten(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
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

/// 30%, 7.5%. A value just under 70, 90 or 100 is never rounded up to it, so the number
/// always agrees with its level (69.96 is "69.9% ✓ ok", not "70% ✓ ok").
pub fn pct(p: f64) -> String {
    let crosses = [70.0, 90.0, 100.0].iter().any(|&b| p < b && p.round() >= b);
    if crosses {
        format!("{:.1}%", (p * 10.0).floor() / 10.0)
    } else if (p - p.round()).abs() < 0.05 {
        format!("{:.0}%", p.round())
    } else {
        format!("{:.1}%", p)
    }
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

// ------------------------------------------------------------ files

/// Opens a regular file for reading. A FIFO or device (a repo can commit a symlink to one)
/// is refused instead of blocking in open() or reading forever.
pub fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NONBLOCK);
    }
    let f = o.open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(f)
}

/// Reads at most `max` bytes of a regular file. A longer file is an error, not a prefix.
pub fn read_limited(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    let f = open_regular(path)?;
    let mut buf = Vec::new();
    f.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(buf)
}

/// Session IDs become file names, so only letters, digits, `-` and `_` are allowed.
pub fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Reads bytes from `offset` to the end. On the first read of a large file it starts
/// `first_read` bytes from the end and drops the partial first line.
pub fn read_new_lines(path: &Path, offset: u64, first_read: u64) -> std::io::Result<(Vec<String>, u64)> {
    use std::io::{Seek, SeekFrom};
    let mut f = open_regular(path)?;
    let size = f.metadata()?.len();
    if size < offset {
        // The file was replaced or truncated: read it again from the tail.
        return read_new_lines(path, 0, first_read);
    }
    let (start, skip_partial) =
        if offset == 0 && size > first_read { (size - first_read, true) } else { (offset, false) };
    // Read one byte before the cut: if it ends a line, the first line read is complete.
    let lead = u64::from(skip_partial);
    f.seek(SeekFrom::Start(start - lead))?;
    let mut buf = Vec::with_capacity((size - start + lead) as usize);
    f.take(size - start + lead).read_to_end(&mut buf)?;
    let cut_on_line_start = skip_partial && buf.first() == Some(&b'\n');
    let body = &buf[lead as usize..];
    // Only consume complete lines; a writer may be midway through the last one.
    let end = body.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    let mut text = String::from_utf8_lossy(&body[..end]).into_owned();
    if skip_partial && !cut_on_line_start {
        text = text.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default();
    }
    let lines = text.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect();
    Ok((lines, start + end as u64))
}

// ------------------------------------------------------------ other processes

/// When a process started, in epoch seconds. Used to spot a PID reused by another program.
#[cfg(target_os = "macos")]
pub fn process_start(pid: u32) -> Option<i64> {
    let pid = libc::c_int::try_from(pid).ok().filter(|p| *p > 0)?;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
    };
    (n == size).then_some(info.pbi_start_tvsec as i64)
}

#[cfg(target_os = "linux")]
pub fn process_start(pid: u32) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22, counted after the command name, which sits in parentheses and may hold spaces.
    let ticks: u64 = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()?;
    let btime: i64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (hz > 0).then(|| btime + (ticks / hz as u64) as i64)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_start(_pid: u32) -> Option<i64> {
    None
}

/// The values of `keys` in another process's environment. Only processes of the same user
/// can be read. Everything else in that environment is dropped unread.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn process_env(pid: u32, keys: &[&str]) -> Option<std::collections::HashMap<String, String>> {
    let raw = process_env_raw(pid)?;
    let mut out = std::collections::HashMap::new();
    for entry in raw.split(|&b| b == 0) {
        let Ok(entry) = std::str::from_utf8(entry) else { continue };
        if let Some((k, v)) = entry.split_once('=') {
            if keys.contains(&k) {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    Some(out)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_env(_pid: u32, _keys: &[&str]) -> Option<std::collections::HashMap<String, String>> {
    None
}

#[cfg(target_os = "linux")]
fn process_env_raw(pid: u32) -> Option<Vec<u8>> {
    read_limited(Path::new(&format!("/proc/{pid}/environ")), 1 << 20).ok()
}

/// KERN_PROCARGS2 returns argc, the executable path, argv and then the environment,
/// each NUL-terminated.
#[cfg(target_os = "macos")]
fn process_env_raw(pid: u32) -> Option<Vec<u8>> {
    let pid = libc::c_int::try_from(pid).ok().filter(|p| *p > 0)?;
    let mut max: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 2, (&raw mut max).cast(), &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    let mut buf = vec![0u8; usize::try_from(max).ok()?];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(len);
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let mut rest = &buf[4..];
    // Skip the executable path and the NUL padding after it.
    let path_end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[path_end..];
    let first = rest.iter().position(|&b| b != 0)?;
    rest = &rest[first..];
    for _ in 0..argc.max(0) {
        let end = rest.iter().position(|&b| b == 0)?;
        rest = &rest[end + 1..];
    }
    let env_end = rest.windows(2).position(|w| w == [0, 0]).map(|i| i + 1).unwrap_or(rest.len());
    Some(rest[..env_end].to_vec())
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
        assert_eq!(pct(69.96), "69.9%");
        assert_eq!(pct(89.97), "89.9%");
        assert_eq!(pct(99.96), "99.9%");
        assert_eq!(pct(100.0), "100%");
    }

    #[test]
    fn shorten_counts_columns() {
        assert_eq!(shorten("abcdef", 4), "abc…");
        assert_eq!(shorten("日本語のなまえ", 7), "日本語…");
        assert_eq!(shorten("abc", 3), "abc");
    }
}
