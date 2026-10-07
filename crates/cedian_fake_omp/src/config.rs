//! `omp config list | set | reset | path` stand-ins for the settings page
//! (ADR-0045). The global layer is `<agent dir>/config.yml` and the project
//! layer `.omp/config.yml` in the current directory; both hold a flat JSON
//! object of dotted keys (JSON is valid YAML, and cedian never parses them).
//! The agent dir is `$PI_CODING_AGENT_DIR`, else `$HOME/.omp/agent`.

use serde_json::{Map, Value, json};
use std::io::Write as _;
use std::path::PathBuf;

/// Fake OMP's settings: key, type, default.
const DEFAULTS: &[(&str, &str, &str)] = &[
    ("tools.approvalMode", "enum", r#""write""#),
    ("computer.enabled", "boolean", "false"),
    ("compaction.enabled", "boolean", "true"),
    ("display.theme", "string", r#""dark""#),
    ("modelRoles", "record", "{}"),
    ("tools.approval", "record", "{}"),
    ("bash.patterns", "array", "[]"),
];

fn agent_dir() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".omp/agent")
        })
}

fn read(path: PathBuf) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn effective(key: &str, default: Value) -> Value {
    let project = read(PathBuf::from(".omp/config.yml"));
    let global = read(agent_dir().join("config.yml"));
    project
        .get(key)
        .or_else(|| global.get(key))
        .cloned()
        .unwrap_or(default)
}

fn print(value: Value) -> i32 {
    i32::from(writeln!(std::io::stdout(), "{value}").is_err())
}

fn fail(message: &str) -> i32 {
    let _ = writeln!(std::io::stderr(), "Error: {message}");
    1
}

/// `None` when `args` is not one of these actions.
pub(crate) fn run(args: &[String]) -> Option<i32> {
    let action = args.get(1).map(String::as_str)?;
    let default = |key: &str| {
        DEFAULTS
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, kind, v)| {
                (
                    *kind,
                    serde_json::from_str::<Value>(v).unwrap_or(Value::Null),
                )
            })
    };
    Some(match action {
        "path" => i32::from(writeln!(std::io::stdout(), "{}", agent_dir().display()).is_err()),
        "list" => {
            let mut out = Map::new();
            for (key, kind, _) in DEFAULTS {
                let (_, value) = default(key).unwrap_or_default();
                out.insert(
                    (*key).into(),
                    json!({"type": kind, "description": "", "value": effective(key, value)}),
                );
            }
            print(Value::Object(out))
        }
        "set" | "reset" => {
            let Some(key) = args.get(2) else {
                return Some(2);
            };
            let Some((kind, fallback)) = default(key) else {
                return Some(fail(&format!("Unknown setting: {key}")));
            };
            let path = agent_dir().join("config.yml");
            let mut global = read(path.clone());
            if action == "set" {
                let raw = args.get(3).map(String::as_str).unwrap_or("");
                let value = match kind {
                    "boolean" => match raw {
                        "true" => json!(true),
                        "false" => json!(false),
                        _ => return Some(fail(&format!("Invalid boolean value: {raw}"))),
                    },
                    "record" | "array" => match serde_json::from_str(raw) {
                        Ok(v) => v,
                        Err(_) => return Some(fail(&format!("Invalid JSON value: {raw}"))),
                    },
                    _ => json!(raw),
                };
                global.insert(key.clone(), value);
            } else {
                global.remove(key);
            }
            let _ = std::fs::create_dir_all(agent_dir());
            if std::fs::write(&path, Value::Object(global).to_string()).is_err() {
                return Some(fail("cannot write config.yml"));
            }
            let mut out = json!({"key": key, "value": effective(key, fallback)});
            if read(PathBuf::from(".omp/config.yml")).contains_key(key.as_str()) {
                out["overriddenBy"] = json!("project");
            }
            print(out)
        }
        _ => return None,
    })
}
