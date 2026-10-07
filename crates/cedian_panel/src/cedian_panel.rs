//! cedian inside the Zed fork (S9a spike): the dock panel that talks to OMP
//! through the cedian headless crates, and the agent-edit import.

pub mod import;
pub mod omp_link;
pub mod panel;
pub mod version;

pub use panel::{CedianPanel, Connection, ImportedEdit, ToggleFocus, init};
