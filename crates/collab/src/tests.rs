use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_doc::para::OBJ;
use wordcraft_doc::props::CharProps;
use wordcraft_doc::{Block, Cell, Paragraph, Row, Run, Table};

use crate::blocks::{body_of, same_content};
use crate::mirror::Mirror;
use crate::{Change, Shim};

fn para(text: &str) -> Arc<Block> {
    Arc::new(Block::Para(Paragraph::with_text(text, CharProps::default())))
}

fn bold() -> CharProps {
    CharProps { bold: Some(true), ..Default::default() }
}

fn with_runs(parts: &[(&str, CharProps)]) -> Arc<Block> {
    let mut p = Paragraph::new();
    for (t, props) in parts {
        p.text.push_str(t);
        p.runs.push(Run { len: t.len(), props: props.clone() });
    }
    Arc::new(Block::Para(p))
}

fn table() -> Arc<Block> {
    Arc::new(Block::Table(Table { grid: vec![100.0, 100.0], rows: vec![Row { cells: vec![Cell::with_text("a"), Cell::with_text("b")], ..Default::default() }], ..Default::default() }))
}

/// A sequential host: one shared tree that ops are applied to.
struct Host {
    tree: Mirror,
}

impl Host {
    fn new() -> Self {
        Host { tree: Mirror::default() }
    }
    fn take(&mut self, batches: &[crate::Batch]) {
        for b in batches {
            for op in &b.ops {
                self.tree.apply(op).unwrap_or_else(|e| panic!("the host refused {op}: {e}"));
            }
        }
    }
}

fn same_body(a: &[Arc<Block>], b: &[Arc<Block>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_content(x, y).unwrap())
}

fn shown(shim: &Shim) -> Vec<Arc<Block>> {
    let (_, blocks) = body_of(&Mirror::new(shim.view().clone()), |_, _| None).unwrap();
    blocks
}

#[test]
fn a_seeded_document_reads_back_as_itself() {
    let doc = vec![para("Hello"), with_runs(&[("plain ", CharProps::default()), ("bold", bold()), (" tail", CharProps::default())]), table(), para("")];
    let mut a = Shim::new(7);
    let mut host = Host::new();
    host.take(&a.seed(&doc, 0).unwrap());
    let mut b = Shim::new(9);
    let got = b.remote_state(&host.tree.root).unwrap();
    assert!(same_body(&doc, &got), "the second window shows the seeded body");
    assert!(same_body(&doc, &shown(&a)));
}

#[test]
fn typing_formatting_splitting_and_deleting_reach_the_shared_tree() {
    let mut doc = vec![para("Hello world"), para("second")];
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed(&doc, 0).unwrap());
    // Typing in the middle of a paragraph is a text edit, not a replacement.
    doc[0] = para("Hello brave world");
    let batches = a.local(&doc, 10).unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].label, "Typing");
    assert!(batches[0].ops.iter().any(|o| o["op"] == "text.insert") && !batches[0].ops.iter().any(|o| o["op"] == "new"));
    host.take(&batches);
    // Bolding a word is a mark.
    doc[0] = with_runs(&[("Hello ", CharProps::default()), ("brave", bold()), (" world", CharProps::default())]);
    let batches = a.local(&doc, 20).unwrap();
    assert_eq!(batches[0].label, "Formatting");
    assert!(batches[0].ops.iter().all(|o| o["op"] == "text.mark"), "{:?}", batches[0].ops);
    host.take(&batches);
    // Enter in the middle: one paragraph becomes two.
    doc[0] = para("Hello ");
    doc.insert(1, with_runs(&[("brave", bold()), (" world", CharProps::default())]));
    host.take(&a.local(&doc, 30).unwrap());
    // A table appears, a paragraph goes.
    doc.push(table());
    doc.remove(2);
    host.take(&a.local(&doc, 40).unwrap());
    let mut b = Shim::new(2);
    let got = b.remote_state(&host.tree.root).unwrap();
    assert!(same_body(&doc, &got), "the room's tree reads back as the editor's body");
    assert!(a.local(&doc, 50).unwrap().is_empty(), "nothing changed, nothing sent");
}

