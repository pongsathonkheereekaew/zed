//! Headless answers to OMP's extension UI requests (approval dialogs, `ask`).
//!
//! A process with no UI must not leave a dialog open: OMP waits on it until
//! the prompt deadline. Fail closed instead — deny an approval, decline a
//! confirm, dismiss everything else that expects an answer — and record the
//! dialog title so the caller can say what was refused (ADR-0012 strict-wins).
//! Fire-and-forget requests (notify, status, widget, …) need no reply.

use omp_rpc::wire::{
    CancelUiResponse, ConfirmUiResponse, ExtensionUiRequest, ExtensionUiResponse, LitTrue,
    ValueUiResponse,
};

/// What cedian's gate did with a dialog it could not show (§64 audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Headless chose the dialog's deny answer or declined a confirm.
    Deny,
    /// Headless only dismissed it: nobody could answer, nothing was chosen.
    Abstain,
}

impl GateDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Abstain => "abstain",
        }
    }
}

/// One dialog headless answered. `tool` is read from an OMP approval title
/// (`Allow tool: <name>`); `at_ms` is stamped by the runtime when it replied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub label: String,
    pub tool: Option<String>,
    pub decision: GateDecision,
    pub at_ms: u64,
}

/// The fail-closed reply to `request` plus what it refused, or `None` when
/// the request expects no reply.
pub fn headless_answer(request: &ExtensionUiRequest) -> Option<(ExtensionUiResponse, Refusal)> {
    let cancel = |id: &str| {
        ExtensionUiResponse::CancelUiResponse(CancelUiResponse {
            id: id.to_string(),
            cancelled: LitTrue,
            timed_out: None,
        })
    };
    let refusal = |title: &str, decision| Refusal {
        label: title.lines().collect::<Vec<_>>().join(" — "),
        tool: title
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("Allow tool: "))
            .map(|t| t.trim().to_string()),
        decision,
        at_ms: 0,
    };
    match request {
        ExtensionUiRequest::Select(r) => Some(
            match r.options.iter().find(|o| o.eq_ignore_ascii_case("deny")) {
                Some(deny) => (
                    ExtensionUiResponse::ValueUiResponse(ValueUiResponse {
                        id: r.id.clone(),
                        value: deny.clone(),
                    }),
                    refusal(&r.title, GateDecision::Deny),
                ),
                None => (cancel(&r.id), refusal(&r.title, GateDecision::Abstain)),
            },
        ),
        ExtensionUiRequest::Confirm(r) => Some((
            ExtensionUiResponse::ConfirmUiResponse(ConfirmUiResponse {
                id: r.id.clone(),
                confirmed: false,
            }),
            refusal(&r.title, GateDecision::Deny),
        )),
        ExtensionUiRequest::Input(r) => {
            Some((cancel(&r.id), refusal(&r.title, GateDecision::Abstain)))
        }
        ExtensionUiRequest::Editor(r) => {
            Some((cancel(&r.id), refusal(&r.title, GateDecision::Abstain)))
        }
        ExtensionUiRequest::Ask(r) => Some((cancel(&r.id), refusal("ask", GateDecision::Abstain))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(v: serde_json::Value) -> ExtensionUiRequest {
        ExtensionUiRequest::from_value(v).expect("valid request")
    }

    #[test]
    fn approval_select_is_denied() {
        let r = request(json!({
            "type": "extension_ui_request", "method": "select", "id": "u1",
            "title": "Allow tool: bash\nCommand: ls", "options": ["Approve", "Deny"]
        }));
        let (reply, refusal) = headless_answer(&r).unwrap();
        assert_eq!(
            reply,
            ExtensionUiResponse::ValueUiResponse(ValueUiResponse {
                id: "u1".into(),
                value: "Deny".into()
            })
        );
        assert_eq!(refusal.label, "Allow tool: bash — Command: ls");
        assert_eq!(refusal.tool.as_deref(), Some("bash"));
        assert_eq!(refusal.decision, GateDecision::Deny, "headless chose Deny");
    }

    #[test]
    fn confirm_is_declined_and_notify_needs_no_reply() {
        let r = request(json!({
            "type": "extension_ui_request", "method": "confirm", "id": "u2",
            "title": "Run?", "message": "really"
        }));
        assert!(matches!(
            headless_answer(&r),
            Some((
                ExtensionUiResponse::ConfirmUiResponse(ConfirmUiResponse {
                    confirmed: false,
                    ..
                }),
                _
            ))
        ));
        let n = request(json!({
            "type": "extension_ui_request", "method": "notify", "id": "u3", "message": "hi"
        }));
        assert!(headless_answer(&n).is_none());
    }

    #[test]
    fn select_without_deny_is_dismissed() {
        let r = request(json!({
            "type": "extension_ui_request", "method": "select", "id": "u4",
            "title": "Pick", "options": ["a", "b"]
        }));
        let (reply, refusal) = headless_answer(&r).unwrap();
        assert!(matches!(reply, ExtensionUiResponse::CancelUiResponse(_)));
        assert_eq!(
            refusal.decision,
            GateDecision::Abstain,
            "no one could answer, nothing was denied"
        );
        assert_eq!(refusal.tool, None);
    }

    #[test]
    fn declined_confirm_is_a_deny_and_cancelled_input_an_abstain() {
        let confirm = request(json!({
            "type": "extension_ui_request", "method": "confirm", "id": "u5",
            "title": "Allow tool: eval", "message": "run python"
        }));
        assert_eq!(
            headless_answer(&confirm).unwrap().1.decision,
            GateDecision::Deny
        );
        let input = request(json!({
            "type": "extension_ui_request", "method": "input", "id": "u6", "title": "Name?"
        }));
        assert_eq!(
            headless_answer(&input).unwrap().1.decision,
            GateDecision::Abstain
        );
    }
}
