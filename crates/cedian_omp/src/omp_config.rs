//! OMP's own settings, read and written through OMP's CLI (ADR-0040,
//! ADR-0045). cedian never parses or edits OMP's YAML: values come from
//! `omp config list --json`, writes go through `omp config set` / `reset`,
//! and the layer behind each value is derived by comparing three reads.

use crate::{OmpError, scrub_env};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where an effective value comes from (ADR-0045 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Default,
    Global,
    Project,
    /// The spawn overlay pins this key for cedian's OMP (ADR-0040 decision 4).
    Cedian,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Global => "global",
            Self::Project => "project",
            Self::Cedian => "set by cedian",
        }
    }
}

/// One key as `omp config list --json` describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: String,
    pub description: String,
    /// `None` when OMP redacts it (secrets) or has no value.
    pub value: Option<Value>,
}

/// One key with its effective value and layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub key: String,
    pub entry: Entry,
    pub layer: Layer,
}

impl Setting {
    /// Booleans, enums, numbers and strings are edited in place; records
    /// and arrays are not (ADR-0045 decision 3).
    pub fn editable(&self) -> bool {
        matches!(
            self.entry.kind.as_str(),
            "boolean" | "enum" | "number" | "string"
        )
    }
}

/// What `omp config set` / `reset` reported.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOutcome {
    pub value: Value,
    /// The higher layer that still wins, when one does.
    pub overridden_by: Option<String>,
}

/// OMP's config CLI for one binary.
#[derive(Debug, Clone)]
pub struct OmpConfig {
    binary: PathBuf,
    timeout: Duration,
}

