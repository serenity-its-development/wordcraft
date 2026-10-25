use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_doc::para::OBJ;
use wordcraft_doc::props::CharProps;
use wordcraft_doc::{Block, Cell, Comment, InlineObject, Paragraph, Part, PartKind, Pos, Row, Run, SectionProps, StoryRef, StyleSheet, Table, para_block};

use crate::blocks::{Shared, body_of, same_content};
use crate::media::{MediaIndex, sha256_hex};
use crate::mirror::Mirror;
use crate::{Change, Extras, Shim};

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
    /// What a window is told when the tree went from `before` to now: the root keys that differ whole, except `blocks`, whose
    /// entries are reported one by one (the host reports the containers an update touched).
    fn changes_since(&self, before: &Value) -> Vec<Change> {
        let now = &self.tree.root;
        let mut out = Vec::new();
        let none = serde_json::Map::new();
        for (k, v) in now.as_object().unwrap_or(&none) {
            if before.get(k) == Some(v) {
                continue;
            }
            if k == "blocks" {
                let old = before.get("blocks").and_then(Value::as_object).unwrap_or(&none);
                for (id, node) in v.as_object().unwrap_or(&none) {
                    if old.get(id) != Some(node) {
                        out.push(Change { path: vec!["blocks".into(), id.clone()], value: Some(node.clone()) });
                    }
                }
                for id in old.keys().filter(|id| v.get(id.as_str()).is_none()) {
                    out.push(Change { path: vec!["blocks".into(), id.clone()], value: None });
                }
            } else {
                out.push(Change { path: vec![k.clone()], value: Some(v.clone()) });
            }
        }
        for k in before.as_object().unwrap_or(&none).keys() {
            if now.get(k).is_none() {
                out.push(Change { path: vec![k.clone()], value: None });
            }
        }
        out
    }
}

fn same_body(a: &[Arc<Block>], b: &[Arc<Block>]) -> bool {
    let idx = MediaIndex::default();
    let cx = Shared::new(&idx);
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_content(x, y, &cx).unwrap())
}

fn shown(shim: &Shim) -> Vec<Arc<Block>> {
    let idx = MediaIndex::default();
    let cx = Shared::new(&idx);
    let (_, blocks) = body_of(&Mirror::new(shim.view().clone()), &["body"], &cx, |_, _| None).unwrap();
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
    let id_two = a.ids()[1].clone();
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

// ── the rest of the document ─────────────────────────────────────────────────────────────────────────────────────────

/// A window's whole document, the way the editor holds it.
struct Doc {
    body: Vec<Arc<Block>>,
    styles: StyleSheet,
    last_section: SectionProps,
    parts: BTreeMap<u32, Part>,
    comments: BTreeMap<u32, Comment>,
    media: BTreeMap<String, Arc<Vec<u8>>>,
}

impl Doc {
    fn new(body: Vec<Arc<Block>>) -> Self {
        Doc { body, styles: StyleSheet::builtin(), last_section: SectionProps::default(), parts: BTreeMap::new(), comments: BTreeMap::new(), media: BTreeMap::new() }
    }
    fn extras(&self) -> Extras<'_> {
        Extras { styles: &self.styles, last_section: &self.last_section, parts: &self.parts, comments: &self.comments, media: &self.media }
    }
}

fn header(text: &str) -> Part {
    Part { kind: PartKind::Header, blocks: vec![para_block(Paragraph::with_text(text, CharProps::default()))], ..Default::default() }
}

fn texts(blocks: &[Arc<Block>]) -> Vec<String> {
    blocks.iter().map(|b| b.as_para().map(|p| p.text.clone()).unwrap_or_else(|| "<table>".into())).collect()
}

