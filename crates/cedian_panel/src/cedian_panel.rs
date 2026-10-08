//! cedian inside the Zed fork (S9a spike): the dock panel that talks to OMP
//! through the cedian headless crates, and the agent-edit import.

mod context;
pub mod dialogs;
pub mod import;
pub mod omp_link;
pub mod omp_settings;
pub mod panel;
pub mod review;

pub use omp_settings::OmpSettings;
pub use panel::{CedianPanel, Connection, DIALOG_TIMEOUT, ToggleFocus, Turn, init};
