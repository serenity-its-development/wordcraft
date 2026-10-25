//! The collaboration shim for node: one [`Window`] is one editor window's side of the host bridge, with the document held
//! as JSON so a test (OneOS's `craft-write-convergence.test.ts`) can drive two windows through the host's real adapter and CRDT
//! engine and compare what they end up showing. Everything the browser build shares is here: the body, styles, headers and
//! footers, comments, pictures (by content address) and cursors.

use std::sync::Arc;

use serde_json::{Value, json};
use wasm_bindgen::prelude::*;
use wordcraft_collab::comments::CommentMsg;
use wordcraft_collab::{Batch, Change, Extras, Shim, Shown, changes_of};
use wordcraft_doc::{Block, Comment, Document, Paragraph, Part, PartKind, Pos, para_block};

fn blocks_of(body_json: &str) -> Result<Vec<Arc<Block>>, JsError> {
    let blocks: Vec<Block> = serde_json::from_str(body_json).map_err(|e| JsError::new(&format!("body: {e}")))?;
    Ok(blocks.into_iter().map(Arc::new).collect())
}

fn json_of(blocks: &[Arc<Block>]) -> String {
    serde_json::to_string(&blocks.iter().map(|b| &**b).collect::<Vec<_>>()).unwrap_or_default()
}

fn batches_json(batches: &[Batch]) -> String {
    Value::Array(batches.iter().map(|b| json!({"seq": b.seq, "ops": b.ops, "label": b.label, "undo": if b.join { "join" } else { "step" }, "seed": b.seed})).collect())
        .to_string()
}

fn err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn text_part(kind: PartKind, text: &str) -> Part {
    Part { kind, blocks: vec![para_block(Paragraph::with_text(text, Default::default()))], ..Default::default() }
}

#[wasm_bindgen]
pub struct Window {
    shim: Shim,
    /// What the editor holds.
    doc: Document,
}

impl Window {
    /// What the editor does with the remote changes beside the body (the browser build does the same in `collab.rs`).
    fn apply_shown(&mut self, shown: &Shown) {
        shown.apply_to(&mut self.doc);
    }
}

#[wasm_bindgen]
impl Window {
    /// A window showing `body_json` (an array of blocks), its ids drawn from `seed`.
    #[wasm_bindgen(constructor)]
    pub fn new(seed: u32, body_json: &str) -> Result<Window, JsError> {
        let mut doc = Document::new();
        doc.body = blocks_of(body_json)?;
        Ok(Window { shim: Shim::new(u64::from(seed) * 0x9e37_79b9 + 1), doc })
    }

    /// The seed batches for the opened document (an array of `{seq, ops, label, undo, seed}`).
    pub fn seed(&mut self, now: f64) -> Result<String, JsError> {
        let Window { shim, doc } = self;
        let batches = shim.seed_all(&doc.body, &Extras::of(doc), now as u64).map_err(err)?;
        Ok(batches_json(&batches))
    }

    /// The person edited the body: the editor now shows `body_json`; answers the batches to send.
    pub fn edit(&mut self, body_json: &str, now: f64) -> Result<String, JsError> {
        self.doc.body = blocks_of(body_json)?;
        self.flush(now)
    }

    /// The person edited something beside the body (a style, a header, a comment, a picture): answers the batches to send.
    pub fn flush(&mut self, now: f64) -> Result<String, JsError> {
        let Window { shim, doc } = self;
        let batches = shim.local_all(&doc.body, &Extras::of(doc), now as u64).map_err(err)?;
        Ok(batches_json(&batches))
    }

    /// The host sent `craft:crdt:state`: `doc_json` is its `doc`, `comments_json` its `comments`.
    pub fn state(&mut self, doc_json: &str, comments_json: &str) -> Result<(), JsError> {
        let doc: Value = serde_json::from_str(doc_json).map_err(|e| JsError::new(&format!("state: {e}")))?;
        self.doc.body = self.shim.remote_state(&doc).map_err(err)?;
        let comments: Value = serde_json::from_str(comments_json).unwrap_or(Value::Null);
        self.shim.remote_comments(&comments);
        let shown = self.shim.take_shown();
        self.apply_shown(&shown);
        Ok(())
    }

    /// The host sent `craft:crdt:delta`: `changes_json` is its `changes` (paths below `doc` carry the leading `doc`; a change at
    /// `["comments"]` is the whole comments value); answers whether the shown body changed.
    pub fn delta(&mut self, changes_json: &str, ack: f64, now: f64) -> Result<bool, JsError> {
        let all: Value = serde_json::from_str(changes_json).map_err(|e| JsError::new(&format!("changes: {e}")))?;
        let changes = changes_of(&all);
        self.shim.remote_comment_changes(&changes);
        let doc_changes: Vec<Change> = changes
            .into_iter()
            .filter_map(|mut c| {
                (c.path.first().map(String::as_str) == Some("doc")).then(|| {
                    c.path.remove(0);
                    c
                })
            })
            .collect();
        let changed = match self.shim.remote_delta(&doc_changes, ack as u64, now as u64).map_err(err)? {
            Some(body) => {
                self.doc.body = body;
                true
            }
            None => false,
        };
        let shown = self.shim.take_shown();
        self.apply_shown(&shown);
        Ok(changed)
    }