#[test]
fn styles_travel_and_a_style_changed_by_one_person_reaches_the_other() {
    let mut a_doc = Doc::new(vec![para("body")]);
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed_all(&a_doc.body, &a_doc.extras(), 0).unwrap());
    // A window joining gets the room's styles whole.
    let mut b = Shim::new(2);
    b.remote_state(&host.tree.root).unwrap();
    let joined = b.take_shown();
    assert!(joined.styles.replace_all);
    assert_eq!(joined.styles.set.len(), StyleSheet::builtin().styles.len(), "every style of the sheet");
    assert!(joined.styles.defaults.is_some());
    // A edits a style and adds one; B is told, by id.
    let before = host.tree.root.clone();
    let heading = a_doc.styles.styles.iter().position(|s| s.id == "Heading1").expect("a built-in Heading1");
    a_doc.styles.styles[heading].chr.bold = Some(false);
    let mut custom = a_doc.styles.styles[heading].clone();
    custom.id = "Callout".into();
    custom.name = "Callout".into();
    custom.builtin = false;
    a_doc.styles.styles.push(custom);
    let batches = a.local_all(&a_doc.body, &a_doc.extras(), 10).unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].label, "Styles and page setup");
    assert_eq!(batches[0].ops.iter().filter(|o| o["op"] == "set").count(), 2, "only the two styles that changed: {:?}", batches[0].ops);
    host.take(&batches);
    b.remote_delta(&host.changes_since(&before), u64::MAX, 20).unwrap();
    let shown = b.take_shown();
    let ids: Vec<&str> = shown.styles.set.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&"Heading1") && ids.contains(&"Callout"));
    assert_eq!(shown.styles.set.iter().find(|s| s.id == "Heading1").unwrap().chr.bold, Some(false));
    // Removing a style removes it for the other person.
    let before = host.tree.root.clone();
    a_doc.styles.styles.retain(|s| s.id != "Callout");
    host.take(&a.local_all(&a_doc.body, &a_doc.extras(), 30).unwrap());
    b.remote_delta(&host.changes_since(&before), u64::MAX, 40).unwrap();
    assert_eq!(b.take_shown().styles.remove, vec!["Callout".to_string()]);
    // Nothing changed, nothing sent.
    assert!(a.local_all(&a_doc.body, &a_doc.extras(), 50).unwrap().is_empty());
}

#[test]
fn a_room_seeded_before_styles_existed_gets_them_from_the_next_edit() {
    let doc = Doc::new(vec![para("old room")]);
    let mut old = Shim::new(1);
    let mut host = Host::new();
    host.take(&old.seed(&doc.body, 0).unwrap());
    assert!(host.tree.root.get("styles").is_none());
    let mut a = Shim::new(2);
    a.remote_state(&host.tree.root).unwrap();
    let batches = a.local_all(&doc.body, &doc.extras(), 5).unwrap();
    assert!(batches[0].ops.iter().any(|o| o["op"] == "ensure" && o["key"] == "styles"), "the roots are made first");
    host.take(&batches);
    assert!(host.tree.root.get("styles").is_some() && host.tree.root.get("lastSection").is_some());
}

