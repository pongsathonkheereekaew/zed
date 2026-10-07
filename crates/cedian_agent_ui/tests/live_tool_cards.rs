//! Phase 3 live smoke: tool cards render without raw JSON.
//!
//! Plan acceptance: "Normal UX shows no raw RPC JSON or ugly terminal
//! transcript for ordinary tools." Drives a prompt that uses `read` + `bash`
//! through `OmpRuntime` → `Panel`, then asserts every rendered tool card has a
//! human preview/summary and no `{`/`"` JSON leakage.
//!
//! Requires ambient OMP auth. Ignored by default:
//! `cargo test -p cedian_agent_ui -- --ignored --nocapture live_tool_cards`

use cedian_agent_ui::{Panel, render_thread};
use cedian_omp::{OmpBinary, OmpRuntime, RouterEvent, RuntimeConfig};
use std::time::Duration;

fn dev_config(tag: &str) -> RuntimeConfig {
    RuntimeConfig {
        binary: OmpBinary::Path("omp".to_string()),
        session_dir: std::env::temp_dir()
            .join(format!("cedian-phase3-{tag}-{}", std::process::id())),
        sessions: cedian_omp::Sessions::InSessionDir,
        cwd: std::env::temp_dir(),
        ask_dialog: true,
        prompt_timeout: Duration::from_secs(240),
        // bash prompts under the spawn profile; allow exactly this probe (bash.patterns).
        policy: cedian_omp::SpawnPolicy {
            bash_patterns: vec![cedian_omp::BashRule {
                pattern: "wc -l *".to_string(),
                approval: cedian_omp::ToolPolicy::Allow,
            }],
            ..Default::default()
        },
    }
}

#[test]
#[ignore]
fn live_tool_cards() {
    let mut rt = OmpRuntime::spawn(dev_config("cards")).expect("spawn");
    let mut panel = Panel::new();
    let id = panel.new_task("cards", std::env::temp_dir());
    let router = rt.router();
    let (_sub, rx) = router.subscribe();

    // Deterministic workspace: known files for read + grep.
    let work = std::env::temp_dir().join(format!("cedian-phase3-work-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("alpha.txt"), "line one\nline two\nline three\n").unwrap();

    let prompt = format!(
        "Read the file {} with the read tool, then run `wc -l` on it with the bash tool, then reply with only the word: cards-ok",
        work.join("alpha.txt").display()
    );
    let turn = rt.prompt(&prompt, vec![]).expect("prompt");
    assert_eq!(turn.assistant_text.as_deref(), Some("cards-ok"));

    for event in rx.iter().take(300) {
        let done = matches!(event, RouterEvent::Settled);
        panel.dispatch(&event);
        if done {
            break;
        }
    }

    let task = panel.get(&id).unwrap();
    let (_messages, cards) = render_thread(task.thread().events());
    assert!(cards.len() >= 2, "read + bash cards, got {}", cards.len());

    for card in &cards {
        let line = card.display_line();
        assert!(
            !line.contains('{') && !line.contains("}}"),
            "no JSON in {line:?}"
        );
        assert!(
            !card.preview.contains('"'),
            "no quoted JSON in {:?}",
            card.preview
        );
        eprintln!("CARD: {line}");
    }
    // read card names the file; bash card names the command.
    assert!(
        cards
            .iter()
            .any(|c| c.name == "read" && c.preview.contains("alpha.txt"))
    );
    assert!(
        cards
            .iter()
            .any(|c| c.name == "bash" && c.preview.contains("wc"))
    );

    std::fs::remove_dir_all(&work).ok();
    rt.shutdown().expect("shutdown");
}