    /// The comment messages the last local calls produced (an array of `craft:crdt:comment` bodies).
    pub fn comment_msgs(&mut self) -> String {
        Value::Array(self.shim.take_comment_msgs().iter().map(CommentMsg::body).collect()).to_string()
    }

    // ── the document beyond the body, as the tests edit it ────────────────────────────────────────────────────────

    /// Set (replace) a header or footer part with one paragraph of `text`.
    pub fn set_part(&mut self, pid: u32, kind: &str, text: &str) {
        self.doc.parts.insert(pid, text_part(if kind == "footer" { PartKind::Footer } else { PartKind::Header }, text));
    }

    pub fn remove_part(&mut self, pid: u32) {
        self.doc.parts.remove(&pid);
    }

    /// Each header and footer part: `{pid: {kind, text}}`.
    pub fn parts_json(&self) -> String {
        let mut o = serde_json::Map::new();
        for (pid, p) in &self.doc.parts {
            if !matches!(p.kind, PartKind::Header | PartKind::Footer) {
                continue;
            }
            let text: Vec<String> = p.blocks.iter().filter_map(|b| b.as_para().map(|x| x.text.clone())).collect();
            o.insert(pid.to_string(), json!({"kind": if p.kind == PartKind::Header { "header" } else { "footer" }, "text": text}));
        }
        Value::Object(o).to_string()
    }

    /// Set a style's bold flag, or (`copy_of` not empty) add a style `id` copied from that one with the flag.
    pub fn set_style_bold(&mut self, id: &str, copy_of: &str, bold: bool) -> Result<(), JsError> {
        if !copy_of.is_empty() {
            let mut s = self.doc.styles.styles.iter().find(|s| s.id == copy_of).cloned().ok_or_else(|| JsError::new("no such style"))?;
            s.id = id.to_owned();
            s.name = id.to_owned();
            s.builtin = false;
            s.chr.bold = Some(bold);
            self.doc.styles.styles.retain(|x| x.id != id);
            self.doc.styles.styles.push(s);
            return Ok(());
        }
        let s = self.doc.styles.styles.iter_mut().find(|s| s.id == id).ok_or_else(|| JsError::new("no such style"))?;
        s.chr.bold = Some(bold);
        Ok(())
    }

    pub fn remove_style(&mut self, id: &str) {
        self.doc.styles.styles.retain(|s| s.id != id);
    }

    /// Each style id with its bold flag: `{id: bold|null}`, plus `"$count"`.
    pub fn styles_json(&self) -> String {
        let mut o = serde_json::Map::new();
        for s in &self.doc.styles.styles {
            o.insert(s.id.clone(), json!(s.chr.bold));
        }
        o.insert("$count".into(), json!(self.doc.styles.styles.len()));
        Value::Object(o).to_string()
    }

    /// Store a picture under `key` (the editor's own key).
    pub fn add_media(&mut self, key: &str, bytes: &[u8]) {
        self.doc.media.insert(key.to_owned(), Arc::new(bytes.to_vec()));
    }

    /// The content addresses this window wants bytes for (an array of hashes).
    pub fn wanted(&self) -> String {
        serde_json::to_string(&self.shim.media().wanted()).unwrap_or_default()
    }

    /// The editor found picture bytes in the file's saved version: store them if they are the picture `hash` names. Answers the
    /// key they were stored under, or an empty string.
    pub fn store_media(&mut self, hash: &str, bytes: &[u8]) -> String {
        match self.shim.media_key_for(hash, bytes, "x.png") {
            Some(key) => {
                self.doc.media.insert(key.clone(), Arc::new(bytes.to_vec()));
                self.shim.media_stored(&self.doc.media);
                if let Ok(blocks) = self.shim.reshow() {
                    self.doc.body = blocks;
                }
                key
            }
            None => String::new(),
        }
    }

    /// The picture keys the document holds, with their sizes: `{key: size}`.
    pub fn media_json(&self) -> String {
        Value::Object(self.doc.media.iter().map(|(k, v)| (k.clone(), json!(v.len()))).collect()).to_string()
    }

    /// Add a comment with the text, optionally a reply to `parent` (0: none).
    pub fn add_comment(&mut self, wc: u32, text: &str, parent: u32) {
        let pid = self.doc.parts.keys().next_back().map_or(1, |k| k + 1);
        self.doc.parts.insert(pid, text_part(PartKind::Comment, text));
        self.doc.comments.insert(wc, Comment { author: "Test".into(), part: pid, parent: (parent != 0).then_some(parent), ..Default::default() });
    }

    pub fn resolve_comment(&mut self, wc: u32, resolved: bool) {
        if let Some(c) = self.doc.comments.get_mut(&wc) {
            c.resolved = resolved;
        }
    }