#[test]
fn headers_and_footers_are_shared_text_that_merges_like_the_body() {
    let mut a_doc = Doc::new(vec![para("body")]);
    a_doc.parts.insert(3, header("Quarterly report"));
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed_all(&a_doc.body, &a_doc.extras(), 0).unwrap());
    let mut b = Shim::new(2);
    b.remote_state(&host.tree.root).unwrap();
    let joined = b.take_shown();
    assert!(joined.parts_replace_all);
    assert_eq!(joined.parts_set.len(), 1);
    assert_eq!(texts(&joined.parts_set[&3].blocks), vec!["Quarterly report"]);
    assert_eq!(joined.parts_set[&3].kind, PartKind::Header);
    // A types in the header: a text edit on the shared header paragraph, not a new part.
    let before = host.tree.root.clone();
    a_doc.parts.insert(3, header("Quarterly report - draft"));
    let batches = a.local_all(&a_doc.body, &a_doc.extras(), 10).unwrap();
    assert_eq!(batches[0].label, "Header");
    assert!(batches[0].ops.iter().any(|o| o["op"] == "text.insert") && !batches[0].ops.iter().any(|o| o["op"] == "new"));
    host.take(&batches);
    b.remote_delta(&host.changes_since(&before), u64::MAX, 20).unwrap();
    assert_eq!(texts(&b.take_shown().parts_set[&3].blocks), vec!["Quarterly report - draft"]);
    // B adds a footer; A is told.
    let mut b_doc = Doc::new(vec![para("body")]);
    b_doc.parts.insert(3, header("Quarterly report - draft"));
    b_doc.parts.insert(4, Part { kind: PartKind::Footer, blocks: vec![para_block(Paragraph::with_text("Page", CharProps::default()))], ..Default::default() });
    let before = host.tree.root.clone();
    host.take(&b.local_all(&b_doc.body, &b_doc.extras(), 30).unwrap());
    a.remote_delta(&host.changes_since(&before), u64::MAX, 40).unwrap();
    let got = a.take_shown();
    assert_eq!(got.parts_set.keys().copied().collect::<Vec<_>>(), vec![4]);
    assert_eq!(got.parts_set[&4].kind, PartKind::Footer);
    // The header goes.
    let before = host.tree.root.clone();
    b_doc.parts.remove(&3);
    host.take(&b.local_all(&b_doc.body, &b_doc.extras(), 50).unwrap());
    a.remote_delta(&host.changes_since(&before), u64::MAX, 60).unwrap();
    assert_eq!(a.take_shown().parts_remove, vec![3]);
}

fn picture(key: &str) -> Arc<Block> {
    let mut p = Paragraph::with_text("see ", CharProps::default());
    p.text.push(OBJ);
    p.runs = vec![Run { len: p.text.len(), props: CharProps::default() }];
    p.objects = vec![InlineObject::Image { media: key.into(), w: 100.0, h: 50.0, alt: String::new(), float: Default::default(), crop: [0.0; 4], ole: None }];
    Arc::new(Block::Para(p))
}

fn media_key_of(block: &Block) -> String {
    let Block::Para(p) = block else { panic!("a paragraph") };
    match &p.objects[0] {
        InlineObject::Image { media, .. } => media.clone(),
        other => panic!("a picture, got {other:?}"),
    }
}

#[test]
fn a_picture_is_a_content_address_in_the_room_and_its_bytes_are_never_there() {
    let bytes: Vec<u8> = (0..4000u32).map(|i| (i * 7 % 251) as u8).collect();
    let hash = sha256_hex(&bytes);
    let mut a_doc = Doc::new(vec![picture("word/media/image1.png")]);
    a_doc.media.insert("word/media/image1.png".into(), Arc::new(bytes.clone()));
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed_all(&a_doc.body, &a_doc.extras(), 0).unwrap());
    // The room holds the address, not the key and not the bytes.
    let tree = host.tree.root.to_string();
    assert!(tree.contains(&format!("cas-{hash}.png")), "the address is in the room");
    assert!(!tree.contains("image1.png"), "A's own key is not");
    let blocks = host.tree.root["blocks"].to_string();
    assert!(blocks.len() < 1500, "the document's blocks hold no bytes ({} chars for a 4000-byte picture)", blocks.len());
    // B joins with no bytes: it shows the picture by its address and says which bytes it wants.
    let mut b = Shim::new(2);
    let got = b.remote_state(&host.tree.root).unwrap();
    assert_eq!(media_key_of(&got[0]), format!("cas-{hash}.png"));
    assert_eq!(b.take_shown().wanted, vec![hash.clone()]);
    // Bytes that are not the address are not believed; the right ones are stored under the shared key.
    assert_eq!(b.media_key_for(&hash, b"other", "x.png"), None);
    let key = b.media_key_for(&hash, &bytes, "word/media/image1.png").expect("the right bytes");
    assert_eq!(key, format!("cas-{hash}.png"));
    let mut b_media: BTreeMap<String, Arc<Vec<u8>>> = BTreeMap::new();
    b_media.insert(key, Arc::new(bytes.clone()));
    b.media_stored(&b_media);
    assert!(b.take_shown().wanted.is_empty(), "nothing is wanted once the bytes are held");
    // A's later edit sets no object again: the picture is not re-sent.
    let mut p = a_doc.body[0].as_para().unwrap().clone();
    p.text.push_str(" and more");
    p.runs = vec![Run { len: p.text.len(), props: CharProps::default() }];
    a_doc.body = vec![Arc::new(Block::Para(p))];
    let batches = a.local_all(&a_doc.body, &a_doc.extras(), 10).unwrap();
    assert!(!batches.is_empty());
    assert!(batches.iter().all(|b| b.ops.iter().all(|o| o["op"] != "text.mark" || o["key"] != "obj")), "{batches:?}");
}

