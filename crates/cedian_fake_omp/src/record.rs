//! Record mode: transparent proxy to the real OMP, teeing both directions.

use crate::fixture::{Dir, Placeholders, Record};
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

pub(crate) fn run(
    real: &Path,
    args: &[String],
    out: &Path,
    cwd: &Path,
    placeholders: &Placeholders,
) -> i32 {
    let file = match File::create(out) {
        Ok(f) => Arc::new(Mutex::new(f)),
        Err(e) => {
            eprintln!("fake-omp record: cannot create {}: {e}", out.display());
            return 2;
        }
    };
    let mut child = match Command::new(real)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fake-omp record: cannot spawn {}: {e}", real.display());
            return 2;
        }
    };
    let (Some(mut child_in), Some(child_out)) = (child.stdin.take(), child.stdout.take()) else {
        eprintln!("fake-omp record: child pipes missing");
        return 2;
    };

    let tee = {
        let placeholders = placeholders.clone();
        move |file: &Mutex<File>, dir: Dir, line: &str| {
            let Ok(frame) = serde_json::from_str::<Value>(line) else {
                eprintln!("fake-omp record: non-JSON line skipped");
                return;
            };
            if frame.get("type").and_then(Value::as_str) == Some("rpc_chunk") {
                eprintln!("fake-omp record: rpc_chunk recorded raw (payload not redacted)");
            }
            let record = Record {
                dir,
                frame: placeholders.redact_value(&frame),
            };
            if let Ok(mut f) = file.lock() {
                let _ = writeln!(f, "{}", record.to_line());
                let _ = f.flush();
            }
        }
    };

    // Host → OMP. Ends when the host closes stdin; detached otherwise.
    {
        let file = Arc::clone(&file);
        let tee = tee.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                tee(&file, Dir::In, &line);
                if writeln!(child_in, "{line}")
                    .and_then(|()| child_in.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    // OMP → host, until OMP closes stdout. Around each tool call, the
    // files that tool wrote are recorded as `fs` frames just before its
    // `tool_execution_end` (see `fs_effects`).
    let mut stdout = std::io::stdout().lock();
    let mut files = crate::fs_effects::snapshot(cwd);
    for line in BufReader::new(child_out).lines() {
        let Ok(line) = line else { break };
        let kind = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string));
        match kind.as_deref() {
            Some("tool_execution_start") => files = crate::fs_effects::snapshot(cwd),
            Some("tool_execution_end") => {
                let now = crate::fs_effects::snapshot(cwd);
                for frame in crate::fs_effects::diff(&files, &now) {
                    tee(&file, Dir::Fs, &frame.to_string());
                }
                files = now;
            }
            _ => {}
        }
        tee(&file, Dir::Out, &line);
        if writeln!(stdout, "{line}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
    child.wait().ok().and_then(|s| s.code()).unwrap_or(1)
}
