//! Message + tool-card view models (plan Phase 2 `streaming markdown`,
//! `tool calls`, `tool outputs`). Render projections over `Thread` events —
//! no markdown parsing here (GPUI renderer tokenizes at paint time).

use cedian_agent::{ThreadEvent, ToolCallStatus};

/// Render role of one message row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Thinking,
}

/// One rendered row: role + text + streaming flag. Thinking is a SEPARATE row
/// from the answer text (collapsible in the panel, hidden in compact mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageModel {
    pub role: MessageRole,
    pub text: String,
    pub streaming: bool,
}

/// Tool card status (mirrors `ToolCallStatus`, adds display-only `Stale` for
/// Phase 5 review — kept here so cards render it without a second enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCardStatus {
    Running,
    Done,
    Error,
    Interrupted,
}

/// One tool card: title + status + args preview + result summary (Phase 3:
/// both human one-liners, never raw JSON).
#[derive(Debug, Clone)]
pub struct ToolCard {
    pub call_id: String,
    pub name: String,
    pub title: String,
    pub status: ToolCardStatus,
    pub preview: String,
    pub summary: String,
}

impl ToolCard {
    /// New running card with the args preview.
    pub fn running(call_id: &str, name: &str, title: &str, preview: &str) -> Self {
        Self {
            call_id: call_id.to_string(),
            name: name.to_string(),
            title: title.to_string(),
            status: ToolCardStatus::Running,
            preview: preview.to_string(),
            summary: String::new(),
        }
    }

    /// Status transition; only `Running` cards move (terminal states stick).
    pub fn set_status(&mut self, status: ToolCardStatus) {
        if self.status == ToolCardStatus::Running {
            self.status = status;
        }
    }

    /// Fill the result summary (at `ToolEnd`).
    pub fn set_summary(&mut self, summary: &str) {
        self.summary = summary.to_string();
    }

    /// Single display line: `Title — preview → summary` (parts omitted when empty).
    pub fn display_line(&self) -> String {
        let mut line = self.title.clone();
        if !self.preview.is_empty() {
            line.push_str(" — ");
            line.push_str(&self.preview);
        }
        if !self.summary.is_empty() {
            line.push_str(" → ");
            line.push_str(&self.summary);
        }
        line
    }
}

impl From<ToolCallStatus> for ToolCardStatus {
    fn from(s: ToolCallStatus) -> Self {
        match s {
            ToolCallStatus::Running => Self::Running,
            ToolCallStatus::Done => Self::Done,
            ToolCallStatus::Error => Self::Error,
            ToolCallStatus::Interrupted => Self::Interrupted,
        }
    }
}

/// Project a thread's events into render rows: user/assistant rows, thinking
/// rows (non-empty only), tool cards, turn markers skipped (status line owns
/// them), queue chips appended as a single system row.
pub fn render_thread(events: &[ThreadEvent]) -> (Vec<MessageModel>, Vec<ToolCard>) {
    let mut messages = Vec::new();
    let mut cards = Vec::new();
    for event in events {
        match event {
            ThreadEvent::User { text } => {
                messages.push(MessageModel {
                    role: MessageRole::User,
                    text: text.clone(),
                    streaming: false,
                });
            }
            ThreadEvent::Assistant {
                text,
                thinking,
                streaming,
                ..
            } => {
                if !thinking.is_empty() {
                    messages.push(MessageModel {
                        role: MessageRole::Thinking,
                        text: thinking.clone(),
                        streaming: *streaming,
                    });
                }
                messages.push(MessageModel {
                    role: MessageRole::Assistant,
                    text: text.clone(),
                    streaming: *streaming,
                });
            }
            ThreadEvent::Tool {
                call_id,
                name,
                status,
                preview,
                summary,
            } => {
                let mut card = match crate::host_device(name, preview) {
                    // OMP 18.6 mounts host tools as `xd://<tool>`: `write` runs
                    // the tool (card named after it), `read` fetches its docs.
                    Some((device, true)) => ToolCard::running(call_id, device, "Host tool", device),
                    Some((device, false)) => {
                        ToolCard::running(call_id, name, "Read host tool docs", device)
                    }
                    None => {
                        ToolCard::running(call_id, name, crate::card_for_tool(name).title, preview)
                    }
                };
                card.status = (*status).into();
                card.summary = summary.clone();
                cards.push(card);
            }
            ThreadEvent::Queue {
                steering,
                follow_up,
            } => {
                if !steering.is_empty() || !follow_up.is_empty() {
                    messages.push(MessageModel {
                        role: MessageRole::User,
                        text: format!("queued: {} / {}", steering.join(", "), follow_up.join(", ")),
                        streaming: false,
                    });
                }
            }
            ThreadEvent::TurnEnd { .. } => {}
        }
    }
    (messages, cards)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_splits_into_own_row() {
        let events = vec![ThreadEvent::Assistant {
            message_id: "m".to_string(),
            text: "answer".to_string(),
            thinking: "hmm".to_string(),
            streaming: true,
        }];
        let (msgs, _) = render_thread(&events);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::Thinking);
        assert_eq!(msgs[1].role, MessageRole::Assistant);
    }

    #[test]
    fn terminal_card_status_sticks() {
        let mut card = ToolCard::running("c", "read", "Read file", "a.rs");
        card.set_status(ToolCardStatus::Done);
        card.set_status(ToolCardStatus::Running);
        assert_eq!(card.status, ToolCardStatus::Done);
        assert_eq!(card.display_line(), "Read file — a.rs");
        card.set_summary("42 lines");
        assert_eq!(card.display_line(), "Read file — a.rs → 42 lines");
    }

    #[test]
    fn xd_host_tool_cards_named_after_the_tool() {
        let tool = |call_id: &str, name: &str| ThreadEvent::Tool {
            call_id: call_id.to_string(),
            name: name.to_string(),
            status: ToolCallStatus::Done,
            preview: "xd://cedian_apply_edit".to_string(),
            summary: String::new(),
        };
        let (_, cards) = render_thread(&[tool("r", "read"), tool("w", "write")]);
        // Reading the device docs is not an edit: name stays `read`.
        assert_eq!(
            (cards[0].name.as_str(), cards[0].title.as_str()),
            ("read", "Read host tool docs")
        );
        assert_eq!(cards[1].name, "cedian_apply_edit");
        assert_eq!(cards[1].display_line(), "Host tool — cedian_apply_edit");
    }
}