#[test]
fn two_pictures_with_one_key_stay_two_pictures() {
    // Both people insert a picture and the editor calls both `word/media/image1.png`.
    let (bytes_a, bytes_b) = (b"AAAA picture a".to_vec(), b"BBBB picture b".to_vec());
    let mut a_doc = Doc::new(vec![para("one"), picture("word/media/image1.png")]);
    a_doc.media.insert("word/media/image1.png".into(), Arc::new(bytes_a.clone()));
    let mut b_doc = Doc::new(vec![para("one")]);
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed_all(&a_doc.body, &a_doc.extras(), 0).unwrap());
    let mut b = Shim::new(2);
    b.remote_state(&host.tree.root).unwrap();
    b.take_shown();
    // B inserts its own picture under the same key.
    b_doc.media.insert("word/media/image1.png".into(), Arc::new(bytes_b.clone()));
    b_doc.body = b.body().to_vec();
    b_doc.body.push(picture("word/media/image1.png"));
    host.take(&b.local_all(&b_doc.body, &b_doc.extras(), 5).unwrap());
    let mut c = Shim::new(3);
    let got = c.remote_state(&host.tree.root).unwrap();
    let keys: Vec<String> = got.iter().filter(|b| b.as_para().is_some_and(|p| !p.objects.is_empty())).map(|b| media_key_of(b)).collect();
    assert_eq!(keys.len(), 2, "both pictures are there: {got:?}");
    assert_ne!(keys[0], keys[1], "and they are not confused: {keys:?}");
    let mut wanted = c.take_shown().wanted;
    wanted.sort();
    let mut expect = vec![sha256_hex(&bytes_a), sha256_hex(&bytes_b)];
    expect.sort();
    assert_eq!(wanted, expect);
}

#[test]
fn a_picture_in_a_table_cell_travels_by_address_too() {
    let bytes = b"table picture".to_vec();
    let hash = sha256_hex(&bytes);
    let mut cell = Cell::with_text("x");
    cell.blocks = vec![picture("word/media/t.png")];
    let t = Arc::new(Block::Table(Table { grid: vec![100.0], rows: vec![Row { cells: vec![cell], ..Default::default() }], ..Default::default() }));
    let mut a_doc = Doc::new(vec![t]);
    a_doc.media.insert("word/media/t.png".into(), Arc::new(bytes));
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed_all(&a_doc.body, &a_doc.extras(), 0).unwrap());
    assert!(host.tree.root.to_string().contains(&format!("cas-{hash}.png")));
    let mut b = Shim::new(2);
    b.remote_state(&host.tree.root).unwrap();
    assert_eq!(b.take_shown().wanted, vec![hash]);
}

