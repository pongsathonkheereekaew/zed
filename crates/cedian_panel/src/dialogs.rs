//! One OMP dialog waiting on the person (S9 U4, §63): an approval or other
//! `select`, a `confirm`, an `input` or `editor`, or an `ask`. The panel
//! renders it and answers through [`crate::omp_link::OmpLink::answer`], which
//! sends the reply and writes its audit row.

use cedian_agent_ui::{AskDialog, AskError};
use cedian_omp::UserAnswer;
use editor::Editor;
use gpui::{App, AppContext as _, Entity, Window};
use omp_rpc::{AskAnswer, ExtensionUiRequest};

pub struct OpenDialog {
    request: ExtensionUiRequest,
    /// Selection state of an `ask`.
    ask: Option<AskDialog>,
    /// The free-text box of each `ask` question, in question order.
    custom: Vec<Entity<Editor>>,
    /// The text box of an `input` or `editor`.
    text: Option<Entity<Editor>>,
    /// Why the last answer did not go through.
    pub error: Option<String>,
}

impl OpenDialog {
    /// `None` for a request that expects no answer.
    pub fn new(request: ExtensionUiRequest, window: &mut Window, cx: &mut App) -> Option<Self> {
        let (id, _) = cedian_omp::dialog::dialog(&request)?;
        let ask = AskDialog::from_request(id, &request);
        let custom = ask
            .iter()
            .flat_map(|ask| ask.questions())
            .map(|_| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("Or type an answer", window, cx);
                    editor
                })
            })
            .collect();
        let text = match &request {
            ExtensionUiRequest::Input(r) => Some(cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                if let Some(placeholder) = &r.placeholder {
                    editor.set_placeholder_text(placeholder, window, cx);
                }
                editor
            })),
            ExtensionUiRequest::Editor(r) => Some(cx.new(|cx| {
                let mut editor = Editor::auto_height(1, 12, window, cx);
                editor.set_text(r.prefill.clone().unwrap_or_default(), window, cx);
                editor
            })),
            _ => None,
        };
        Some(Self {
            request,
            ask,
            custom,
            text,
            error: None,
        })
    }

    pub fn request(&self) -> &ExtensionUiRequest {
        &self.request
    }

    pub fn title(&self) -> &str {
        cedian_omp::dialog::dialog(&self.request).map_or("", |(_, title)| title)
    }

    pub fn ask(&self) -> Option<&AskDialog> {
        self.ask.as_ref()
    }

    pub fn custom_box(&self, question: usize) -> Option<&Entity<Editor>> {
        self.custom.get(question)
    }

    pub fn text_box(&self) -> Option<&Entity<Editor>> {
        self.text.as_ref()
    }

    /// Pick `label` for question `question_id` of an `ask`: a single-choice
    /// question takes it in place of the last pick, a multi-choice one
    /// toggles it.
    pub fn toggle_option(&mut self, question_id: &str, label: &str) {
        let Some(question) = self.ask.as_mut().and_then(|a| a.question_mut(question_id)) else {
            return;
        };
        if let Some(at) = question.selected.iter().position(|s| s == label) {
            question.selected.remove(at);
        } else if question.multi {
            question.selected.push(label.to_string());
        } else {
            question.selected = vec![label.to_string()];
        }
    }

    /// The answer the Submit button sends: the typed text of an `input` or
    /// `editor`, or every `ask` answer checked against its question.
    pub fn submission(&self, cx: &App) -> Result<UserAnswer, String> {
        if let Some(text) = &self.text {
            return Ok(UserAnswer::Text(text.read(cx).text(cx)));
        }
        // A copy, so an answer OMP never received can be corrected and sent again.
        let mut ask = self.ask.clone().ok_or("this dialog has no Submit")?;
        for (question, editor) in self.custom.iter().enumerate() {
            let id = ask.questions()[question].id.clone();
            if let Some(q) = ask.question_mut(&id) {
                q.custom_input = editor.read(cx).text(cx);
            }
        }
        let answers = ask.answer().map_err(|e| {
            match e {
                AskError::TooManySelections => "pick one option",
                AskError::UnknownOption => "pick one of the listed options",
                AskError::OptionPlusCustom => "pick an option or type an answer, not both",
                AskError::NotPending => "already answered",
            }
            .to_string()
        })?;
        Ok(UserAnswer::Answers(
            answers
                .into_iter()
                .map(|a| AskAnswer {
                    id: a.id,
                    selected_options: a.selected,
                    custom_input: a.custom_input,
                })
                .collect(),
        ))
    }
}
