//! How cedian starts OMP for a person at the keyboard: the user's
//! `cedian.toml` mapped onto the spawn profile (ADR-0020, ADR-0035), and the
//! ADR-0041 pin of OMP's own `tools.approval` allows. The CLI and the app
//! both start OMP through here.

use crate::{Policy, Settings, Verdict};
use cedian_omp::{ApprovalMode, Approvals, SpawnPolicy, ToolPolicy};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Map `[permissions]` onto the OMP spawn policy (ADR-0020). Under the
/// default policy it only tightens: `dangerous = allow` still leaves the exec
/// floor at `prompt` (strict-wins, ADR-0012), and yolo is unrepresentable.
/// Under `policy = "omp"` (ADR-0035) OMP's own config decides approvals;
/// cedian's denies still apply. `host_tools` is the whole host-tool set the
/// caller registers (ADR-0004), so the overlay allows exactly those names.
pub fn spawn_policy(
    settings: &Settings,
    policy_source: Policy,
    host_tools: &[&str],
) -> SpawnPolicy {
    let mut policy = SpawnPolicy::default();
    if policy_source == Policy::Omp {
        policy.approvals = Approvals::Omp;
    }
    for tool in host_tools {
        policy.host_tools.insert((*tool).to_string());
    }
    if settings.permissions.project_write != Verdict::Allow && policy.approvals != Approvals::Omp {
        policy.approvals = Approvals::Cedian(ApprovalMode::AlwaysAsk);
    }
    let mut deny = |tools: &[&str]| {
        for tool in tools {
            policy
                .tool_policies
                .insert((*tool).to_string(), ToolPolicy::Deny);
        }
    };
    if settings.permissions.project_write == Verdict::Deny {
        deny(&["edit", "write", "ast_edit"]);
    }
    if settings.permissions.dangerous == Verdict::Deny {
        deny(cedian_omp::spawn_profile::EXEC_TOOLS);
    }
    policy
}

/// The OMP binary: `CEDIAN_OMP_BINARY` (absolute; the hermetic lanes point
/// it at fake-omp), else the first `omp` on `PATH`.
pub fn omp_binary() -> Result<PathBuf, String> {
    match std::env::var("CEDIAN_OMP_BINARY") {
        Ok(path) => Ok(PathBuf::from(path)),
        Err(_) => cedian_omp::resolve_on_path("omp", std::env::var("PATH").ok().as_deref())
            .map_err(|e| e.to_string()),
    }
}

/// Tools OMP's merged config sets to `allow` in `workdir` (ADR-0041
/// decision 2). Fails closed: without the record the default profile cannot
/// pin them, so the spawn is refused.
pub fn config_allows(binary: &Path, workdir: &Path) -> Result<BTreeSet<String>, String> {
    let record =
        cedian_omp::omp_config_get(binary, workdir, "tools.approval", Duration::from_secs(10))
            .map_err(|e| format!("cannot read OMP's tools.approval, so cannot pin it: {e}"))?;
    let record = record
        .as_object()
        .ok_or("OMP's tools.approval is not a record")?;
    Ok(record
        .iter()
        .filter(|(_, v)| v.as_str() == Some("allow"))
        .map(|(k, _)| k.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_in_hands_approvals_to_omp_but_keeps_denies() {
        let mut settings = Settings::default();
        let default = spawn_policy(&settings, Policy::Cedian, &[]);
        assert_eq!(default.approvals, Approvals::Cedian(ApprovalMode::Write));
        assert_eq!(
            spawn_policy(&settings, Policy::Omp, &[]).approvals,
            Approvals::Omp
        );
        settings.permissions.project_write = Verdict::Deny;
        let opted = spawn_policy(&settings, Policy::Omp, &[]);
        assert_eq!(opted.approvals, Approvals::Omp);
        assert_eq!(opted.tool_policies.get("write"), Some(&ToolPolicy::Deny));
        assert_eq!(
            spawn_policy(&settings, Policy::Cedian, &[]).approvals,
            Approvals::Cedian(ApprovalMode::AlwaysAsk)
        );
    }

    #[test]
    fn host_tools_are_exactly_the_ones_named() {
        let policy = spawn_policy(&Settings::default(), Policy::Cedian, &["cedian_apply_edit"]);
        assert_eq!(
            policy
                .host_tools
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["cedian_apply_edit"]
        );
    }
}
