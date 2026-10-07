//! Answers to OMP's extension UI requests (approval dialogs, `ask`), and the
//! record each answer leaves for the audit log (§64 audit tuple).
//!
//! The app shows a dialog and the person answers it ([`user_answer`]). A
//! process with no UI must not leave one open: OMP waits on it until the
//! prompt deadline. It fails closed instead ([`headless_answer`]): deny an
//! approval, decline a confirm, dismiss everything else that expects an
//! answer (ADR-0012 strict-wins). A dialog nobody could answer, because OMP
//! died or withdrew it, is an [`abstained`] record (§63 lease, ADR-0013).
//! Fire-and-forget requests (notify, status, widget, …) need no reply.

use omp_rpc::wire::{
    AnswersUiResponse, AskAnswer, CancelUiResponse, ConfirmUiResponse, ExtensionUiRequest,
    ExtensionUiResponse, LitTrue, ValueUiResponse,
};

/// What cedian's gate did with a dialog (§64 audit tuple).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// The person answered: approved, confirmed, or gave the value asked for.
    Allow,
    /// The dialog's deny answer, a declined confirm, or a person's dismissal.
    Deny,
    /// Nobody could answer and nothing was chosen. System-generated only.
    Abstain,
}

impl GateDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Abstain => "abstain",
        }
    }
}

/// Who answered a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answerer {
    /// The person, in the app's dialog.
    User,
    /// cedian, with no UI to show it (fail closed) or no OMP left to answer.
    Cedian,
}

impl Answerer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Cedian => "cedian",
        }
    }
}

/// One dialog's outcome. `tool` is read from an OMP approval title
/// (`Allow tool: <name>`); `at_ms` is stamped when the reply was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogRecord {
    pub label: String,
    pub tool: Option<String>,
    pub decision: GateDecision,
    pub answered_by: Answerer,
    pub at_ms: u64,
}

/// A person's answer to one dialog, by kind.
#[derive(Debug, Clone, PartialEq)]
pub enum UserAnswer {
    /// One of a `select`'s options.
    Choice(String),
    /// A `confirm`'s yes or no.
    Confirm(bool),
    /// The text of an `input` or `editor`.
    Text(String),
    /// Every `ask` question, in question order.
    Answers(Vec<AskAnswer>),
    /// Closed without an answer.
    Dismiss,
}

/// The reply to `request` and its record, or `None` when the request
/// expects no reply or `answer` does not fit its kind.
pub fn user_answer(
    request: &ExtensionUiRequest,
    answer: UserAnswer,
) -> Option<(ExtensionUiResponse, DialogRecord)> {
    let (id, title) = dialog(request)?;
    let id = id.to_string();
    let record = |decision| record(title, decision, Answerer::User);
    Some(match (request, answer) {
        (_, UserAnswer::Dismiss) => (cancel(id), record(GateDecision::Deny)),
        (ExtensionUiRequest::Select(r), UserAnswer::Choice(value)) => {
            if !r.options.contains(&value) {
                return None;
            }
            let decision = if value.eq_ignore_ascii_case("deny") {
                GateDecision::Deny
            } else {
                GateDecision::Allow
            };
            (
                ExtensionUiResponse::ValueUiResponse(ValueUiResponse { id, value }),
                record(decision),
            )
        }
        (ExtensionUiRequest::Confirm(_), UserAnswer::Confirm(confirmed)) => (
            ExtensionUiResponse::ConfirmUiResponse(ConfirmUiResponse { id, confirmed }),
            record(if confirmed {
                GateDecision::Allow
            } else {
                GateDecision::Deny
            }),
        ),
        (ExtensionUiRequest::Input(_) | ExtensionUiRequest::Editor(_), UserAnswer::Text(value)) => {
            (
                ExtensionUiResponse::ValueUiResponse(ValueUiResponse { id, value }),
                record(GateDecision::Allow),
            )
        }
        (ExtensionUiRequest::Ask(_), UserAnswer::Answers(answers)) => (
            ExtensionUiResponse::AnswersUiResponse(AnswersUiResponse { id, answers }),
            record(GateDecision::Allow),
        ),
        _ => return None,
    })
}

/// The fail-closed reply to `request` and its record, or `None` when the
/// request expects no reply.
pub fn headless_answer(
    request: &ExtensionUiRequest,
) -> Option<(ExtensionUiResponse, DialogRecord)> {
    let (id, title) = dialog(request)?;
    let id = id.to_string();
    let record = |decision| record(title, decision, Answerer::Cedian);
    Some(match request {
        ExtensionUiRequest::Select(r) => {
            match r.options.iter().find(|o| o.eq_ignore_ascii_case("deny")) {
                Some(deny) => (
                    ExtensionUiResponse::ValueUiResponse(ValueUiResponse {
                        id,
                        value: deny.clone(),
                    }),
                    record(GateDecision::Deny),
                ),
                None => (cancel(id), record(GateDecision::Abstain)),
            }
        }
        ExtensionUiRequest::Confirm(_) => (
            ExtensionUiResponse::ConfirmUiResponse(ConfirmUiResponse {
                id,
                confirmed: false,
            }),
            record(GateDecision::Deny),
        ),
        _ => (cancel(id), record(GateDecision::Abstain)),
    })
}

