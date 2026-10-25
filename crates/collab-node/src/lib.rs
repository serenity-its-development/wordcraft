//! The collaboration shim for node: one [`Window`] is one editor window's side of the host bridge, with the document body held
//! as JSON so a test (OneOS's `craft-write-convergence.test.ts`) can drive two windows through the host's real adapter and
//! CRDT engine and compare what they end up showing.

use std::sync::Arc;

use serde_json::{Value, json};
use wasm_bindgen::prelude::*;
use wordcraft_collab::{Batch, Shim, changes_of};
use wordcraft_doc::Block;

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

#[wasm_bindgen]
pub struct Window {
    shim: Shim,
    /// What the editor shows.
    body: Vec<Arc<Block>>,
}

#[wasm_bindgen]
impl Window {
    /// A window showing `body_json` (an array of blocks), its ids drawn from `seed`.
    #[wasm_bindgen(constructor)]
    pub fn new(seed: u32, body_json: &str) -> Result<Window, JsError> {
        Ok(Window { shim: Shim::new(u64::from(seed) * 0x9e37_79b9 + 1), body: blocks_of(body_json)? })
    }

    /// The seed batches for the opened body (an array of `{seq, ops, label, undo, seed}`).
    pub fn seed(&mut self, now: f64) -> Result<String, JsError> {
        let batches = self.shim.seed(&self.body, now as u64).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(batches_json(&batches))
    }

    /// The person edited: the editor now shows `body_json`; answers the batches to send.
    pub fn edit(&mut self, body_json: &str, now: f64) -> Result<String, JsError> {
        self.body = blocks_of(body_json)?;
        let batches = self.shim.local(&self.body, now as u64).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(batches_json(&batches))
    }

    /// The host sent `craft:crdt:state`'s `doc`: show it.
    pub fn state(&mut self, doc_json: &str) -> Result<(), JsError> {
        let doc: Value = serde_json::from_str(doc_json).map_err(|e| JsError::new(&format!("state: {e}")))?;
        self.body = self.shim.remote_state(&doc).map_err(|e| JsError::new(&e.to_string()))?;
        Ok(())
    }

    /// The host sent `craft:crdt:delta`: `changes_json` is its `changes` (paths below `doc`, the leading `doc` already
    /// stripped); answers whether the shown body changed.
    pub fn delta(&mut self, changes_json: &str, ack: f64, now: f64) -> Result<bool, JsError> {
        let changes: Value = serde_json::from_str(changes_json).map_err(|e| JsError::new(&format!("changes: {e}")))?;
        let changes: Vec<_> = changes_of(&changes)
            .into_iter()
            .filter_map(|mut c| {
                (c.path.first().map(String::as_str) == Some("doc")).then(|| {
                    c.path.remove(0);
                    c
                })
            })
            .collect();
        match self.shim.remote_delta(&changes, ack as u64, now as u64).map_err(|e| JsError::new(&e.to_string()))? {
            Some(body) => {
                self.body = body;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// What the editor shows, as JSON blocks.
    pub fn body(&self) -> String {
        json_of(&self.body)
    }

    /// Each block's text and its per-character formatting, as one comparable JSON string.
    pub fn digest(&self) -> String {
        let blocks: Vec<Value> = self
            .body
            .iter()
            .map(|b| match &**b {
                Block::Para(p) => {
                    let marks = wordcraft_collab::blocks::char_marks(p).unwrap_or_default();
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
