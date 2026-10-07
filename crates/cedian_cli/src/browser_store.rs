//! Headless browser session head for the CLI (S4 Task 5).
//!
//! One-shot-per-invocation stopgap: each `cedian browser …` command spawns
//! its own headless Chrome (unique profile under `.cedian/chrome-<pid>-<nanos>`),
//! performs exactly one action, persists the updated [`BrowserHead`], and
//! exits — the Chrome child dies with the invocation (reaped on drop, never
//! reconnected). `status`/`dom`/`shot` re-read the head and respawn fresh
//! Chrome on the saved URL; only `url` and `seq` carry meaning across
//! invocations (`port`/`ws_url` record the last spawn for debugging).
//! `close` clears the head and sweeps stale `chrome-*` profile dirs.
//!
//! S9 handoff: the app shell keeps ONE live session (R1 user-input-preempts
//! needs a persistent page); this file dies then.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cedian_browser::{BrowserProcess, BrowserSession, CdpClient};

/// Chrome executable: system install (macOS bundle path, `google-chrome`
/// elsewhere — same convention as the `cedian_browser` live tests).
#[cfg(target_os = "macos")]
pub const CHROME_EXE: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
/// Chrome executable: system install (macOS bundle path, `google-chrome`
/// elsewhere — same convention as the `cedian_browser` live tests).
#[cfg(not(target_os = "macos"))]
pub const CHROME_EXE: &str = "google-chrome";

/// Last browser action: where the saved page lives and its frame seq.
/// Persisted as JSON at `.cedian/browser.json` so CLI invocations share it
/// (same pattern as `workflow_store`; hand-rolled `serde_json::Value`
/// mapping so this harness needs no `serde` derive dependency).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserHead {
    /// Debug port of the spawn that wrote this head (stale after exit).
    pub port: u16,
    /// Page-level CDP URL of the spawn that wrote this head (stale after exit).
    pub ws_url: String,
    /// Page URL: the next `dom`/`shot` respawns fresh Chrome here.
    pub url: String,
    /// Session seq after the last action (freshness key for evidence).
    pub seq: u64,
}

/// `browser.json` schema version (ADR-0016 / P3). Bump on any shape change.
pub const BROWSER_SNAPSHOT_VERSION: u32 = 1;

fn browser_path(workdir: &Path) -> PathBuf {
    workdir.join(".cedian").join("browser.json")
}

/// Load the head, or fail with a usage hint when no session was opened.
pub fn load(workdir: &Path) -> Result<BrowserHead, String> {
    let raw = std::fs::read_to_string(browser_path(workdir))
        .map_err(|_| "no browser session: run `cedian browser open <url>` first".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("corrupt browser.json: {e}"))?;
    let version = v
        .get("snapshot_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version != u64::from(BROWSER_SNAPSHOT_VERSION) {
        return Err(format!(
            "browser state too old (got v{version}, want v{BROWSER_SNAPSHOT_VERSION}), \
             re-baseline: run `cedian browser open <url>`"
        ));
    }
    let port = v
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| "corrupt browser.json: bad `port`".to_string())?;
    let str_field = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("corrupt browser.json: bad `{key}`"))
    };
    Ok(BrowserHead {
        port,
        ws_url: str_field("ws_url")?,
        url: str_field("url")?,
        seq: v
            .get("seq")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "corrupt browser.json: bad `seq`".to_string())?,
    })
}

/// Save the head (creates `.cedian/` like the other stores).
pub fn save(workdir: &Path, head: &BrowserHead) -> Result<(), String> {
    std::fs::create_dir_all(workdir.join(".cedian")).map_err(|e| e.to_string())?;
    let raw = serde_json::to_string_pretty(&serde_json::json!({
        "snapshot_version": BROWSER_SNAPSHOT_VERSION,
        "port": head.port,
        "ws_url": head.ws_url,
        "url": head.url,
        "seq": head.seq,
    }))
    .map_err(|e| e.to_string())?;
    std::fs::write(browser_path(workdir), raw).map_err(|e| e.to_string())
}

/// Delete the head (fresh `open` overwrites anyway; explicit for `close`).
pub fn clear(workdir: &Path) {
    let _ = std::fs::remove_file(browser_path(workdir));
}

/// Spawn a FRESH headless Chrome on a unique profile, connect over the
/// page-level CDP socket, and navigate to `url` (30s deadline).
///
/// Returns the process, the session, and the page-level WebSocket URL the
/// session is attached to (recorded into [`BrowserHead`] for debugging).
///
/// The returned [`BrowserProcess`] MUST stay alive while the session is
/// used — bind it (e.g. `let (_proc, ..) = …`) so its `Drop` (child
/// kill + profile sweep) runs only after the action completes.
pub fn spawn_fresh(
    workdir: &Path,
    url: &str,
) -> Result<(BrowserProcess, BrowserSession, String), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let profile = workdir
        .join(".cedian")
        .join(format!("chrome-{}-{nanos}", std::process::id()));
    let proc = match BrowserProcess::spawn(CHROME_EXE, &profile) {
        Ok(proc) => proc,
        Err(e) => {
            // Spawn failed before `Drop` could own the child: sweep the
            // profile dir ourselves (best-effort) so a retry never trips
            // on a stale SingletonLock.
            let _ = std::fs::remove_dir_all(&profile);
            return Err(e.to_string());
        }
    };
    // Later failures drop `proc` → child killed + profile removed.
    // One page per spawn, so this is the target the session attaches to.
    let ws_url = proc.page_ws_url().map_err(|e| e.to_string())?;
    let client = CdpClient::connect(&ws_url).map_err(|e| e.to_string())?;
    let session =
        BrowserSession::open(client, url, Duration::from_secs(30)).map_err(|e| e.to_string())?;
    Ok((proc, session, ws_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip() {
        let dir = std::env::temp_dir().join("cedian-browser-store-test");
        let _ = std::fs::create_dir_all(&dir);
        let state = BrowserHead {
            port: 9222,
            ws_url: "ws://127.0.0.1:9222/devtools/page/1".into(),
            url: "about:blank".into(),
            seq: 1,
        };
        save(&dir, &state).unwrap();
        assert_eq!(load(&dir).unwrap().url, "about:blank");
        clear(&dir);
        assert!(load(&dir).is_err());
    }

    #[test]
    fn unversioned_head_fails_closed() {
        let dir = std::env::temp_dir().join(format!("cedian-browser-stale-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".cedian")).unwrap();
        std::fs::write(
            browser_path(&dir),
            r#"{"port":9222,"ws_url":"ws://x","url":"about:blank","seq":1}"#,
        )
        .unwrap();
        assert!(load(&dir).unwrap_err().contains("too old"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
