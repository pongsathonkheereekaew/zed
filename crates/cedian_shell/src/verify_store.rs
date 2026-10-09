//! Verification-profile ledger (ADR-0025):
//! `verify.json` in the state dir (ADR-0044), `snapshot_version` envelope (P3, fails closed).
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

fn verify_path(state_dir: &Path) -> PathBuf {
    state_dir.join("verify.json")
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

/// The ledger in `state_dir`; profile skills from `workdir`.
pub struct DiskProfileStore {
    pub state_dir: PathBuf,
    pub workdir: PathBuf,
}

impl ProfileStore for DiskProfileStore {
    fn load(&self) -> Result<ProfileLedger, String> {
        let Ok(raw) = std::fs::read_to_string(verify_path(&self.state_dir)) else {
            return Ok(ProfileLedger::default());
        };
        let version: Stored<serde::de::IgnoredAny> =
            serde_json::from_str(&raw).map_err(|e| format!("corrupt verify.json: {e}"))?;
        if version.snapshot_version != VERIFY_SNAPSHOT_VERSION {
            return Err(format!(
                "verification state too old (got v{}, want v{VERIFY_SNAPSHOT_VERSION}): \
                 delete {} and re-run the profile end to end",
                version.snapshot_version,
                verify_path(&self.state_dir).display()
            ));
        }
        let stored: Stored<ProfileLedger> =
            serde_json::from_str(&raw).map_err(|e| format!("corrupt verify.json: {e}"))?;
        Ok(stored.ledger)
    }

    fn save(&self, ledger: &ProfileLedger) -> Result<(), String> {
        let raw = serde_json::to_string_pretty(&Stored {
            snapshot_version: VERIFY_SNAPSHOT_VERSION,
            ledger,
        })
        .map_err(|e| e.to_string())?;
        std::fs::write(verify_path(&self.state_dir), raw).map_err(|e| e.to_string())
    }

    fn skill(&self, profile: &str) -> Option<String> {
        if !valid_profile(profile) {
            return None;
        }
        std::fs::read_to_string(
            self.workdir
                .join(".omp/skills")
                .join(profile)
                .join("SKILL.md"),
        )
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_reads_skills_and_rejects_escapes() {
        let d = crate::test_dir::TestDir::new("verify");
        std::fs::create_dir_all(d.join(".omp/skills/verify-notes")).unwrap();
        std::fs::write(d.join(".omp/skills/verify-notes/SKILL.md"), "# v").unwrap();
        let state = crate::test_dir::TestDir::new("verify-state");
        let store = DiskProfileStore {
            state_dir: state.to_path_buf(),
            workdir: d.to_path_buf(),
        };
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
        std::fs::write(verify_path(&state), r#"{"snapshot_version":0}"#).unwrap();
        assert!(store.load().unwrap_err().contains("too old"));
    }
}
