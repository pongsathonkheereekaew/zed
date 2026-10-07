//! The app's line to OMP (S9 U3): one OMP process per workspace, started the
//! way the CLI starts it (`cedian_shell::launch`, the user's `cedian.toml`,
//! the spawn profile) on its own thread, so a dying OMP never takes the IDE
//! with it. Sessions live where OMP's CLI keeps them for the project, so a
//! session started in either opens in the other (ADR-0040 decision 5) and a
//! restart's `open_session` adopts it; cedian's overlay lives in the
//! workspace's state dir (ADR-0044).

use cedian_omp::{OmpBinary, OmpRuntime, RouterEvent, RuntimeConfig, Sessions, SpawnPolicy};
use cedian_shell::{Policy, RunKind};
use futures::channel::mpsc::UnboundedSender;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// What the OMP thread tells the panel.
#[derive(Debug, Clone)]
pub enum LinkEvent {
    /// OMP is up and its session is open.
    Ready {
        session_id: String,
        /// The session's file in OMP's store, where the CLI finds it too.
        session_file: Option<String>,
        resumed: bool,
        /// Set under `policy = "omp"` (ADR-0035): OMP's config decides.
        policy_note: Option<String>,
    },
    /// One of OMP's events, `Disconnected` included.
    Event(RouterEvent),
    /// OMP could not start, or its session could not open.
    Failed(String),
}

/// Everything one launch needs, resolved before any process starts.
pub struct LaunchSpec {
    pub binary: PathBuf,
    pub workdir: PathBuf,
    /// cedian's directory for this OMP: the spawn overlay.
    pub overlay_dir: PathBuf,
    pub sessions: Sessions,
    pub policy: SpawnPolicy,
    pub policy_note: Option<String>,
}

impl LaunchSpec {
    /// Resolve settings, policy and paths for `workdir`. The app registers no
    /// host tools yet (U4, U5 add them).
    pub fn resolve(workdir: &Path) -> Result<Self, String> {
        let settings = cedian_shell::resolve_settings(workdir).map_err(|e| e.to_string())?;
        let binary = cedian_shell::launch::omp_binary()?;
        let choice = settings.policy_for(workdir, RunKind::Interactive);
        let mut policy = cedian_shell::launch::spawn_policy(&settings, choice.policy, &[]);
        let policy_note = match choice.policy {
            Policy::Cedian => {
                policy.config_allows = cedian_shell::launch::config_allows(&binary, workdir)?;
                None
            }
            Policy::Omp => Some("policy = \"omp\": OMP's own config decides approvals".to_string()),
        };
        Ok(Self {
            binary,
            workdir: workdir.to_path_buf(),
            overlay_dir: cedian_shell::state::dir(workdir)?.join("omp"),
            sessions: Sessions::OmpDefault,
            policy,
            policy_note,
        })
    }
}

/// A running OMP. Dropping it shuts OMP down (no `Disconnected` follows).
pub struct OmpLink {
    prompts: mpsc::Sender<String>,
    pid: Arc<AtomicU32>,
}

impl OmpLink {
    /// Start OMP on its own thread; everything it reports goes to `events`.
    /// Prompts sent before it is ready wait in order.
    pub fn start(spec: LaunchSpec, events: UnboundedSender<LinkEvent>) -> Self {
        let (prompts, prompt_rx) = mpsc::channel::<String>();
        let pid = Arc::new(AtomicU32::new(0));
        let thread_pid = Arc::clone(&pid);
        let spawned = std::thread::Builder::new()
            .name("cedian-omp".to_string())
            .spawn(move || run(spec, prompt_rx, events, thread_pid));
        if let Err(e) = spawned {
            log::error!("cedian: cannot start the OMP thread: {e}");
        }
        Self { prompts, pid }
    }

    pub fn send(&self, prompt: String) -> Result<(), String> {
        self.prompts
            .send(prompt)
            .map_err(|_| "OMP is not running".to_string())
    }

    /// OMP's process id, once it has started.
    pub fn pid(&self) -> Option<u32> {
        Some(self.pid.load(Ordering::Relaxed)).filter(|pid| *pid != 0)
    }
}