#[test]
fn cursors_are_block_ids_and_character_offsets_and_resolve_in_the_other_window() {
    let doc = Doc::new(vec![para("first"), para("héllo wörld"), table()]);
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed_all(&doc.body, &doc.extras(), 0).unwrap());
    let mut b = Shim::new(2);
    b.remote_state(&host.tree.root).unwrap();
    // A's caret is after "héllo " (7 bytes, 6 characters); B resolves it to the same place.
    let caret = Pos::body(1, 7);
    let body = a.cursor_for(&caret, &caret).unwrap();
    assert_eq!(body["f"]["o"], 6);
    assert!(body.get("a").is_none(), "a caret has no anchor");
    assert!(body.to_string().len() < 2048);
    let seen = b.peer_cursor(&body).unwrap();
    assert_eq!((seen.focus.clone(), seen.anchor.clone()), (Pos::body(1, 7), Pos::body(1, 7)));
    // A selection runs from one block to another.
    let sel = a.cursor_for(&Pos::body(0, 2), &Pos::body(1, 3)).unwrap();
    let seen = b.peer_cursor(&sel).unwrap();
    assert_eq!((seen.anchor, seen.focus), (Pos::body(0, 2), Pos::body(1, 3)));
    // A place inside a table is the table. A block the other window does not hold is no cursor.
    let in_cell = Pos { story: StoryRef::Body, path: wordcraft_doc::Path(vec![2, 0, 0, 0]), off: 1 };
    assert_eq!(b.peer_cursor(&a.cursor_for(&in_cell, &in_cell).unwrap()).unwrap().focus, Pos::body(2, 0));
    assert!(b.peer_cursor(&json!({"f": {"s": null, "b": "nope", "o": 0}})).is_none());
    assert!(a.cursor_for(&Pos::body(9, 0), &Pos::body(9, 0)).is_none());
}

#[test]
fn a_cursor_in_a_header_resolves_in_the_header() {
    let mut doc = Doc::new(vec![para("body")]);
    doc.parts.insert(3, header("Page header"));
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed_all(&doc.body, &doc.extras(), 0).unwrap());
    let mut b = Shim::new(2);
    b.remote_state(&host.tree.root).unwrap();
    let at = Pos { story: StoryRef::Part(3), path: wordcraft_doc::Path::top(0), off: 4 };
    let body = a.cursor_for(&at, &at).unwrap();
    assert_eq!(body["f"]["s"], 3);
    assert_eq!(b.peer_cursor(&body).unwrap().focus, at);
}

#[test]
fn the_hosts_comment_deltas_change_one_field_or_the_whole_value() {
    let mut s = Shim::new(1);
    let comment = |text: &str, resolved: bool| json!({"by": "p", "name": "Ben Ray", "at": 1_700_000_000_000u64, "text": text, "resolved": resolved, "anchor": {"wc": 3}});
    let at = |path: &[&str], value: Option<Value>| Change { path: path.iter().map(|p| (*p).to_owned()).collect(), value };
    // One comment appears (the entry, whole).
    s.remote_comment_changes(&[at(&["comments", "h1"], Some(comment("first", false)))]);
    let shown = s.take_shown();
    assert_eq!((shown.comments.upsert.len(), shown.comments.upsert[0].text.as_str()), (1, "first"));
    // One field changes.
    s.remote_comment_changes(&[at(&["comments", "h1", "text"], Some(json!("first, edited")))]);
    assert_eq!(s.take_shown().comments.upsert[0].text, "first, edited");
    s.remote_comment_changes(&[at(&["comments", "h1", "resolved"], Some(json!(true)))]);
    assert!(s.take_shown().comments.upsert[0].resolved);
    // Changes that are not comments' change nothing.
    s.remote_comment_changes(&[at(&["doc", "blocks"], None), at(&["suggestions", "s"], Some(json!(1)))]);
    assert!(s.take_shown().comments.is_empty());
    // The whole value replaces them, and a missing entry is a removed comment.
    s.remote_comment_changes(&[at(&["comments"], Some(json!({})))]);
    assert_eq!(s.take_shown().comments.remove, vec![3]);
    s.remote_comment_changes(&[at(&["comments", "h2"], Some(comment("second", false)))]);
    s.take_shown();
    s.remote_comment_changes(&[at(&["comments", "h2"], None)]);
    assert_eq!(s.take_shown().comments.remove, vec![3]);
}

