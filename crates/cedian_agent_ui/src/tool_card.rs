//! Tool-card registry stub (plan Phase 2 needs cards for ≥ the Phase 3 tool
//! list; full registry with args rendering lands in Phase 3).
//!
//! Phase 2 contract: every OMP tool name maps to a card with a display title
//! and a normal-UX summary line — raw RPC JSON never reaches the transcript
//! (Phase 3 acceptance preview). Unknown tools get a generic card, never a
//! JSON dump.

/// Display metadata for one tool card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCardMeta {
    /// Human title, e.g. `Read file`.
    pub title: &'static str,
    /// One-line summary template hint, e.g. `path`.
    pub summary_hint: &'static str,
}

/// Card metadata for the Phase 3 tool list (plan Phase 3 acceptance set).
/// Unknown names fall back to `generic_card`.
pub fn card_for_tool(name: &str) -> ToolCardMeta {
    match name {
        "read" => ToolCardMeta {
            title: "Read file",
            summary_hint: "path",
        },
        "edit" => ToolCardMeta {
            title: "Edit file",
            summary_hint: "path + hunks",
        },
        "write" => ToolCardMeta {
            title: "Write file",
            summary_hint: "path",
        },
        "bash" => ToolCardMeta {
            title: "Run command",
            summary_hint: "command",
        },
        "grep" => ToolCardMeta {
            title: "Search",
            summary_hint: "pattern",
        },
        "glob" => ToolCardMeta {
            title: "Find files",
            summary_hint: "pattern",
        },
        "find" => ToolCardMeta {
            title: "Find",
            summary_hint: "query",
        },
        "lsp" => ToolCardMeta {
            title: "Language server",
            summary_hint: "operation",
        },
        "debug" => ToolCardMeta {
            title: "Debug",
            summary_hint: "operation",
        },
        "task" => ToolCardMeta {
            title: "Subagent task",
            summary_hint: "description",
        },
        "eval" => ToolCardMeta {
            title: "Evaluate",
            summary_hint: "code",
        },
        "browser" => ToolCardMeta {
            title: "Browser",
            summary_hint: "action",
        },
        "todo" => ToolCardMeta {
            title: "Todo",
            summary_hint: "update",
        },
        _ => generic_card(),
    }
}

/// Fallback card: renders name + status, never raw JSON.
/// `read`/`write` aimed at an `xd://<tool>` device (how OMP 18.6 exposes
/// host tools): `Some((tool, is_call))`, `is_call` = the write that runs it.
pub fn host_device<'a>(name: &str, preview: &'a str) -> Option<(&'a str, bool)> {
    let is_call = match name {
        "write" => true,
        "read" => false,
        _ => return None,
    };
    let device = preview.trim().strip_prefix("xd://")?;
    let device = device.split(['/', ' ', '?']).next().unwrap_or("");
    (!device.is_empty()).then_some((device, is_call))
}

pub fn generic_card() -> ToolCardMeta {
    ToolCardMeta {
        title: "Tool call",
        summary_hint: "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xd_devices_map_to_host_tools() {
        assert_eq!(
            host_device("write", "xd://cedian_apply_edit"),
            Some(("cedian_apply_edit", true))
        );
        assert_eq!(
            host_device("read", "xd://echo_host"),
            Some(("echo_host", false))
        );
        assert_eq!(host_device("write", "src/a.rs"), None);
        assert_eq!(host_device("bash", "xd://x"), None);
        assert_eq!(host_device("write", "xd://"), None);
    }

    #[test]
    fn known_tools_have_cards() {
        for name in [
            "read", "edit", "write", "bash", "grep", "glob", "find", "lsp", "debug", "task",
            "eval", "browser", "todo",
        ] {
            assert_ne!(
                card_for_tool(name).title,
                "Tool call",
                "{name} needs a card"
            );
        }
    }

    #[test]
    fn unknown_tool_never_dumps_json() {
        let meta = card_for_tool("some_future_tool_xyz");
        assert_eq!(meta.title, "Tool call");
    }
}