fn run(
    spec: LaunchSpec,
    prompts: mpsc::Receiver<String>,
    events: UnboundedSender<LinkEvent>,
    pid: Arc<AtomicU32>,
) {
    let mut runtime = match OmpRuntime::spawn(RuntimeConfig {
        binary: OmpBinary::Bundled(spec.binary),
        session_dir: spec.overlay_dir,
        sessions: spec.sessions,
        cwd: spec.workdir,
        ask_dialog: true,
        prompt_timeout: Duration::from_secs(600),
        policy: spec.policy,
    }) {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = events.unbounded_send(LinkEvent::Failed(format!("OMP did not start: {e}")));
            return;
        }
    };
    pid.store(runtime.pid().unwrap_or(0), Ordering::Relaxed);
    let (_sub, router_events) = runtime.router().subscribe();
    let forward = events.clone();
    std::thread::spawn(move || {
        for event in router_events {
            if forward.unbounded_send(LinkEvent::Event(event)).is_err() {
                break;
            }
        }
    });
    match runtime.open_session("app") {
        Ok(session) => {
            let _ = events.unbounded_send(LinkEvent::Ready {
                session_id: session.session_id,
                session_file: session.session_file,
                resumed: session.resumed,
                policy_note: spec.policy_note,
            });
        }
        Err(e) => {
            let _ = events.unbounded_send(LinkEvent::Failed(format!(
                "OMP's session did not open: {e}"
            )));
            let _ = runtime.shutdown();
            return;
        }
    }
    for prompt in prompts {
        if let Err(e) = runtime.prompt(&prompt, vec![]) {
            log::error!("cedian: OMP turn failed: {e}");
        }
    }
    let _ = runtime.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    fn ready(
        events: &mut futures::channel::mpsc::UnboundedReceiver<LinkEvent>,
    ) -> (String, bool, Option<String>) {
        futures::executor::block_on(async {
            while let Some(event) = events.next().await {
                match event {
                    LinkEvent::Ready {
                        session_id,
                        resumed,
                        session_file,
                        ..
                    } => return (session_id, resumed, session_file),
                    LinkEvent::Failed(e) => panic!("{e}"),
                    LinkEvent::Event(_) => {}
                }
            }
            panic!("OMP thread ended without a session")
        })
    }

    /// Real OMP keeps the app's session in its own store, where the CLI
    /// finds it (ADR-0040 decision 5), and adopts it on restart. OMP writes a
    /// session only once it has a message, so this sends one short prompt
    /// (one model call). The session folder it made is removed after.
    /// `cargo test -p cedian_panel -- --ignored live_`
    #[test]
    #[ignore]
    fn live_restart_adopts_the_same_omp_session() {
        let root = std::env::temp_dir().join(format!("cedian-u3-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ws")).unwrap();
        let root = root.canonicalize().unwrap();
        let spec = || LaunchSpec {
            binary: cedian_shell::launch::omp_binary().unwrap(),
            workdir: root.join("ws"),
            overlay_dir: root.join("overlay"),
            sessions: Sessions::OmpDefault,
            policy: SpawnPolicy::default(),
            policy_note: None,
        };
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let link = OmpLink::start(spec(), tx);
        let (first, resumed, file) = ready(&mut rx);
        assert!(!resumed);
        let file = PathBuf::from(file.expect("OMP names the session file"));
        let agent = cedian_omp::OmpConfig::new(spec().binary)
            .dir(&root)
            .unwrap();
        assert!(
            file.starts_with(agent.join("sessions")),
            "in OMP's own store: {}",
            file.display()
        );
        link.send("Reply with exactly: ok".to_string()).unwrap();
        futures::executor::block_on(async {
            while let Some(event) = rx.next().await {
                if matches!(event, LinkEvent::Event(RouterEvent::Settled)) {
                    break;
                }
            }
        });
        drop(link);
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let _link = OmpLink::start(spec(), tx);
        let (again, resumed, _) = ready(&mut rx);
        assert_eq!((again.as_str(), resumed), (first.as_str(), true));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}
