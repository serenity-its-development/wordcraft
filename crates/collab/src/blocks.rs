//! Body blocks <-> the shared tree.
//!
//! The schema (beneath the doc's `craft` root; paths below are from that root):
//!
//! ```text
//! body                 list of block ids (the document order)
//! blocks.<id>          one block, a map:
//!   text               a paragraph's text, as CRDT text
//!   props, mark, section   a paragraph's ParaProps / mark CharProps / section break, as JSON values (absent = default)
//!   v                  a TABLE, whole, as one JSON value (last writer wins); a block with `v` is a table
//! ```
//!
//! A paragraph's character formatting is MARKS on its text: one mark per CharProps field, `c:<field>` (the field's JSON
//! value), `expand: false`, so two people formatting one word differently merge field by field. An inline object
//! (picture, field, note reference...) is the U+FFFC character carrying an `obj` mark whose value is the object's JSON,
//! so it travels with its character as others type around it.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use wordcraft_doc::para::{OBJ, next_rev};
use wordcraft_doc::props::{CharProps, ParaProps};
use wordcraft_doc::{Block, InlineObject, Paragraph, Run, SectionProps, Table};

use crate::mirror::{Chars, Marks, Mirror, spans_to_chars};
use crate::ShimError;

/// The mark key of a CharProps field.
const FIELD: &str = "c:";
/// The mark key of an inline object.
const OBJECT: &str = "obj";
/// The largest text one `new` / `text.insert` carries (the host's limit is 65 536 characters).
const CHUNK: usize = 60_000;
/// The largest JSON value one `set` carries (the host's limit is 64 KiB).
const MAX_VALUE: usize = 60_000;

fn json_of<T: serde::Serialize>(v: &T) -> Result<Value, ShimError> {
    serde_json::to_value(v).map_err(|e| ShimError::Model(e.to_string()))
}

fn too_large(what: &str, bytes: usize) -> ShimError {
    ShimError::TooLarge(format!("{what} is {bytes} bytes of data - more than one shared value holds ({MAX_VALUE})"))
}

/// A value that must fit one `set`.
fn bounded(what: &str, v: Value) -> Result<Value, ShimError> {
    let n = v.to_string().len();
    if n > MAX_VALUE { Err(too_large(what, n)) } else { Ok(v) }
}

/// Per-character marks of a paragraph: its runs' CharProps fields, and the objects at their U+FFFC.
pub fn char_marks(p: &Paragraph) -> Result<Vec<Marks>, ShimError> {
    let mut out = Vec::with_capacity(p.text.chars().count());
    let mut runs = p.runs.iter();
    let mut run = runs.next();
    let mut left = run.map_or(0, |r| r.len);
    let mut props = run.map(|r| field_marks(&r.props)).transpose()?.unwrap_or_default();
    let mut object = 0usize;
    for c in p.text.chars() {
        while left == 0 {
            run = runs.next();
            match run {
                Some(r) => {
                    left = r.len;
                    props = field_marks(&r.props)?;
                }
                None => {
                    props = Marks::new();
                    left = usize::MAX;
                }
            }
        }
        let mut marks = props.clone();
        if c == OBJ {
            if let Some(o) = p.objects.get(object) {
                marks.insert(OBJECT.to_owned(), bounded("an inline object", json_of(o)?)?);
            }
            object += 1;
        }
        out.push(marks);
        left = left.saturating_sub(c.len_utf8());
    }
    Ok(out)
}

fn field_marks(props: &CharProps) -> Result<Marks, ShimError> {
    let mut out = Marks::new();
    if let Value::Object(fields) = json_of(props)? {
        for (k, v) in fields {
            if !v.is_null() {
                out.insert(format!("{FIELD}{k}"), v);
            }
        }
    }
    Ok(out)
}

fn char_props(marks: &Marks) -> Result<CharProps, ShimError> {
    let fields: Map<String, Value> = marks.iter().filter_map(|(k, v)| k.strip_prefix(FIELD).map(|f| (f.to_owned(), v.clone()))).collect();
    serde_json::from_value(Value::Object(fields)).map_err(|e| ShimError::Model(format!("character formatting: {e}")))
}

