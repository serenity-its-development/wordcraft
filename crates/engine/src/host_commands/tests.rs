use serde_json::{Value, json};
use wordcraft_doc::StoryRef;

use super::*;
use crate::Session;

fn session() -> Session {
    Session::new(wordcraft_doc::Document::new())
}

fn run(s: &mut Session, label: &str, steps: Value) -> Result<Value, String> {
    s.run_command_batch(&json!({"label": label, "steps": steps}))
}

fn text(s: &Session) -> String {
    s.doc.plain_text(StoryRef::Body)
}

/// Every command id the registry has.
fn universe() -> Vec<String> {
    session().registry.all().iter().map(|c| c.id.to_string()).collect()
}

/// Names that say "print" but are a view of the page.
const NAME_ONLY: &[&str] = &["view.printLayout"];

#[test]
fn the_allow_list_and_the_commands_left_out_are_the_whole_registry_with_nothing_twice() {
    let all = universe();
    let mut seen = std::collections::HashSet::new();
    for id in ASSISTANT_COMMANDS {
        assert!(all.iter().any(|c| c == id), "{id} is not a command of the app");
        assert!(seen.insert(id.to_string()), "{id} is listed twice");
    }
    for (reason, ids) in LEFT_OUT {
        assert!(reason.len() > 20, "a reason: {reason}");
        for id in *ids {
            assert!(all.iter().any(|c| c == id), "{id} is left out but is not a command of the app");
            assert!(seen.insert(id.to_string()), "{id} is both allowed and left out, or left out twice");
        }
    }
    let missing: Vec<&String> = all.iter().filter(|c| !seen.contains(c.as_str())).collect();
    assert!(missing.is_empty(), "commands neither allowed nor left out (with a reason): {missing:?}");
    assert_eq!(allowed().len(), ASSISTANT_COMMANDS.len() + ARGUMENT_COMMANDS.len());
}

#[test]
fn no_allowed_command_names_a_file_or_is_undo_or_redo() {
    let words = |id: &str| -> Vec<String> {
        let mut out = Vec::new();
        for seg in id.split('.') {
            let mut cur = String::new();
            for ch in seg.chars() {
                if ch.is_uppercase() && !cur.is_empty() {
                    out.push(cur.to_lowercase());
                    cur.clear();
                }
                cur.push(ch);
            }
            if !cur.is_empty() {
                out.push(cur.to_lowercase());
            }
        }
        out
    };
    const FILE_WORDS: &[&str] =
        &["file", "files", "save", "export", "import", "download", "upload", "plugin", "script", "print", "package", "library", "revert", "recovery"];
    for id in ASSISTANT_COMMANDS {
        if NAME_ONLY.contains(id) {
            continue;
        }
        let bad: Vec<String> = words(id).into_iter().filter(|w| FILE_WORDS.contains(&w.as_str())).collect();
        assert!(bad.is_empty(), "{id} names {bad:?}");
        assert!(!id.starts_with("file.") && !id.starts_with("clipboard."), "{id}");
        assert!(!id.ends_with(".undo") && !id.ends_with(".redo"), "{id} is Undo or Redo");
    }
}

#[test]
fn a_run_is_one_undo_step_named_for_the_run() {
    let mut s = session();
    let before = s.undo_depth();
    let r = run(
        &mut s,
        "Two lines",
        json!([
            {"name": "text.insert", "args": {"text": "Hello"}},
            {"name": "text.newParagraph"},
            {"name": "text.insert", "args": {"text": "World"}},
        ]),
    )
    .unwrap();
    assert_eq!(r["undoStep"], json!(true));
    assert_eq!(r["steps"].as_array().unwrap().len(), 3);
    assert_eq!(text(&s), "Hello\nWorld");
    assert_eq!(s.undo_depth(), before + 1);
    assert_eq!(s.undo_label(), Some("Two lines"));
    assert!(s.undo());
    assert_eq!(text(&s), "", "one Undo takes the whole run back");
}

