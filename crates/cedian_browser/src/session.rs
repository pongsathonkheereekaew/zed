//! Browser session: open, screenshot, DOM, frame seq (S4 Task 3).
//!
//! Drives one page over a [`CdpClient`]: `open` navigates, `screenshot`
//! captures a PNG, `dom` snapshots URL/title/HTML. Every navigation sets
//! `frame_id` and bumps `seq`; [`Shot`] records both so evidence can prove
//! freshness via the pure [`is_stale`] check (plan §29 R4: stale-frame
//! requires re-capture).
//!
//! Blocking/sync like the rest of the crate. `&self` methods use interior
//! mutability for the CDP client so callers never need `mut`. Chrome child
//! lifecycle stays with [`BrowserProcess`]; this type only drives pages.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::cdp::{CdpClient, CdpEvent};
use crate::process::BrowserError;

/// Screenshot capture: PNG path + the CDP frame it came from + session seq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    /// `{dir}/shot-{seq}.png`, written by [`BrowserSession::screenshot`].
    pub path: PathBuf,
    /// CDP frame the pixels belong to (evidence key, plan §29 R4).
    pub frame_id: String,
    /// Session seq at capture; stale once [`BrowserSession::current_seq`]
    /// moves past it (see [`is_stale`]).
    pub seq: u64,
}

/// DOM snapshot: current URL, title, and (possibly truncated) outer HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomSnapshot {
    pub url: String,
    pub title: String,
    /// `document.documentElement.outerHTML`, cut to `max_bytes` (see below).
    pub html: String,
}

/// Owned CDP session over one page: open → shot/dom.
///
/// Connects to the PAGE-level WebSocket (`BrowserProcess::page_ws_url`):
/// page targets own an active page so screenshots work. Browser-level
/// flatten attach has no active page ("Not attached to an active page").
pub struct BrowserSession {
    client: RefCell<CdpClient>,
    frame_id: RefCell<String>,
    seq: Cell<u64>,
}

impl BrowserSession {
    /// Navigate the attached page to `url` and wait up to `timeout` for
    /// `LoadEventFired` (a `frameId` in the navigate result or a
    /// `FrameNavigated` event short-circuits the wait). The client MUST be
    /// connected to a page-level WebSocket.
    ///
    /// No `Page.enable`: on a page-level socket, enabling the Page domain
    /// detaches the page for screenshots ("Not attached to an active page",
    /// observed Chrome 154). Navigation + load events arrive without it.
    pub fn open(client: CdpClient, url: &str, timeout: Duration) -> Result<Self, BrowserError> {
        let session = BrowserSession {
            client: RefCell::new(client),
            frame_id: RefCell::new(String::new()),
            seq: Cell::new(0),
        };
        let result = session.call("Page.navigate", serde_json::json!({ "url": url }), timeout)?;
        // Settle: the surface can lag load by ~a frame; capture retries
        // inside `screenshot` cover stragglers, this covers the common case.
        std::thread::sleep(Duration::from_secs(2));
        if let Some(frame_id) = result.get("frameId").and_then(|v| v.as_str()) {
            *session.frame_id.borrow_mut() = frame_id.to_string();
        }
        session.wait_for_load(timeout)?;
        // One navigation per open → exactly one bump.
        session.seq.set(session.seq.get() + 1);
        Ok(session)
    }