/// The paragraph a block node of the shared tree holds.
pub fn para_from_node(node: &Value) -> Result<Paragraph, ShimError> {
    let chars: Chars = node.get("text").and_then(|t| t.get("$text")).map(spans_to_chars).unwrap_or_default();
    let mut text = String::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut objects: Vec<InlineObject> = Vec::new();
    let mut last: Option<Marks> = None;
    for (c, marks) in &chars {
        if *c == OBJ {
            // An object character whose object did not arrive is no character at all.
            let Some(o) = marks.get(OBJECT) else { continue };
            objects.push(serde_json::from_value(o.clone()).map_err(|e| ShimError::Model(format!("inline object: {e}")))?);
        }
        let fields: Marks = marks.iter().filter(|(k, _)| k.starts_with(FIELD)).map(|(k, v)| (k.clone(), v.clone())).collect();
        match (&last, runs.last_mut()) {
            (Some(prev), Some(run)) if *prev == fields => run.len += c.len_utf8(),
            _ => {
                runs.push(Run { len: c.len_utf8(), props: char_props(&fields)? });
                last = Some(fields);
            }
        }
        text.push(*c);
    }
    let value = |key: &str| node.get(key).map(|v| v.get("$value").unwrap_or(v));
    let props = match value("props") {
        Some(v) => serde_json::from_value::<ParaProps>(v.clone()).map_err(|e| ShimError::Model(format!("paragraph formatting: {e}")))?,
        None => ParaProps::default(),
    };
    let mark = match value("mark") {
        Some(v) => serde_json::from_value::<CharProps>(v.clone()).map_err(|e| ShimError::Model(format!("paragraph mark: {e}")))?,
        None => CharProps::default(),
    };
    let section = match value("section") {
        Some(v) if !v.is_null() => Some(Box::new(serde_json::from_value::<SectionProps>(v.clone()).map_err(|e| ShimError::Model(format!("section: {e}")))?)),
        _ => None,
    };
    Ok(Paragraph { text, runs, props, mark, objects, section, rev: next_rev() })
}

/// The block a node of the shared tree holds: a table when it has `v`, else a paragraph.
pub fn block_from_node(node: &Value) -> Result<Block, ShimError> {
    match node.get("v") {
        Some(v) => {
            let table: Table = serde_json::from_value(v.get("$value").unwrap_or(v).clone()).map_err(|e| ShimError::Model(format!("table: {e}")))?;
            Ok(Block::Table(table))
        }
        None => Ok(Block::Para(para_from_node(node)?)),
    }
}

/// The ids and blocks of the mirror's body, reusing `reuse(id)` when the shared block is the same content.
pub fn body_of(mirror: &Mirror, mut reuse: impl FnMut(&str, &Value) -> Option<Arc<Block>>) -> Result<(Vec<String>, Vec<Arc<Block>>), ShimError> {
    let ids: Vec<String> = mirror
        .get(&["body", "$list"])
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let mut kept = Vec::with_capacity(ids.len());
    let mut blocks = Vec::with_capacity(ids.len());
    for id in ids {
        // A listed id with no block (a peer's batch in flight) is not shown yet.
        let Some(node) = mirror.get(&["blocks", &id]) else { continue };
        let block = match reuse(&id, node) {
            Some(b) => b,
            None => Arc::new(block_from_node(node)?),
        };
        kept.push(id);
        blocks.push(block);
    }
    Ok((kept, blocks))
}

// ── ops ─────────────────────────────────────────────────────────────────────────────────────────────────────────────

fn set(path: &[&str], key: &str, value: Value) -> Value {
    json!({"op": "set", "path": path, "key": key, "value": value})
}

fn text_chunks(text: &[char]) -> impl Iterator<Item = String> + '_ {
    text.chunks(CHUNK).map(|c| c.iter().collect())
}

/// The mark ops that take per-character marks `from` to `to` (equal lengths): one op per run of characters whose value of
/// a key changes, a null value clearing it.
pub fn mark_ops(path: &[&str], from: &[Marks], to: &[Marks]) -> Vec<Value> {
    debug_assert_eq!(from.len(), to.len());
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for m in from.iter().chain(to.iter()) {
        keys.extend(m.keys().map(String::as_str));
    }
    let mut ops = Vec::new();
    for key in keys {
        let mut i = 0;
        while i < to.len() {
            let (want, have) = (to[i].get(key), from[i].get(key));
            if want == have {
                i += 1;
                continue;
            }
            let start = i;
            while i < to.len() && to[i].get(key) == want && from[i].get(key) != want {
                i += 1;
            }
            ops.push(json!({"op": "text.mark", "path": path, "start": start, "end": i, "key": key, "value": want.cloned().unwrap_or(Value::Null), "expand": false}));
        }
    }
    ops
}

/// The ops that make block `id` (a new map beneath `blocks`) and list it at body index `at`.
pub fn create_ops(id: &str, block: &Block, at: usize, out: &mut Vec<Value>) -> Result<(), ShimError> {
    out.push(json!({"op": "new", "path": ["blocks"], "key": id, "kind": "map"}));
    match block {
        Block::Table(t) => out.push(set(&["blocks", id], "v", bounded("a table", json_of(t)?)?)),
        Block::Para(p) => {
            let chars: Vec<char> = p.text.chars().collect();
            let mut chunks = text_chunks(&chars);
            match chunks.next() {
                Some(first) => out.push(json!({"op": "new", "path": ["blocks", id], "key": "text", "kind": "text", "text": first})),
                None => out.push(json!({"op": "new", "path": ["blocks", id], "key": "text", "kind": "text"})),
            }
            let mut at_char = CHUNK.min(chars.len());
            for more in chunks {
                let n = more.chars().count();
                out.push(json!({"op": "text.insert", "path": ["blocks", id, "text"], "at": at_char, "text": more}));
                at_char += n;
            }
            let marks = char_marks(p)?;
            let blank = vec![Marks::new(); marks.len()];
            out.extend(mark_ops(&["blocks", id, "text"], &blank, &marks));
            for (key, v) in para_values(p)? {
                if let Some(v) = v {
                    out.push(set(&["blocks", id], key, v));
                }
            }
        }
    }
    out.push(json!({"op": "list.insert", "path": ["body"], "at": at, "value": id}));
    Ok(())
}