    pub fn edit_comment(&mut self, wc: u32, text: &str) {
        if let Some(part) = self.doc.comments.get(&wc).map(|c| c.part) {
            self.doc.parts.insert(part, text_part(PartKind::Comment, text));
        }
    }

    pub fn delete_comment(&mut self, wc: u32) {
        if let Some(c) = self.doc.comments.remove(&wc) {
            self.doc.parts.remove(&c.part);
        }
    }

    /// Each comment: `{wc: {text, parent, resolved}}` (the author is the host's stamp and differs by window).
    pub fn comments_json(&self) -> String {
        let mut o = serde_json::Map::new();
        for (wc, c) in &self.doc.comments {
            let text = self.doc.parts.get(&c.part).map(wordcraft_collab::comments::text_of_part).unwrap_or_default();
            o.insert(wc.to_string(), json!({"text": text, "parent": c.parent, "resolved": c.resolved}));
        }
        Value::Object(o).to_string()
    }

    // ── cursors ───────────────────────────────────────────────────────────────────────────────────────────────────

    /// The `craft:crdt:cursor` body for a selection in the body, from `anchor_block`/`anchor_off` to `block`/`off` (bytes).
    pub fn cursor(&self, anchor_block: u32, anchor_off: u32, block: u32, off: u32) -> String {
        let a = Pos::body(anchor_block as usize, anchor_off as usize);
        let f = Pos::body(block as usize, off as usize);
        self.shim.cursor_for(&a, &f).map(|v| v.to_string()).unwrap_or_default()
    }

    /// Where another person is, in this window: `{anchor: [block, off], focus: [block, off]}`, or an empty string.
    pub fn peer(&self, cursor_json: &str) -> String {
        let Ok(v) = serde_json::from_str::<Value>(cursor_json) else { return String::new() };
        match self.shim.peer_cursor(&v) {
            Some(p) => json!({
                "anchor": [p.anchor.path.0.first(), p.anchor.off],
                "focus": [p.focus.path.0.first(), p.focus.off],
            })
            .to_string(),
            None => String::new(),
        }
    }

    // ── what the editor shows ─────────────────────────────────────────────────────────────────────────────────────

    /// What the editor shows, as JSON blocks.
    pub fn body(&self) -> String {
        json_of(&self.doc.body)
    }

    /// Each block's text and its per-character formatting, as one comparable JSON string.
    pub fn digest(&self) -> String {
        let cx = wordcraft_collab::blocks::Shared::new(self.shim.media());
        let blocks: Vec<Value> = self
            .doc
            .body
            .iter()
            .map(|b| match &**b {
                Block::Para(p) => {
                    let marks = wordcraft_collab::blocks::char_marks(p, &cx).unwrap_or_default();
                    json!({"p": p.text, "marks": marks, "props": p.props, "mark": p.mark})
                }
                Block::Table(t) => json!({"t": t}),
            })
            .collect();
        Value::Array(blocks).to_string()
    }
}

/// A two-cell table as a block, in the editor's own JSON.
#[wasm_bindgen]
pub fn sample_table() -> String {
    let row = wordcraft_doc::Row { cells: vec![wordcraft_doc::Cell::with_text("a"), wordcraft_doc::Cell::with_text("b")], ..Default::default() };
    let table = wordcraft_doc::Table { grid: vec![100.0, 100.0], rows: vec![row], ..Default::default() };
    serde_json::to_string(&Block::Table(table)).unwrap_or_default()
}

/// A paragraph with a picture (`media_key` is the editor's own key for its bytes), in the editor's own JSON.
#[wasm_bindgen]
pub fn sample_picture(media_key: &str) -> String {
    let mut p = Paragraph::with_text("see ", Default::default());
    p.text.push(wordcraft_doc::para::OBJ);
    p.runs = vec![wordcraft_doc::Run { len: p.text.len(), props: Default::default() }];
    p.objects = vec![wordcraft_doc::InlineObject::Image { media: media_key.to_owned(), w: 100.0, h: 50.0, alt: String::new(), float: Default::default(), crop: [0.0; 4], ole: None }];
    serde_json::to_string(&Block::Para(p)).unwrap_or_default()
}

/// A paragraph carrying comment `wc` over its middle words, in the editor's own JSON.
#[wasm_bindgen]
pub fn sample_commented(wc: u32) -> String {
    let mut p = Paragraph::with_text("see ", Default::default());
    p.text.push(wordcraft_doc::para::OBJ);
    p.text.push_str("this word");
    p.text.push(wordcraft_doc::para::OBJ);
    p.text.push_str(" now");
    p.objects = vec![wordcraft_doc::InlineObject::CommentStart { id: wc }, wordcraft_doc::InlineObject::CommentEnd { id: wc }];
    p.runs = vec![wordcraft_doc::Run { len: p.text.len(), props: Default::default() }];
    serde_json::to_string(&Block::Para(p)).unwrap_or_default()
}
