//! Host service registry (plan §11): capability advertisement.
//!
//! cedian announces services (`workspace`, `lsp`, `dap`, `browser_surface`,
//! `ios`); tools resolve a backend at runtime. Phase 4 owns `workspace` — the
//! rest register as known-but-unimplemented so capability queries fail with
//! "not yet implemented", never silent absence.

use serde::{Deserialize, Serialize};

/// Known host service ids (plan §11 `set_host_services` shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceId {
    Workspace,
    Lsp,
    Dap,
    BrowserSurface,
    Ios,
}

/// Advertisement state of one service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Live in this build.
    Ready,
    /// Known, lands in a later phase (caller-visible, never silent).
    Planned,
}

/// The host service table: which backends this cedian build serves.
#[derive(Debug, Clone)]
pub struct HostService {
    services: Vec<(ServiceId, ServiceState)>,
}

impl HostService {
    /// Phase 4 table: only `workspace` is live.
    pub fn phase4() -> Self {
        Self {
            services: vec![
                (ServiceId::Workspace, ServiceState::Ready),
                (ServiceId::Lsp, ServiceState::Planned),
                (ServiceId::Dap, ServiceState::Planned),
                (ServiceId::BrowserSurface, ServiceState::Planned),
                (ServiceId::Ios, ServiceState::Planned),
            ],
        }
    }

    /// Services to advertise (ready ones only — planned stays hidden from OMP
    /// until its phase lands, so the model never calls a stub).
    pub fn advertised(&self) -> Vec<ServiceId> {
        self.services
            .iter()
            .filter(|(_, s)| *s == ServiceState::Ready)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Query one service: `Ok` when ready, caller-visible `Err` when planned.
    pub fn require(&self, id: ServiceId) -> Result<(), String> {
        match self.services.iter().find(|(s, _)| *s == id) {
            Some((_, ServiceState::Ready)) => Ok(()),
            Some((_, ServiceState::Planned)) => {
                Err(format!("host service {id:?} not yet implemented"))
            }
            None => Err(format!("unknown host service {id:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_workspace_advertised() {
        let h = HostService::phase4();
        assert_eq!(h.advertised(), vec![ServiceId::Workspace]);
        assert!(h.require(ServiceId::Workspace).is_ok());
        assert!(h.require(ServiceId::Lsp).is_err());
    }
}
