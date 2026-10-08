//! The Chromium process the app owns: a visible window on its own profile
//! in the workspace's state dir, debugging on a loopback port Chromium picks
//! and writes to `DevToolsActivePort`. The profile outlives the process, so
//! the person's logins in it stay (ADR-0049).
#![allow(
    clippy::disallowed_methods,
    reason = "the browser is a long-lived child the app owns, started from the endpoint thread"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const START_LIMIT: Duration = Duration::from_secs(60);
/// How long Chromium gets to close itself (and flush cookies) before it is
/// killed.
const CLOSE_LIMIT: Duration = Duration::from_secs(3);

pub struct Chromium {
    child: Option<Child>,
    pub port: u16,
}

impl Chromium {
    pub fn launch(exe: &Path, profile: &Path) -> Result<Self, String> {
        create_profile(profile)?;
        let port_file = profile.join("DevToolsActivePort");
        close_leftover(&port_file);
        let _ = std::fs::remove_file(&port_file);
        let mut command = Command::new(exe);
        // Its own process group: the app reaps Chromium and its helpers
        // together, and a terminal's signals to cedian do not reach it.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start {}: {e}", exe.display()))?;
        match wait_for_port(&port_file, &mut child) {
            Ok(port) => Ok(Self {
                child: Some(child),
                port,
            }),
            Err(e) => {
                kill(&mut child);
                Err(e)
            }
        }
    }
}

impl Chromium {
    pub fn exited(&mut self) -> bool {
        self.child
            .as_mut()
            .is_none_or(|c| !matches!(c.try_wait(), Ok(None)))
    }

    /// [`Drop`]'s close on this thread, bounded by `limit`.
    pub fn close_now(&mut self, limit: Duration) {
        if let Some(mut child) = self.child.take() {
            close(self.port, &mut child, limit);
        }
    }
}

impl Drop for Chromium {
    /// `Browser.close` over CDP, so the profile's cookies flush; the group
    /// is killed after [`CLOSE_LIMIT`]. Runs off the dropping thread.
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let port = self.port;
        std::thread::spawn(move || close(port, &mut child, CLOSE_LIMIT));
    }
}

fn close(port: u16, child: &mut Child, limit: Duration) {
    browser_close(port, limit.min(Duration::from_secs(2)));
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    kill(child);
}

/// Kills the child's process group, but only while the child is unreaped:
/// once reaped, its pid (and so the group id) may belong to someone else.
fn kill(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: signals the process group the child leads (process_group(0)).
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Ask the Chromium on `port` to close itself.
fn browser_close(port: u16, limit: Duration) {
    let Some(url) = http_get(port, "/json/version").ok().and_then(|body| {
        let version: serde_json::Value = serde_json::from_str(&body).ok()?;
        Some(version.get("webSocketDebuggerUrl")?.as_str()?.to_string())
    }) else {
        return;
    };
    if let Ok(mut client) = super::cdp::CdpClient::connect(&url) {
        let _ = client.call("Browser.close", serde_json::json!({}), limit);
    }
}

/// A Chromium a crashed cedian left running on this profile is closed (not
/// adopted: the app could not see it exit or kill it), so the new one can
/// take the profile.
fn close_leftover(port_file: &Path) {
    let Some(port) = std::fs::read_to_string(port_file)
        .ok()
        .and_then(|text| text.lines().next()?.trim().parse::<u16>().ok())
    else {
        return;
    };
    if http_get(port, "/json/version").is_err() {
        return;
    }
    browser_close(port, Duration::from_secs(2));
    let deadline = Instant::now() + CLOSE_LIMIT;
    while Instant::now() < deadline && TcpStream::connect(("127.0.0.1", port)).is_ok() {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn create_profile(profile: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(profile)
        .map_err(|e| format!("browser profile: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("browser profile: {e}"))?;
    }
    Ok(())
}

/// `CEDIAN_CHROMIUM`, else the first installed Chrome or Chromium.
pub fn executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CEDIAN_CHROMIUM") {
        return Some(PathBuf::from(path));
    }
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/chromium",
        "/usr/bin/google-chrome",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

fn wait_for_port(port_file: &Path, child: &mut Child) -> Result<u16, String> {
    let deadline = Instant::now() + START_LIMIT;
    loop {
        if let Some(port) = std::fs::read_to_string(port_file)
            .ok()
            .and_then(|text| text.lines().next()?.trim().parse::<u16>().ok())
            && http_get(port, "/json/version").is_ok()
        {
            return Ok(port);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the browser exited before it was ready ({status})"));
        }
        if Instant::now() >= deadline {
            return Err("the browser did not open its debugging port".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One `GET` to the debugging port. Chromium answers only HTTP/1.1 with an
/// IP `Host`, and may keep the connection open past the body.
pub fn http_get(port: u16, path: &str) -> Result<String, String> {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("browser port: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
        if body_complete(&raw) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    if !text.starts_with("HTTP/1.1 200") {
        return Err(format!("browser answered {:?}", text.lines().next()));
    }
    text.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .ok_or_else(|| "malformed browser reply".to_string())
}

fn body_complete(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .is_some_and(|len| body.len() >= len)
}
