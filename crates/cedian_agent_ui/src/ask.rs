//! `ask` dialog model (plan §63): native dialog state for the OMP `ask` tool.
//!
//! Wire shape: `extension_ui_request{method:ask, questions[{id,question,
//! options[],multi?,recommended?}], timeout?}` with strictly-ordered answers.
//! Liveness (§63 R3): a pending dialog is a LEASE, not a lock — disconnect or
//! timeout resolves `Abstain` (system-generated, never a user answer) and the
//! task goes `blocked` until reconnect. Strict-wins (§64) applies to the
//! ANSWER at the cedian gate, not to liveness: cedian may always dismiss a
//! dead dialog.

use omp_rpc::{AskQuestion, ExtensionUiRequest};

/// One rendered question: labels + recommended index + free-text always offered.
#[derive(Debug, Clone)]
pub struct AskQuestionModel {
    pub id: String,
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<String>,
    pub multi: bool,
    /// Recommended option index (upstream `recommended?`).
    pub recommended: Option<usize>,
    /// Selected labels (exact option labels, no duplicates).
    pub selected: Vec<String>,
    /// Free-text answer (trimmed, ignored when empty).
    pub custom_input: String,
}

impl AskQuestionModel {
    fn from_wire(q: &AskQuestion) -> Self {
        Self {
            id: q.id.clone(),
            question: q.question.clone(),
            header: q.header.clone(),
            options: q.options.iter().map(|o| o.label.clone()).collect(),
            multi: q.multi,
            recommended: q.recommended.and_then(|i| usize::try_from(i).ok()),
            selected: Vec::new(),
            custom_input: String::new(),
        }
    }

    /// Validate one answer per the wire contract: single-select takes at most
    /// one option and not both an option and custom input; exact labels only.
    fn validate(&self) -> Result<AskAnswer, AskError> {
        if !self.multi && self.selected.len() > 1 {
            return Err(AskError::TooManySelections);
        }
        for label in &self.selected {
            if !self.options.contains(label) {
                return Err(AskError::UnknownOption);
            }
        }
        let custom = self.custom_input.trim();
        if !self.multi && !self.selected.is_empty() && !custom.is_empty() {
            return Err(AskError::OptionPlusCustom);
        }
        Ok(AskAnswer {
            id: self.id.clone(),
            selected: self.selected.clone(),
            custom_input: if custom.is_empty() {
                None
            } else {
                Some(custom.to_string())
            },
        })
    }
}

/// One validated answer (strictly ordered by question in the response).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskAnswer {
    pub id: String,
    pub selected: Vec<String>,
    pub custom_input: Option<String>,
}

/// Dialog lifecycle: pending → answered | abstained (lease expiry/disconnect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskState {
    Pending,
    Answered,
    /// System-generated: timeout (default 5 min) or disconnect. Never a user
    /// answer; logged in the audit tuple; task goes `blocked` until reconnect.
    Abstained,
}

/// One `ask` tool call as a native dialog.
#[derive(Debug)]
pub struct AskDialog {
    request_id: String,
    questions: Vec<AskQuestionModel>,
    state: AskState,
}

impl AskDialog {
    /// Build from an `ask` extension UI request. Returns `None` for any other
    /// method (caller routes those elsewhere).
    pub fn from_request(request_id: &str, request: &ExtensionUiRequest) -> Option<Self> {
        match request {
            ExtensionUiRequest::Ask(ask) => Some(Self {
                request_id: request_id.to_string(),
                questions: ask
                    .questions
                    .iter()
                    .map(AskQuestionModel::from_wire)
                    .collect(),
                state: AskState::Pending,
            }),
            _ => None,
        }
    }

    /// Answer the dialog: validates every question in order, flips to
    /// `Answered`. Returns the ordered answers for the wire response.
    pub fn answer(&mut self) -> Result<Vec<AskAnswer>, AskError> {
        if self.state != AskState::Pending {
            return Err(AskError::NotPending);
        }
        let mut answers = Vec::with_capacity(self.questions.len());
        for q in &self.questions {
            answers.push(q.validate()?);
        }
        self.state = AskState::Answered;
        Ok(answers)
    }

    /// Lease expiry: timeout or disconnect → `Abstained`. Always allowed from
    /// `Pending` (liveness over strictness); answering afterwards fails.
    pub fn abstain(&mut self) {
        if self.state == AskState::Pending {
            self.state = AskState::Abstained;
        }
    }

    /// Mutable access to one question's selection (panel binds checkboxes).
    pub fn question_mut(&mut self, id: &str) -> Option<&mut AskQuestionModel> {
        self.questions.iter_mut().find(|q| q.id == id)
    }

    pub fn state(&self) -> AskState {
        self.state
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn questions(&self) -> &[AskQuestionModel] {
        &self.questions
    }
}

/// Ask answer errors (rendered inline in the dialog, never silent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskError {
    TooManySelections,
    UnknownOption,
    OptionPlusCustom,
    NotPending,
}

#[cfg(test)]
mod tests {
    use super::*;
    use omp_rpc::{AskOption, AskUiRequest};

    fn dialog() -> AskDialog {
        let req = ExtensionUiRequest::Ask(AskUiRequest {
            id: "r1".to_string(),
            questions: vec![
                AskQuestion {
                    id: "db".to_string(),
                    question: "Which database?".to_string(),
                    options: vec![
                        AskOption {
                            label: "Postgres".to_string(),
                            description: None,
                            preview: None,
                        },
                        AskOption {
                            label: "SQLite".to_string(),
                            description: None,
                            preview: None,
                        },
                    ],
                    header: None,
                    multi: false,
                    recommended: Some(1),
                },
                AskQuestion {
                    id: "feat".to_string(),
                    question: "Features?".to_string(),
                    options: vec![AskOption {
                        label: "Auth".to_string(),
                        description: None,
                        preview: None,
                    }],
                    header: None,
                    multi: true,
                    recommended: None,
                },
            ],
            timeout: None,
        });
        AskDialog::from_request("r1", &req).unwrap()
    }

    #[test]
    fn valid_answers_in_order() {
        let mut d = dialog();
        d.question_mut("db")
            .unwrap()
            .selected
            .push("SQLite".to_string());
        let answers = d.answer().unwrap();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].id, "db");
        assert_eq!(d.state(), AskState::Answered);
    }

    #[test]
    fn single_select_rejects_option_plus_custom() {
        let mut d = dialog();
        let q = d.question_mut("db").unwrap();
        q.selected.push("SQLite".to_string());
        q.custom_input = "DuckDB".to_string();
        assert_eq!(d.answer(), Err(AskError::OptionPlusCustom));
    }

    #[test]
    fn abstain_lease_then_answer_fails() {
        let mut d = dialog();
        d.abstain();
        assert_eq!(d.state(), AskState::Abstained);
        assert_eq!(d.answer(), Err(AskError::NotPending));
    }
}