impl OmpConfig {
    pub fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            timeout: Duration::from_secs(10),
        }
    }

    /// Every key's effective value for `cwd`.
    pub fn list(&self, cwd: &Path) -> Result<BTreeMap<String, Entry>, OmpError> {
        parse_list(&self.run(cwd, &["list", "--json"], &[])?)
    }

    /// OMP's defaults: a list against an empty agent directory.
    pub fn defaults(&self, empty_dir: &Path) -> Result<BTreeMap<String, Entry>, OmpError> {
        let dir = empty_dir.to_string_lossy();
        let out = self.run(
            empty_dir,
            &["list", "--json"],
            &[("PI_CODING_AGENT_DIR", dir.as_ref())],
        )?;
        parse_list(&out)
    }

    /// The directory OMP keeps its global `config.yml` in.
    pub fn dir(&self, cwd: &Path) -> Result<PathBuf, OmpError> {
        let out = self.run(cwd, &["path"], &[])?;
        Ok(PathBuf::from(out.trim()))
    }

    pub fn set(&self, cwd: &Path, key: &str, value: &str) -> Result<WriteOutcome, OmpError> {
        check_key(key)?;
        parse_write(&self.run(cwd, &["set", key, value, "--json"], &[])?)
    }

    pub fn reset(&self, cwd: &Path, key: &str) -> Result<WriteOutcome, OmpError> {
        check_key(key)?;
        parse_write(&self.run(cwd, &["reset", key, "--json"], &[])?)
    }

    /// Run `omp config <args>` in `cwd` with the scrubbed env plus `extra`.
    /// A failure carries OMP's own message.
    fn run(&self, cwd: &Path, args: &[&str], extra: &[(&str, &str)]) -> Result<String, OmpError> {
        use std::io::Read as _;
        crate::spawn_profile::check_binary(&self.binary)?;
        let mut env = scrub_env(std::env::vars());
        if !extra.is_empty() {
            // An explicit agent dir is only honoured without a profile.
            env.retain(|(k, _)| k != "OMP_PROFILE" && !extra.iter().any(|(e, _)| e == k));
            env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        }
        let mut child = std::process::Command::new(&self.binary)
            .arg("config")
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| OmpError::Spawn(format!("omp config: {e}")))?;
        let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
            std::thread::spawn(move || {
                let mut out = String::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_string(&mut out);
                }
                out
            })
        };
        let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
        let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
        let deadline = std::time::Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| OmpError::Spawn(e.to_string()))?
            {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(OmpError::Timeout {
                    command: format!("config {}", args.join(" ")),
                    after: Some(self.timeout),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let out = stdout.join().unwrap_or_default();
        if !status.success() {
            let err = stderr.join().unwrap_or_default();
            let message = [err.trim(), out.trim()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("no output")
                .lines()
                .next()
                .unwrap_or_default()
                .trim_start_matches("Error: ")
                .to_string();
            return Err(OmpError::Command {
                command: format!("config {}", args.join(" ")),
                error: message,
                code: None,
            });
        }
        Ok(out)
    }
}

fn check_key(key: &str) -> Result<(), OmpError> {
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(OmpError::InvalidSpawnProfile(format!(
            "bad config key {key:?}"
        )));
    }
    Ok(())
}

fn parse_list(out: &str) -> Result<BTreeMap<String, Entry>, OmpError> {
    let parsed: Map<String, Value> =
        serde_json::from_str(out).map_err(|e| OmpError::Spawn(format!("omp config list: {e}")))?;
    Ok(parsed
        .into_iter()
        .map(|(key, v)| {
            let text = |f: &str| v.get(f).and_then(Value::as_str).unwrap_or("").to_string();
            let entry = Entry {
                kind: text("type"),
                description: text("description"),
                value: v.get("value").cloned(),
            };
            (key, entry)
        })
        .collect())
}

fn parse_write(out: &str) -> Result<WriteOutcome, OmpError> {
    let v: Value =
        serde_json::from_str(out).map_err(|e| OmpError::Spawn(format!("omp config: {e}")))?;
    Ok(WriteOutcome {
        value: v.get("value").cloned().unwrap_or(Value::Null),
        overridden_by: v
            .get("overriddenBy")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Every dotted key the overlay sets, parents included
/// (`tools.approval.bash` → `tools`, `tools.approval`, `tools.approval.bash`).
pub fn overlay_keys(overlay: &Value) -> BTreeSet<String> {
    fn walk(prefix: &str, value: &Value, out: &mut BTreeSet<String>) {
        if let Value::Object(map) = value {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out.insert(key.clone());
                walk(&key, v, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk("", overlay, &mut out);
    out
}

/// Join the three reads into one layer per key (ADR-0045 decision 2).
/// `pinned` holds the overlay's keys: a key pinned there, or with a pinned
/// child, is cedian's for cedian's OMP.
pub fn layered(
    effective: BTreeMap<String, Entry>,
    global: &BTreeMap<String, Entry>,
    defaults: &BTreeMap<String, Entry>,
    pinned: &BTreeSet<String>,
) -> Vec<Setting> {
    effective
        .into_iter()
        .map(|(key, entry)| {
            let value = |m: &BTreeMap<String, Entry>| m.get(&key).and_then(|e| e.value.clone());
            let layer = if pinned.contains(&key) {
                Layer::Cedian
            } else if entry.value != value(global) {
                Layer::Project
            } else if value(global) != value(defaults) {
                Layer::Global
            } else {
                Layer::Default
            };
            Setting { key, entry, layer }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(kind: &str, value: Value) -> Entry {
        Entry {
            kind: kind.into(),
            description: String::new(),
            value: Some(value),
        }
    }

    #[test]
    fn each_value_names_the_layer_that_supplies_it() {
        let defaults = BTreeMap::from([
            ("a".to_string(), entry("boolean", json!(false))),
            ("b".to_string(), entry("enum", json!("write"))),
            ("c".to_string(), entry("string", json!("x"))),
            (
                "tools.approvalMode".to_string(),
                entry("enum", json!("write")),
            ),
        ]);
        let mut global = defaults.clone();
        global.insert("b".into(), entry("enum", json!("yolo")));
        let mut effective = global.clone();
        effective.insert("c".into(), entry("string", json!("project")));
        let pinned = overlay_keys(&json!({"tools": {"approvalMode": "write"}}));
        let layers: BTreeMap<_, _> = layered(effective, &global, &defaults, &pinned)
            .into_iter()
            .map(|s| (s.key, s.layer))
            .collect();
        assert_eq!(layers["a"], Layer::Default);
        assert_eq!(layers["b"], Layer::Global);
        assert_eq!(layers["c"], Layer::Project);
        assert_eq!(layers["tools.approvalMode"], Layer::Cedian);
    }

    #[test]
    fn overlay_keys_include_parents() {
        let keys = overlay_keys(
            &json!({"tools": {"approval": {"bash": "prompt"}}, "computer": {"enabled": false}}),
        );
        for k in [
            "tools",
            "tools.approval",
            "tools.approval.bash",
            "computer.enabled",
        ] {
            assert!(keys.contains(k), "{k}");
        }
    }

    #[test]
    fn writes_report_the_layer_that_still_wins() {
        let out =
            parse_write(r#"{"key":"tools.approvalMode","value":"write","overriddenBy":"project"}"#)
                .unwrap();
        assert_eq!(out.overridden_by.as_deref(), Some("project"));
        assert!(check_key("modelRoles").is_ok());
        assert!(check_key("a; rm -rf /").is_err());
    }
}