    /// Capture a PNG into `{dir}/shot-{seq}.png`.
    ///
    /// Late frame navigations drain after the capture, so a racing
    /// navigation bumps `seq` past the returned shot (making it read as
    /// stale, correctly). The shot keeps the pre-drain `seq`/`frame_id`
    /// the pixels actually belong to.
    pub fn screenshot(&self, dir: &Path) -> Result<Shot, BrowserError> {
        // Let the compositor settle: a capture issued the instant load fires
        // can hit "Not attached to an active page" before the surface exists.
        self.note_navigations();
        if self.frame_id.borrow().is_empty() {
            std::thread::sleep(Duration::from_millis(500));
            self.note_navigations();
        }
        let result = self.call(
            "Page.captureScreenshot",
            serde_json::json!({ "format": "png", "fromSurface": true }),
            Duration::from_secs(10),
        )?;
        let data = result.get("data").and_then(|v| v.as_str()).ok_or_else(|| {
            BrowserError::Handshake("captureScreenshot: missing data".to_string())
        })?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| BrowserError::Handshake(format!("screenshot base64: {e:?}")))?;
        let seq = self.seq.get();
        let frame_id = self.frame_id.borrow().clone();
        std::fs::create_dir_all(dir)
            .map_err(|e| BrowserError::Handshake(format!("screenshot mkdir: {e:?}")))?;
        let path = dir.join(format!("shot-{seq}.png"));
        std::fs::write(&path, &bytes)
            .map_err(|e| BrowserError::Handshake(format!("screenshot write: {e:?}")))?;
        self.note_navigations();
        Ok(Shot {
            path,
            frame_id,
            seq,
        })
    }

    /// Snapshot URL/title/outer HTML, truncating HTML to `max_bytes` with a
    /// `…[truncated N bytes]` suffix when over budget.
    pub fn dom(&self, max_bytes: usize) -> Result<DomSnapshot, BrowserError> {
        let result = self.call(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "({url:location.href,title:document.title,html:document.documentElement.outerHTML})",
                "returnByValue": true,
            }),
            Duration::from_secs(10),
        )?;
        // `returnByValue` nests the object at result.result.value.
        let value = &result["result"]["value"];
        let value = if value.is_object() { value } else { &result };
        let get = |key: &str| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let url = get("url");
        let title = get("title");
        let html = truncate_bytes(&get("html"), max_bytes);
        self.note_navigations();
        Ok(DomSnapshot { url, title, html })
    }

    /// Current navigation seq; compare with [`Shot::seq`] via [`is_stale`].
    pub fn current_seq(&self) -> u64 {
        self.seq.get()
    }

    /// One CDP call over the page socket (no session id needed — the socket
    /// IS the page). Maps failures into [`BrowserError`]. (Debug-formatted
    /// so this file never depends on the sibling's `Display` impls.)
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, BrowserError> {
        self.client
            .borrow_mut()
            .call(method, params, timeout)
            .map_err(|e| BrowserError::Handshake(format!("{method}: {e:?}")))
    }

    /// Wait until load fires or a frame id is known; error on deadline with
    /// neither. Each poll re-reads the socket so queued `FrameNavigated` /
    /// `LoadEventFired` events keep flowing.
    fn wait_for_load(&self, timeout: Duration) -> Result<(), BrowserError> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut loaded = false;
            for event in self.client.borrow_mut().drain_events() {
                match event {
                    CdpEvent::FrameNavigated { frame_id } => {
                        *self.frame_id.borrow_mut() = frame_id;
                    }
                    CdpEvent::LoadEventFired => loaded = true,
                }
            }
            if loaded || !self.frame_id.borrow().is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Handshake(format!(
                    "navigation timed out after {}s",
                    timeout.as_secs()
                )));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let poll = remaining.min(Duration::from_millis(500));
            // Poll failures just mean "no news yet"; the deadline above owns
            // timeout errors.
            let _ = self.call("Page.getFrameTree", serde_json::json!({}), poll);
        }
    }

    /// Record late frame navigations (bump seq); drop load events, which
    /// carry no frame identity.
    fn note_navigations(&self) {
        for event in self.client.borrow_mut().drain_events() {
            if let CdpEvent::FrameNavigated { frame_id } = event {
                *self.frame_id.borrow_mut() = frame_id;
                self.seq.set(self.seq.get() + 1);
            }
        }
    }
}

/// Pure staleness check: a shot is stale once the session advanced past it.
pub fn is_stale(shot_seq: u64, current_seq: u64) -> bool {
    shot_seq < current_seq
}

/// Cut `html` to `max_bytes` at a char boundary, noting skipped bytes.
fn truncate_bytes(html: &str, max_bytes: usize) -> String {
    if html.len() <= max_bytes {
        return html.to_string();
    }
    let mut end = max_bytes;
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let skipped = html.len() - end;
    format!("{}…[truncated {skipped} bytes]", &html[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::CdpClient;
    use crate::process::BrowserProcess;

    #[cfg(target_os = "macos")]
    const CHROME_EXE: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
    #[cfg(not(target_os = "macos"))]
    const CHROME_EXE: &str = "google-chrome";

    const TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn stale_when_session_advanced() {
        assert!(!is_stale(3, 3));
        assert!(is_stale(2, 3));
    }

    #[test]
    #[ignore]
    fn open_shot_dom_roundtrip() {
        let dir = std::env::temp_dir().join(format!("cedian-browser-sess-{}", std::process::id()));
        let proc = BrowserProcess::spawn(CHROME_EXE, &dir.join("profile")).unwrap();
        let page_url = proc.page_ws_url().unwrap();
        let client = CdpClient::connect(&page_url).unwrap();
        let session = BrowserSession::open(
            client,
            "data:text/html,<title>hi</title><h1>hello</h1>",
            TIMEOUT,
        )
        .unwrap();
        let shot = session.screenshot(&dir).unwrap();
        assert!(shot.path.exists());
        assert!(!shot.frame_id.is_empty());
        let dom = session.dom(4096).unwrap();
        assert!(dom.title == "hi");
        assert!(dom.html.contains("hello"));
        assert!(!is_stale(shot.seq, session.current_seq()));
    }
}
