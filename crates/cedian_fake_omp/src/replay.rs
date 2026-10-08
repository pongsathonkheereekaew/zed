//! Replay mode: play the server side of a fixture against a live host.
//!
//! Server (`out`) frames are emitted in recorded order. At each recorded host
//! (`in`) frame, replay blocks for the host's next line and checks its `type`
//! matches, and a dialog answer (and a prompt's text and recorded images,
//! and a host URI read's content) matches whole; the host's fresh request id
//! is mapped onto the recorded one so the recorded `response` frames
//! correlate.
//! Any divergence exits with [`crate::EXIT_DIVERGED`] — the host sees a
//! closed transport, never a hang.

use crate::fixture::{Dir, Placeholders, Record};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::Path,
};

pub(crate) fn run(fixture: &Path, cwd: &Path, placeholders: &Placeholders) -> i32 {
    let records = match load(fixture) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fake-omp replay: {e}");
            return 2;
        }
    };
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = std::io::stdout().lock();
    // recorded request id → this run's request id.
    let mut ids: HashMap<String, String> = HashMap::new();

    for (n, record) in records.iter().enumerate() {
        match record.dir {
            Dir::Fs => {
                let frame = placeholders.expand_value(&record.frame);
                match frame.get("type").and_then(Value::as_str) {
                    Some("fs_hold") => {
                        hold(cwd, &frame);
                        continue;
                    }
                    // A slow OMP: the next frame comes this much later.
                    Some("fs_sleep") => {
                        let ms = frame.get("ms").and_then(Value::as_u64).unwrap_or(0);
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        continue;
                    }
                    _ => {}
                }
                if let Err(e) = crate::fs_effects::apply(cwd, &frame) {
                    eprintln!("fake-omp replay: record {n}: {e}");
                    return crate::EXIT_DIVERGED;
                }
            }
            Dir::Out => {
                let mut frame = placeholders.expand_value(&record.frame);
                // A prompt's result names the prompt's request id too.
                if matches!(
                    frame.get("type").and_then(Value::as_str),
                    Some("response" | "prompt_result")
                ) {
                    remap_id(&mut frame, &ids);
                }
                if writeln!(stdout, "{frame}")
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    return 0; // host went away
                }
            }
            Dir::In => {
                let Some(Ok(line)) = lines.next() else {
                    return 0; // host closed stdin early: normal shutdown
                };
                let got: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("fake-omp replay: host sent non-JSON at record {n}: {e}");
                        return crate::EXIT_DIVERGED;
                    }
                };
                let got_type = got.get("type").and_then(Value::as_str);
                if got_type != record.frame_type() {
                    eprintln!(
                        "fake-omp replay: divergence at record {n}: expected {:?}, host sent {:?}",
                        record.frame_type(),
                        got_type
                    );
                    return crate::EXIT_DIVERGED;
                }
                if got_type == Some("extension_ui_response") {
                    let expected = placeholders.expand_value(&record.frame);
                    if got != expected {
                        eprintln!(
                            "fake-omp replay: divergence at record {n}: expected answer {expected}, host sent {got}"
                        );
                        return crate::EXIT_DIVERGED;
                    }
                }
                // A prompt must be the recorded one, not merely a prompt.
                let expected = placeholders.expand_value(&record.frame);
                if got_type == Some("prompt") && got.get("message") != expected.get("message") {
                    eprintln!(
                        "fake-omp replay: divergence at record {n}: expected prompt {:?}, host sent {:?}",
                        expected.get("message"),
                        got.get("message")
                    );
                    return crate::EXIT_DIVERGED;
                }
                // A host URI read must answer what the recording answered.
                if got_type == Some("host_uri_result") {
                    for field in ["content", "isError", "error"] {
                        if got.get(field) != expected.get(field) {
                            eprintln!(
                                "fake-omp replay: divergence at record {n}: expected {field} {:?}, host sent {:?}",
                                expected.get(field),
                                got.get(field)
                            );
                            return crate::EXIT_DIVERGED;
                        }
                    }
                }
                // Images the recording prompted with must reach OMP too.
                let expected = record.frame.get("images");
                if expected.is_some() && got.get("images") != expected {
                    eprintln!(
                        "fake-omp replay: divergence at record {n}: expected images {expected:?}, host sent {:?}",
                        got.get("images")
                    );
                    return crate::EXIT_DIVERGED;
                }
                if let (Some(rec), Some(now)) = (
                    record.frame.get("id").and_then(Value::as_str),
                    got.get("id").and_then(Value::as_str),
                ) {
                    ids.insert(rec.to_string(), now.to_string());
                }
            }
        }
    }

    // Fixture exhausted: refuse further commands explicitly until EOF.
    for line in lines {
        let Ok(line) = line else { break };
        let Ok(got) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = got.get("id").and_then(Value::as_str) else {
            continue;
        };
        let reply = json!({
            "type": "response",
            "id": id,
            "command": got.get("type").cloned().unwrap_or(Value::Null),
            "success": false,
            "error": "fake-omp: fixture exhausted",
        });
        if writeln!(stdout, "{reply}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
    0
}

/// `fs_hold`: a child in this process's group keeps `path` open for writing
/// and ignores SIGTERM, as an OMP slow to exit holds its session until the
/// host's SIGKILL.
fn hold(cwd: &Path, frame: &Value) {
    let Some(path) = frame.get("path").and_then(Value::as_str) else {
        return;
    };
    let spawned = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("trap '' TERM; exec 3>>\"$0\"; exec sleep 60")
        .arg(cwd.join(path))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        eprintln!("fake-omp replay: fs_hold: {e}");
    }
}

fn remap_id(frame: &mut Value, ids: &HashMap<String, String>) {
    let Some(id) = frame.get("id").and_then(Value::as_str) else {
        return;
    };
    if let Some(now) = ids.get(id) {
        frame["id"] = Value::String(now.clone());
    }
}

fn load(path: &Path) -> Result<Vec<Record>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(Record::parse)
        .collect()
}
