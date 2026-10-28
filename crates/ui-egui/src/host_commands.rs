//! The OneOS host bridge's half inside the window: the assistant's run (`craft:command {method: "command.batch"}`).
//!
//! A run is judged and recorded by `wordcraft_engine::host_commands` (the allow-list, the arguments no run may carry, `$step`
//! references, ONE undo step or none). Here every step goes through [`WordApp::run`], the one path the ribbon, the shortcuts and
//! the control channel use, so the interface follows (the caret, the panes, the status line) exactly as it does for the person.
//! A step that would open a dialog is refused and the run taken back: a dialog never sits over the person's work.

use serde_json::Value;
use wordcraft_engine::host_commands::run_batch;

use crate::WordApp;

impl WordApp {
    /// One `command.batch` in this window: all of it as ONE undo step named by the run's label, or none of it.
    pub fn run_command_batch(&mut self, params: &Value) -> Result<Value, String> {
        run_batch(self, |app| &mut app.session, |app, name, args| app.run_host_step(name, args), params)
    }

    fn run_host_step(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        let (dialog, colors) = (self.dialog.is_some(), self.colors.is_some());
        let result = self.run(name, args.clone())?;
        if (self.dialog.is_some() && !dialog) || (self.colors.is_some() && !colors) || result.get("pending").is_some() {
            // A command in the list never asks; if it does, the dialog must not sit over the person's work.
            if !dialog {
                self.dialog = None;
            }
            if !colors {
                self.colors = None;
            }
            return Err(format!("{name} needed a dialog"));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wordcraft_doc::StoryRef;

    use crate::WordApp;

    fn app() -> WordApp {
        WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), crate::Services::default())
    }

    fn text(app: &WordApp) -> String {
        app.session.doc.plain_text(StoryRef::Body)
    }

    #[test]
    fn a_run_goes_through_the_window_and_is_one_undo_step() {
        let mut app = app();
        let before = app.session.undo_depth();
        let r = app
            .run_command_batch(&json!({"label": "A heading", "steps": [
                {"name": "text.insert", "args": {"text": "Plan"}},
                {"name": "para.heading1"},
            ]}))
            .unwrap();
        assert_eq!(r["undoStep"], json!(true));
        assert_eq!(text(&app), "Plan");
        assert_eq!(app.session.undo_depth(), before + 1);
        assert_eq!(app.session.undo_label(), Some("A heading"));
    }

    #[test]
    fn a_step_the_document_forbids_takes_the_whole_run_back() {
        let mut app = app();
        let e = app
            .run_command_batch(&json!({"steps": [
                {"name": "text.insert", "args": {"text": "gone"}},
                {"name": "table.deleteRow"},
            ]}))
            .unwrap_err();
        assert!(e.starts_with("step-failed: step 2"), "{e}");
        assert_eq!(text(&app), "");
    }

    #[test]
    fn a_run_outside_the_list_changes_nothing() {
        let mut app = app();
        let e = app.run_command_batch(&json!({"steps": [{"name": "file.close"}]})).unwrap_err();
        assert!(e.starts_with("command-not-allowed"), "{e}");
        assert!(app.has_document());
    }

    #[test]
    fn a_step_that_would_open_a_dialog_is_refused_and_the_run_taken_back() {
        let mut app = app();
        // Insert Symbol with no character shows its dialog for a person; for a run it is an error, never a dialog.
        let e = app.run_command_batch(&json!({"steps": [{"name": "text.insert", "args": {"text": "gone"}}, {"name": "insert.symbol"}]})).unwrap_err();
        assert!(e.starts_with("step-failed: step 2 (insert.symbol)"), "{e}");
        assert!(app.dialog.is_none(), "a dialog never sits over the person's work");
        assert_eq!(text(&app), "");
    }
}