/// The record of a dialog that was open when nobody could answer it any
/// more, or `None` when `request` expects no reply.
pub fn abstained(request: &ExtensionUiRequest) -> Option<DialogRecord> {
    let (_, title) = dialog(request)?;
    Some(record(title, GateDecision::Abstain, Answerer::Cedian))
}

/// Id and title of a request that expects a reply.
pub fn dialog(request: &ExtensionUiRequest) -> Option<(&str, &str)> {
    match request {
        ExtensionUiRequest::Select(r) => Some((&r.id, &r.title)),
        ExtensionUiRequest::Confirm(r) => Some((&r.id, &r.title)),
        ExtensionUiRequest::Input(r) => Some((&r.id, &r.title)),
        ExtensionUiRequest::Editor(r) => Some((&r.id, &r.title)),
        ExtensionUiRequest::Ask(r) => Some((&r.id, "ask")),
        _ => None,
    }
}

fn cancel(id: String) -> ExtensionUiResponse {
    ExtensionUiResponse::CancelUiResponse(CancelUiResponse {
        id,
        cancelled: LitTrue,
        timed_out: None,
    })
}

fn record(title: &str, decision: GateDecision, answered_by: Answerer) -> DialogRecord {
    DialogRecord {
        label: title.lines().collect::<Vec<_>>().join(" — "),
        tool: title
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("Allow tool: "))
            .map(|t| t.trim().to_string()),
        decision,
        answered_by,
        at_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(v: serde_json::Value) -> ExtensionUiRequest {
        ExtensionUiRequest::from_value(v).expect("valid request")
    }

    fn approval() -> ExtensionUiRequest {
        request(json!({
            "type": "extension_ui_request", "method": "select", "id": "u1",
            "title": "Allow tool: bash\nCommand: ls", "options": ["Approve", "Deny"]
        }))
    }

    #[test]
    fn approval_select_is_denied() {
        let (reply, record) = headless_answer(&approval()).unwrap();
        assert_eq!(
            reply,
            ExtensionUiResponse::ValueUiResponse(ValueUiResponse {
                id: "u1".into(),
                value: "Deny".into()
            })
        );
        assert_eq!(record.label, "Allow tool: bash — Command: ls");
        assert_eq!(record.tool.as_deref(), Some("bash"));
        assert_eq!(record.decision, GateDecision::Deny, "headless chose Deny");
        assert_eq!(record.answered_by, Answerer::Cedian);
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
        assert!(abstained(&n).is_none());
        assert!(user_answer(&n, UserAnswer::Dismiss).is_none());
    }

    #[test]
    fn select_without_deny_is_dismissed() {
        let r = request(json!({
            "type": "extension_ui_request", "method": "select", "id": "u4",
            "title": "Pick", "options": ["a", "b"]
        }));
        let (reply, record) = headless_answer(&r).unwrap();
        assert!(matches!(reply, ExtensionUiResponse::CancelUiResponse(_)));
        assert_eq!(
            record.decision,
            GateDecision::Abstain,
            "no one could answer, nothing was denied"
        );
        assert_eq!(record.tool, None);
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

    #[test]
    fn a_person_approving_is_an_allow_and_choosing_deny_a_deny() {
        let (reply, record) =
            user_answer(&approval(), UserAnswer::Choice("Approve".into())).unwrap();
        assert_eq!(
            reply,
            ExtensionUiResponse::ValueUiResponse(ValueUiResponse {
                id: "u1".into(),
                value: "Approve".into()
            })
        );
        assert_eq!(
            (record.decision, record.answered_by, record.tool.as_deref()),
            (GateDecision::Allow, Answerer::User, Some("bash"))
        );
        let (_, record) = user_answer(&approval(), UserAnswer::Choice("Deny".into())).unwrap();
        assert_eq!(record.decision, GateDecision::Deny);
    }

    #[test]
    fn a_person_dismissing_is_a_deny_never_an_abstain() {
        let (reply, record) = user_answer(&approval(), UserAnswer::Dismiss).unwrap();
        assert!(matches!(reply, ExtensionUiResponse::CancelUiResponse(_)));
        assert_eq!(
            record.decision,
            GateDecision::Deny,
            "abstain is system-generated only (§64)"
        );
    }

    #[test]
    fn an_answer_that_does_not_fit_the_dialog_is_refused() {
        assert!(user_answer(&approval(), UserAnswer::Choice("Maybe".into())).is_none());
        assert!(user_answer(&approval(), UserAnswer::Confirm(true)).is_none());
    }

    #[test]
    fn ask_answers_go_back_in_order_and_an_open_ask_abstains() {
        let ask = request(json!({
            "type": "extension_ui_request", "method": "ask", "id": "u7",
            "questions": [{"id": "db", "question": "Which?", "options": [{"label": "SQLite"}]}]
        }));
        let answers = vec![AskAnswer {
            id: "db".into(),
            selected_options: vec!["SQLite".into()],
            custom_input: None,
        }];
        let (reply, record) = user_answer(&ask, UserAnswer::Answers(answers.clone())).unwrap();
        assert_eq!(
            reply,
            ExtensionUiResponse::AnswersUiResponse(AnswersUiResponse {
                id: "u7".into(),
                answers
            })
        );
        assert_eq!(
            (record.label.as_str(), record.decision),
            ("ask", GateDecision::Allow)
        );
        let open = abstained(&ask).unwrap();
        assert_eq!(
            (open.decision, open.answered_by),
            (GateDecision::Abstain, Answerer::Cedian)
        );
    }
}
