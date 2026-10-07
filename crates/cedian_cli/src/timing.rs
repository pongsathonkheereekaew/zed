//! Per-turn timing for the speed benchmark (ADR-0037): with `CEDIAN_TIMING`
//! set to a file path, each spawn and each turn appends one JSON line.
//! cedian overhead = spawn + context + post; `omp_ms` is OMP's own time
//! (prompt sent → `prompt_result`).

use serde_json::Value;
use std::io::Write as _;

pub const TIMING_ENV: &str = "CEDIAN_TIMING";

pub fn record(row: Value) {
    let Ok(path) = std::env::var(TIMING_ENV) else {
        return;
    };
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{row}"));
    if let Err(e) = written {
        eprintln!("cedian: {TIMING_ENV}={path}: {e}");
    }
}

pub fn ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
