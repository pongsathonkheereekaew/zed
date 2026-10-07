//! Verification-profile ledger for the CLI harness (ADR-0025):
//! `.cedian/verify.json`, `snapshot_version` envelope (P3, fails closed).
//! Profile skills are read from `.omp/skills/<profile>/SKILL.md` — the
//! user's OMP config, never written here (§77).

use cedian_workflow::{ProfileLedger, ProfileStore};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `verify.json` schema version.
pub const VERIFY_SNAPSHOT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Stored<S> {
    #[serde(default)]
    snapshot_version: u32,
    #[serde(flatten)]
    ledger: S,
}

fn verify_path(workdir: &Path) -> PathBuf {
    workdir.join(".cedian").join("verify.json")
}

/// `verify-<app>` with a plain name: no path can escape `.omp/skills/`.
fn valid_profile(name: &str) -> bool {
    name.strip_prefix("verify-").is_some_and(|app| {
        !app.is_empty()
            && app
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

pub struct DiskProfileStore(pub PathBuf);

impl ProfileStore for DiskProfileStore {
    fn load(&self) -> Result<ProfileLedger, String> {
        let Ok(raw) = std::fs::read_to_string(verify_path(&self.0)) else {
            return Ok(ProfileLedger::default());
        };
        let version: Stored<serde::de::IgnoredAny> =
            serde_json::from_str(&raw).map_err(|e| format!("corrupt verify.json: {e}"))?;
        if version.snapshot_version != VERIFY_SNAPSHOT_VERSION {
            return Err(format!(
                "verification state too old (got v{}, want v{VERIFY_SNAPSHOT_VERSION}): \
                 delete .cedian/verify.json and re-run the profile end to end",
                version.snapshot_version
            ));
        }
        let stored: Stored<ProfileLedger> =
            serde_json::from_str(&raw).map_err(|e| format!("corrupt verify.json: {e}"))?;
        Ok(stored.ledger)
    }

    fn save(&self, ledger: &ProfileLedger) -> Result<(), String> {
        std::fs::create_dir_all(self.0.join(".cedian")).map_err(|e| e.to_string())?;
        let raw = serde_json::to_string_pretty(&Stored {
            snapshot_version: VERIFY_SNAPSHOT_VERSION,
            ledger,
        })
        .map_err(|e| e.to_string())?;
        std::fs::write(verify_path(&self.0), raw).map_err(|e| e.to_string())
    }

    fn skill(&self, profile: &str) -> Option<String> {
        if !valid_profile(profile) {
            return None;
        }
        std::fs::read_to_string(self.0.join(".omp/skills").join(profile).join("SKILL.md")).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_reads_skills_and_rejects_escapes() {
        let d = std::env::temp_dir().join(format!("cedian-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".omp/skills/verify-notes")).unwrap();
        std::fs::write(d.join(".omp/skills/verify-notes/SKILL.md"), "# v").unwrap();
        let store = DiskProfileStore(d.clone());
        assert_eq!(store.load().unwrap(), ProfileLedger::default());
        let ledger = ProfileLedger {
            seq: 7,
            ..ProfileLedger::default()
        };
        store.save(&ledger).unwrap();
        assert_eq!(store.load().unwrap().seq, 7);
        assert_eq!(store.skill("verify-notes").as_deref(), Some("# v"));
        assert_eq!(store.skill("verify-../../etc"), None);
        assert_eq!(store.skill("bug-fix"), None);
        std::fs::write(verify_path(&d), r#"{"snapshot_version":0}"#).unwrap();
        assert!(store.load().unwrap_err().contains("too old"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