#[test]
fn an_inline_object_travels_with_its_character() {
    let mut p = Paragraph::with_text("a", CharProps::default());
    p.text.push(OBJ);
    p.text.push('b');
    p.runs = vec![Run { len: p.text.len(), props: CharProps::default() }];
    p.objects = vec![wordcraft_doc::InlineObject::Opaque { format: "docx".into(), xml: "<w:r/>".into(), text: "x".into() }];
    let doc = vec![Arc::new(Block::Para(p))];
    let mut a = Shim::new(3);
    let mut host = Host::new();
    host.take(&a.seed(&doc, 0).unwrap());
    let mut b = Shim::new(4);
    let got = b.remote_state(&host.tree.root).unwrap();
    let Block::Para(q) = &*got[0] else { panic!("a paragraph") };
    assert_eq!(q.text, doc[0].as_para().unwrap().text);
    assert_eq!(q.objects, doc[0].as_para().unwrap().objects);
}

#[test]
fn a_remote_delta_updates_the_view_and_keeps_edits_in_flight() {
    let mut doc = vec![para("one"), para("two")];
    let mut a = Shim::new(1);
    let mut host = Host::new();
    let seeded = a.seed(&doc, 0).unwrap();
    host.take(&seeded);
    // The host has applied the seed: a message acknowledges it.
    assert!(a.remote_delta(&[], seeded.last().unwrap().seq, 1).unwrap().is_none());
    // A types "!" at the end of "one"; the host has NOT yet applied that batch when a remote change to "two" is reported.
    doc[0] = para("one!");
    let batches = a.local(&doc, 10).unwrap();
    let id_two = a.ids[1].clone();
    let spans = json!({"$text": [{"text": "two and more", "marks": {}}]});
    let change = Change { path: vec!["blocks".into(), id_two, "text".into()], value: Some(spans) };
    let shown_body = a.remote_delta(&[change], 0, 20).unwrap().expect("the body changed");
    let texts: Vec<String> = shown_body.iter().map(|b| b.as_para().unwrap().text.clone()).collect();
    assert_eq!(texts, vec!["one!", "two and more"], "the remote change shows and the edit in flight stays");
    host.take(&batches);
}

#[test]
fn a_big_paste_goes_as_several_batches_in_order() {
    let mut doc = vec![para("start")];
    let mut a = Shim::new(5);
    let mut host = Host::new();
    host.take(&a.seed(&doc, 0).unwrap());
    for i in 0..900 {
        doc.push(with_runs(&[(&format!("line {i} "), CharProps::default()), ("end", bold())]));
    }
    let batches = a.local(&doc, 5).unwrap();
    assert!(batches.len() > 1 && batches.iter().all(|b| b.ops.len() <= crate::BATCH_OPS));
    host.take(&batches);
    let mut b = Shim::new(6);
    assert!(same_body(&doc, &b.remote_state(&host.tree.root).unwrap()));
}

#[test]
fn a_table_too_big_for_one_value_is_refused_by_name() {
    let rows: Vec<Row> = (0..400).map(|i| Row { cells: vec![Cell::with_text(&format!("{i}: {}", "x".repeat(200)))], ..Default::default() }).collect();
    let big = Arc::new(Block::Table(Table { grid: vec![100.0], rows, ..Default::default() }));
    let err = Shim::new(1).seed(&[big], 0).unwrap_err();
    assert!(matches!(err, crate::ShimError::TooLarge(_)));
}

#[test]
fn mirror_changes_set_and_remove() {
    let mut m = Mirror::default();
    m.apply_change(&["blocks".into(), "x".into(), "text".into()], Some(&json!({"$text": []}))).unwrap();
    assert!(m.get(&["blocks", "x", "text"]).is_some());
    m.apply_change(&["blocks".into(), "x".into()], None).unwrap();
    assert!(m.get(&["blocks", "x"]).is_none());
    let v: Value = json!([{"path": ["a"], "value": 1}, {"path": ["b"], "removed": true}]);
    assert_eq!(crate::changes_of(&v).len(), 2);
}