fn commented_paragraph(wc: u32) -> Arc<Block> {
    let mut p = Paragraph::with_text("see ", CharProps::default());
    p.text.push(OBJ);
    p.text.push_str("this word");
    p.text.push(OBJ);
    p.text.push_str(" now");
    p.objects = vec![InlineObject::CommentStart { id: wc }, InlineObject::CommentEnd { id: wc }];
    p.runs = vec![Run { len: p.text.len(), props: CharProps::default() }];
    Arc::new(Block::Para(p))
}

fn comment_part(text: &str) -> Part {
    Part { kind: PartKind::Comment, blocks: vec![para_block(Paragraph::with_text(text, CharProps::default()))], ..Default::default() }
}

#[test]
fn a_comment_is_anchored_in_the_shared_text_and_the_hosts_comments_come_back() {
    let mut a_doc = Doc::new(vec![commented_paragraph(41)]);
    a_doc.parts.insert(9, comment_part("is this right?"));
    a_doc.comments.insert(41, Comment { author: "Ann".into(), part: 9, ..Default::default() });
    let mut a = Shim::new(1);
    let mut host = Host::new();
    host.take(&a.seed_all(&a_doc.body, &a_doc.extras(), 0).unwrap());
    let msgs = a.take_comment_msgs();
    assert_eq!(msgs.len(), 1);
    let body = msgs[0].body();
    assert_eq!(body["action"], "add");
    assert_eq!(body["text"], "is this right?");
    assert_eq!(body["anchor"]["wc"], 41);
    // The anchor names the shared paragraph and the characters from the start object to the end object.
    let block_id = a.ids()[0].clone();
    assert_eq!(body["anchor"]["path"], json!(["blocks", block_id, "text"]));
    // From the start object (character 4) through the end object (character 14): the range is [4, 15).
    assert_eq!((body["anchor"]["start"].as_u64(), body["anchor"]["end"].as_u64()), (Some(4), Some(15)));
    // Comment parts are the host's, not the room's: no header or footer part was made for it.
    assert!(host.tree.root["parts"].as_object().unwrap().is_empty());
    // B joins; the host (here: the test) holds the comment as the comments tier does, and B's window shows it.
    let mut b = Shim::new(2);
    b.remote_state(&host.tree.root).unwrap();
    let host_comments = json!({"h1": {"by": "p1", "name": "Ann Lee", "at": 1_700_000_000_000u64, "text": "is this right?", "resolved": false, "anchor": body["anchor"]}});
    b.remote_comments(&host_comments);
    let shown = b.take_shown();
    assert_eq!(shown.comments.upsert.len(), 1);
    assert_eq!((shown.comments.upsert[0].wc, shown.comments.upsert[0].author.as_str(), shown.comments.upsert[0].initials.as_str()), (41, "Ann Lee", "AL"));
    // B replies and resolves; the host's id is the parent.
    let mut b_doc = Doc::new(vec![commented_paragraph(41)]);
    b_doc.parts.insert(9, comment_part("is this right?"));
    b_doc.comments.insert(41, Comment { author: "Ann Lee".into(), part: 9, ..Default::default() });
    b.local_all(&b_doc.body, &b_doc.extras(), 5).unwrap();
    b_doc.parts.insert(10, comment_part("yes"));
    b_doc.comments.insert(77, Comment { author: "Ben".into(), part: 10, parent: Some(41), ..Default::default() });
    b_doc.comments.get_mut(&41).unwrap().resolved = true;
    b.local_all(&b_doc.body, &b_doc.extras(), 6).unwrap();
    let sent: Vec<Value> = b.take_comment_msgs().iter().map(crate::comments::CommentMsg::body).collect();
    assert!(sent.contains(&json!({"action": "reply", "id": "h1", "text": "yes", "anchor": {"wc": 77}})), "{sent:?}");
    assert!(sent.contains(&json!({"action": "resolve", "id": "h1"})), "{sent:?}");
}
