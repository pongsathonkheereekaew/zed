//! Palette registry: every agent action discoverable (plan §2.5).
//!
//! Headless: action id + title + hint. GPUI binds keybindings + fuzzy filter
//! with the fork (reusing kit primitives, never hand-rolled dispatch). The CLI
//! harness maps these ids to subcommands — same ids transfer to the palette.

/// One palette entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteAction {
    /// Stable id (CLI subcommand + future command-palette id).
    pub id: &'static str,
    pub title: &'static str,
    pub hint: &'static str,
}

/// Every agent action, in palette order. New actions register HERE — a missing
/// entry means undiscoverable, which is a bug (§2.5 rule).
pub const ACTIONS: &[PaletteAction] = &[
    PaletteAction {
        id: "prompt",
        title: "Ask OMP",
        hint: "new turn with ambient context",
    },
    PaletteAction {
        id: "abort",
        title: "Abort turn",
        hint: "stop the running turn",
    },
    PaletteAction {
        id: "review",
        title: "Review changes",
        hint: "baseline → current hunks",
    },
    PaletteAction {
        id: "accept",
        title: "Accept hunk",
        hint: "mark (keep buffer)",
    },
    PaletteAction {
        id: "reject",
        title: "Reject hunk",
        hint: "inverse patch to baseline",
    },
    PaletteAction {
        id: "accept-all",
        title: "Accept all",
        hint: "skips unattributed",
    },
    PaletteAction {
        id: "state",
        title: "Session state",
        hint: "model, streaming, queue",
    },
];

/// The palette: prefix-filter over the registry.
#[derive(Debug, Default)]
pub struct Palette;

impl Palette {
    /// All actions.
    pub fn all() -> &'static [PaletteAction] {
        ACTIONS
    }

    /// Filter by id/title substring (case-insensitive). Empty query → all.
    pub fn filter(query: &str) -> Vec<&'static PaletteAction> {
        let q = query.to_lowercase();
        ACTIONS
            .iter()
            .filter(|a| q.is_empty() || a.id.contains(&q) || a.title.to_lowercase().contains(&q))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_commands_all_registered() {
        for cmd in [
            "prompt",
            "review",
            "accept",
            "reject",
            "accept-all",
            "state",
        ] {
            assert!(
                ACTIONS.iter().any(|a| a.id == cmd),
                "{cmd} missing from palette"
            );
        }
    }

    #[test]
    fn filter_matches() {
        assert_eq!(Palette::filter("acc").len(), 2);
        assert_eq!(Palette::filter("").len(), ACTIONS.len());
    }
}
