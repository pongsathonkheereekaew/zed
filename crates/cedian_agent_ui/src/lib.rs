//! `cedian_agent_ui`: headless UI models for the agent panel.
//!
//! Plan §7 layout (`panel.rs`, `composer.rs`, `message.rs`, `tool_card.rs`;
//! `workflow.rs` + `subagents.rs` land in their phases). Pure view-models over
//! `cedian_agent` — GPUI rendering binds when the Zed fork does. Batching
//! contract (§73): streaming deltas accumulate in [`TextDeltaBuffer`], flushed
//! at ~16–33ms; tests drive flushes explicitly.

pub mod ask;
pub mod composer;
pub mod message;
pub mod panel;
pub mod text_buffer;
pub mod tool_card;
pub use ask::{AskAnswer, AskDialog, AskQuestionModel, AskState};
pub use composer::{Composer, ComposerMode};
pub use message::{MessageModel, MessageRole, ToolCard, ToolCardStatus, render_thread};
pub use panel::{Panel, PanelTask};
pub use text_buffer::TextDeltaBuffer;
pub use tool_card::{ToolCardMeta, card_for_tool, generic_card, host_device};