/// A paragraph's JSON-valued properties (`None` = default / absent).
pub fn para_values(p: &Paragraph) -> Result<[(&'static str, Option<Value>); 3], ShimError> {
    let props = if p.props.is_empty() { None } else { Some(bounded("paragraph formatting", json_of(&p.props)?)?) };
    let mark = if p.mark.is_empty() { None } else { Some(bounded("a paragraph mark", json_of(&p.mark)?)?) };
    let section = match &p.section {
        Some(s) => Some(bounded("a section break", json_of(s)?)?),
        None => None,
    };
    Ok([("props", props), ("mark", mark), ("section", section)])
}

/// The ops that take block `old` to `new` under the same id (both paragraphs, or both tables); `None` when they are not
/// the same kind (the caller replaces the block). Returns whether the edit is text-only (typing).
pub fn edit_ops(id: &str, old: &Block, new: &Block, out: &mut Vec<Value>) -> Result<Option<EditKind>, ShimError> {
    match (old, new) {
        (Block::Table(a), Block::Table(b)) => {
            if a != b {
                out.push(set(&["blocks", id], "v", bounded("a table", json_of(b)?)?));
            }
            Ok(Some(EditKind::Structure))
        }
        (Block::Para(a), Block::Para(b)) => {
            let (old_chars, new_chars): (Vec<char>, Vec<char>) = (a.text.chars().collect(), b.text.chars().collect());
            let (old_marks, new_marks) = (char_marks(a)?, char_marks(b)?);
            // The one span that changed: everything outside the common prefix and suffix.
            let mut p = 0;
            while p < old_chars.len() && p < new_chars.len() && old_chars[p] == new_chars[p] {
                p += 1;
            }
            let mut s = 0;
            while s < old_chars.len() - p && s < new_chars.len() - p && old_chars[old_chars.len() - 1 - s] == new_chars[new_chars.len() - 1 - s] {
                s += 1;
            }
            let (del, ins) = (old_chars.len() - p - s, &new_chars[p..new_chars.len() - s]);
            let text_path = ["blocks", id, "text"];
            let mut text_changed = false;
            if del > 0 {
                out.push(json!({"op": "text.delete", "path": text_path, "at": p, "len": del}));
                text_changed = true;
            }
            let mut at = p;
            for chunk in text_chunks(ins) {
                let n = chunk.chars().count();
                out.push(json!({"op": "text.insert", "path": text_path, "at": at, "text": chunk}));
                at += n;
                text_changed = true;
            }
            // What the shared text holds now: the old marks outside the change, none inside it.
            let mut between: Vec<Marks> = Vec::with_capacity(new_marks.len());
            between.extend_from_slice(&old_marks[..p]);
            between.extend(std::iter::repeat_n(Marks::new(), ins.len()));
            between.extend_from_slice(&old_marks[old_marks.len() - s..]);
            let marks = mark_ops(&text_path, &between, &new_marks);
            let marked = !marks.is_empty();
            out.extend(marks);
            let (old_v, new_v) = (para_values(a)?, para_values(b)?);
            let mut valued = false;
            for ((key, before), (_, after)) in old_v.iter().zip(new_v.iter()) {
                if before != after {
                    valued = true;
                    match after {
                        Some(v) => out.push(set(&["blocks", id], key, v.clone())),
                        None => out.push(json!({"op": "remove", "path": ["blocks", id], "key": key})),
                    }
                }
            }
            Ok(Some(match (text_changed, marked || valued) {
                (true, false) => EditKind::Typing,
                (false, true) => EditKind::Format,
                _ => EditKind::Structure,
            }))
        }
        _ => Ok(None),
    }
}

/// What kind of change a batch is (its caption in the room's history, and whether it joins the last undo step).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Only characters were typed or deleted in one paragraph.
    Typing,
    /// Only formatting changed.
    Format,
    /// Anything else.
    Structure,
}

/// Whether two blocks hold the same content (text, per-character formatting, paragraph values), runs merged or not.
pub fn same_content(a: &Block, b: &Block) -> Result<bool, ShimError> {
    Ok(match (a, b) {
        (Block::Table(x), Block::Table(y)) => x == y,
        (Block::Para(x), Block::Para(y)) => {
            x.text == y.text && char_marks(x)? == char_marks(y)? && para_values(x)? == para_values(y)?
        }
        _ => false,
    })
}