#[test]
fn a_run_after_typing_is_a_step_of_its_own() {
    let mut s = session();
    s.run("text.insert", &json!({"text": "typed"})).unwrap();
    run(&mut s, "More", json!([{"name": "text.insert", "args": {"text": " more"}}])).unwrap();
    assert_eq!(text(&s), "typed more");
    assert!(s.undo());
    assert_eq!(text(&s), "typed", "the person's typing is its own step");
}

#[test]
fn a_step_the_document_forbids_takes_the_whole_run_back() {
    let mut s = session();
    s.run("text.insert", &json!({"text": "kept"})).unwrap();
    let depth = s.undo_depth();
    let e = run(&mut s, "Bad", json!([{"name": "text.insert", "args": {"text": " lost"}}, {"name": "table.deleteRow"}])).unwrap_err();
    assert!(e.starts_with("step-failed: step 2 (table.deleteRow)"), "{e}");
    assert_eq!(text(&s), "kept");
    assert_eq!(s.undo_depth(), depth, "no step left behind");
}

#[test]
fn a_run_outside_the_list_changes_nothing() {
    let mut s = session();
    for name in ["file.save", "edit.undo", "edit.paste", "ui.tab", "nonsense"] {
        let e = run(&mut s, "x", json!([{"name": "text.insert", "args": {"text": "a"}}, {"name": name}])).unwrap_err();
        assert!(e.starts_with("command-not-allowed"), "{name}: {e}");
    }
    assert_eq!(text(&s), "");
}

#[test]
fn a_run_does_not_reach_files_or_addresses() {
    let mut s = session();
    for args in [
        json!({"path": "/etc/passwd"}),
        json!({"text": "a", "nested": {"url": "https://example.com"}}),
        json!({"data": "AAAA"}),
        json!({"__dialog": true}),
    ] {
        let e = run(&mut s, "x", json!([{"name": "text.insert", "args": args}])).unwrap_err();
        assert!(e.starts_with("argument-not-allowed"), "{e}");
    }
    // A chart's data is numbers, not a file's bytes.
    assert!(forbidden_arg(&json!({"data": {"categories": ["a"], "series": []}})).is_none());
    assert_eq!(text(&s), "");
}

#[test]
fn a_run_is_judged_whole_before_it_runs() {
    let mut s = session();
    assert!(run(&mut s, "x", json!([])).unwrap_err().starts_with("run-empty"));
    let many: Vec<Value> = (0..=MAX_STEPS).map(|_| json!({"name": "text.insert", "args": {"text": "a"}})).collect();
    assert!(run(&mut s, "x", Value::Array(many)).unwrap_err().starts_with("run-too-long"));
    assert!(run(&mut s, &"l".repeat(MAX_LABEL + 1), json!([{"name": "text.insert"}])).unwrap_err().starts_with("label-too-long"));
    assert!(s.run_command_batch(&json!({"steps": [{"name": "text.insert", "args": "oops"}]})).unwrap_err().starts_with("argument-not-allowed"));
}

#[test]
fn a_later_step_names_what_an_earlier_step_made() {
    let results = vec![json!({"id": "a1", "n": 7})];
    assert_eq!(resolve_step_refs(&json!({"x": {"$step": 0}}), &results, 1, 0).unwrap(), json!({"x": "a1"}));
    assert_eq!(resolve_step_refs(&json!([{"$step": 0, "field": "n"}]), &results, 1, 0).unwrap(), json!([7]));
    assert!(resolve_step_refs(&json!({"$step": 1}), &results, 1, 0).is_err(), "not an earlier step");
    assert!(resolve_step_refs(&json!({"$step": 0, "field": "nope"}), &results, 1, 0).is_err());
    assert!(resolve_step_refs(&json!({"$step": 0, "extra": 1}), &results, 1, 0).is_err());
}

#[test]
fn a_run_that_changes_nothing_adds_no_undo_step() {
    let mut s = session();
    let r = run(&mut s, "Look", json!([{"name": "document.inspect"}])).unwrap();
    assert_eq!(r["undoStep"], json!(false));
    assert_eq!(s.undo_depth(), 0);
}
