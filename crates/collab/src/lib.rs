//! WordCraft's collaboration shim for OneOS (the "Write" editor).
//!
//! The host (OneOS) keeps ONE replica of the file's shared document and speaks to the editor frame in the host bridge's
//! operation contract (`plans/mona/craft-integration-design-2026-10-25.md` section 12.3). This crate is the editor's
//! side of it, pure and testable without a browser:
//!
//! * [`Shim::seed`] and [`Shim::local`] turn the editor's document body into ops (`new`, `set`, `text.insert`,
//!   `text.delete`, `text.mark`, `list.insert`, `list.delete`): paragraphs are CRDT text with a mark per character
//!   formatting field ([`blocks`] says the schema);
//! * [`Shim::remote_state`] and [`Shim::remote_delta`] turn what the host sends back (`craft:crdt:state`,
//!   `craft:crdt:delta`) into the new body to show.
//!
//! The frame's own edits are not echoed back by the host, so the shim keeps its view of the shared tree itself: the host's
//! truth (`confirmed`) plus the local batches the host has not yet acknowledged (`pending`), re-applied over every delta
//! (the contract's client-side prediction). A batch counts as acknowledged when a message carries `ack >= seq`, or after
//! [`PENDING_MS`] (the host is in the same page: a batch is applied within a message turn).
//!
//! Not shared live: styles, numbering, headers and footers, comments, tracked-change records and media. They are the
//! file's, as opened; a document edit that adds media or a style reaches other windows when the file is saved.

pub mod blocks;
pub mod mirror;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_doc::Block;

use blocks::{EditKind, body_of, create_ops, edit_ops, same_content};
use mirror::{Mirror, MirrorError};

/// A local batch is taken as applied by the host this long after it was sent, if no message acknowledged it sooner.
pub const PENDING_MS: u64 = 5_000;
/// At most this many local batches wait for an acknowledgement.
pub const MAX_PENDING: usize = 500;
/// The most ops one batch carries (the host's limit is 2000).
pub const BATCH_OPS: usize = 1_500;

/// Why the shim could not do what was asked. It is loud: the frame says it and falls back to single-editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShimError {
    /// A piece of the document is bigger than one shared value holds.
    TooLarge(String),
    /// The shared tree holds something the document model does not accept.
    Model(String),
    /// The shared tree and the shim disagree (an op did not apply).
    Mirror(String),
}

impl std::fmt::Display for ShimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShimError::TooLarge(m) | ShimError::Model(m) | ShimError::Mirror(m) => f.write_str(m),
        }
    }
}

impl From<MirrorError> for ShimError {
    fn from(e: MirrorError) -> Self {
        ShimError::Mirror(e.0)
    }
}

/// One batch of ops for `craft:crdt:ops`.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub seq: u64,
    pub ops: Vec<Value>,
    /// The app's own name for the edit (its caption in the room's history).
    pub label: String,
    /// Fold into the previous undo step (a typing burst).
    pub join: bool,
    /// The first batch of a seed (`seed: true`).
    pub seed: bool,
}

/// A change the host reports: set the tree at `path` (below `doc`) to `value`, or remove it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: Vec<String>,
    pub value: Option<Value>,
}

/// A host message's `changes` array.
#[must_use]
pub fn changes_of(v: &Value) -> Vec<Change> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let path: Vec<String> = c.get("path")?.as_array()?.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect();
                    let removed = c.get("removed").and_then(Value::as_bool) == Some(true);
                    Some(Change { path, value: if removed { None } else { c.get("value").cloned() } })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub struct Shim {
    rng: u64,
    ids: Vec<String>,
    synced: Vec<Arc<Block>>,
    /// The shared tree as this window shows it (the host's truth plus the batches not yet acknowledged).
    view: Mirror,
    /// The host's truth, kept only while batches are pending.
    confirmed: Option<Mirror>,
    pending: Vec<(u64, u64, Vec<Value>)>,
    seq: u64,
    last_typing_ms: Option<u64>,
}

impl Shim {
    /// `seed`: random bits for block ids (the caller draws them from the platform's randomness).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Shim { rng: seed | 1, ids: Vec::new(), synced: Vec::new(), view: Mirror::default(), confirmed: None, pending: Vec::new(), seq: 0, last_typing_ms: None }
    }

    /// The body as the shim last synced it.
    #[must_use]
    pub fn body(&self) -> &[Arc<Block>] {
        &self.synced
    }

    /// The shared tree as this window shows it.
    #[must_use]
    pub fn view(&self) -> &Value {
        &self.view.root
    }

    fn mint(&mut self) -> String {
        loop {
            // xorshift64*: ids only need to differ between the windows of one file; the seed is random per window.
            let mut x = self.rng;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.rng = x;
            let id = format!("b{:012x}", x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 16);
            if !self.ids.contains(&id) && self.view.get(&["blocks", &id]).is_none() {
                return id;
            }
        }
    }

    fn push(&mut self, ops: Vec<Value>, label: &str, join: bool, seed: bool, now_ms: u64) -> Result<Batch, ShimError> {
        if self.pending.is_empty() {
            self.confirmed = Some(self.view.clone());
        }
        for op in &ops {
            self.view.apply(op)?;
        }
        self.seq += 1;
        self.pending.push((self.seq, now_ms, ops.clone()));
        self.prune(now_ms)?;
        Ok(Batch { seq: self.seq, ops, label: label.to_owned(), join, seed })
    }

    /// Fold old and surplus pending batches into the host's truth (they were applied).
    fn prune(&mut self, now_ms: u64) -> Result<(), ShimError> {
        while let Some((_, sent, _)) = self.pending.first() {
            if self.pending.len() <= MAX_PENDING && now_ms.saturating_sub(*sent) < PENDING_MS {
                break;
            }
            let (_, _, ops) = self.pending.remove(0);
            if let Some(c) = self.confirmed.as_mut() {
                for op in &ops {
                    // An op that no longer applies was refused by the host too; the next state message resets us.
                    let _ = c.apply(op);
                }
            }
        }
        if self.pending.is_empty() {
            self.confirmed = None;
        }
        Ok(())
    }

    /// ★ SEED: the whole body as ops, in batches of whole blocks. The first batch is the seed (`seed: true`, one untracked
    /// change on the host); the rest follow as ordinary batches, each leaving the body complete up to its last block.
    pub fn seed(&mut self, body: &[Arc<Block>], now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        self.ids.clear();
        self.synced.clear();
        self.view = Mirror::default();
        self.confirmed = None;
        self.pending.clear();
        let mut first = vec![json!({"op": "ensure", "path": [], "key": "body", "kind": "list"}), json!({"op": "ensure", "path": [], "key": "blocks", "kind": "map"})];
        let mut batches = Vec::new();
        let mut current = std::mem::take(&mut first);
        let mut is_first = true;
        for (at, block) in body.iter().enumerate() {
            let id = self.mint();
            let mut ops = Vec::new();
            create_ops(&id, block, at, &mut ops)?;
            if !current.is_empty() && current.len() + ops.len() > BATCH_OPS {
                batches.push(self.push(std::mem::take(&mut current), if is_first { "Opened the file" } else { "Opened the file (more)" }, false, is_first, now_ms)?);
                is_first = false;
            }
            current.extend(ops);
            self.ids.push(id);
            self.synced.push(block.clone());
        }
        if !current.is_empty() {
            batches.push(self.push(current, if is_first { "Opened the file" } else { "Opened the file (more)" }, false, is_first, now_ms)?);
        }
        Ok(batches)
    }

    /// ★ A LOCAL EDIT: the ops that take what the peers hold (the last synced body) to `body`, or nothing when the body is
    /// the same. Typing in one paragraph joins the previous undo step when it follows within 1.5 s.
    pub fn local(&mut self, body: &[Arc<Block>], now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        let same = |a: &Arc<Block>, b: &Arc<Block>| Arc::ptr_eq(a, b) || **a == **b;
        let old = &self.synced;
        let mut pre = 0;
        while pre < old.len() && pre < body.len() && same(&old[pre], &body[pre]) {
            pre += 1;
        }
        let mut suf = 0;
        while suf < old.len() - pre && suf < body.len() - pre && same(&old[old.len() - 1 - suf], &body[body.len() - 1 - suf]) {
            suf += 1;
        }
        let (old_mid, new_mid) = (pre..old.len() - suf, pre..body.len() - suf);
        if old_mid.is_empty() && new_mid.is_empty() {
            return Ok(Vec::new());
        }
        let mut ops = Vec::new();
        let mut ids = self.ids.clone();
        let mut synced = self.synced.clone();
        let mut kinds: Vec<EditKind> = Vec::new();
        let paired = old_mid.len().min(new_mid.len());
        // Pair up the changed blocks by position: an edit in place, or a replacement when the kind changed.
        let mut replaced: Vec<(usize, Arc<Block>)> = Vec::new();
        for k in 0..paired {
            let i = pre + k;
            match edit_ops(&self.ids[i], &old[i], &body[i], &mut ops)? {
                Some(kind) => kinds.push(kind),
                None => replaced.push((i, body[i].clone())),
            }
            synced[i] = body[i].clone();
        }
        // A block that became another kind: remove it and make the new one in its place.
        for (i, block) in &replaced {
            let id = self.mint();
            ops.push(json!({"op": "list.delete", "path": ["body"], "at": i, "len": 1}));
            ops.push(json!({"op": "remove", "path": ["blocks"], "key": ids[*i]}));
            create_ops(&id, block, *i, &mut ops)?;
            ids[*i] = id;
            kinds.push(EditKind::Structure);
        }
        let tail = pre + paired;
        if old_mid.len() > paired {
            let extra = old_mid.len() - paired;
            ops.push(json!({"op": "list.delete", "path": ["body"], "at": tail, "len": extra}));
            for id in &ids[tail..tail + extra] {
                ops.push(json!({"op": "remove", "path": ["blocks"], "key": id}));
            }
            ids.drain(tail..tail + extra);
            synced.drain(tail..tail + extra);
            kinds.push(EditKind::Structure);
        }
        for (k, block) in body[tail..body.len() - suf].iter().enumerate() {
            if new_mid.len() <= paired {
                break;
            }
            let id = self.mint();
            create_ops(&id, block, tail + k, &mut ops)?;
            ids.insert(tail + k, id);
            synced.insert(tail + k, block.clone());
            kinds.push(EditKind::Structure);
        }
        if ops.is_empty() {
            // The blocks differ only in ways the shared tree does not hold (runs merged, a revision counter): nothing to say.
            self.synced = synced;
            return Ok(Vec::new());
        }
        let typing = kinds.len() == 1 && kinds[0] == EditKind::Typing;
        let join = typing && self.last_typing_ms.is_some_and(|t| now_ms.saturating_sub(t) < 1_500);
        self.last_typing_ms = if typing { Some(now_ms) } else { None };
        let label = match (kinds.len(), kinds.first()) {
            (1, Some(EditKind::Typing)) => "Typing",
            (1, Some(EditKind::Format)) => "Formatting",
            _ => "Edit",
        };
        self.ids = ids;
        self.synced = synced;
        // The host takes at most 2000 ops a batch: a bigger edit (a large paste) goes as several, in order - every prefix
        // of the ops leaves a valid tree.
        let mut batches = Vec::new();
        let mut rest = ops;
        let mut first = true;
        while !rest.is_empty() {
            let take: Vec<Value> = rest.drain(..BATCH_OPS.min(rest.len())).collect();
            let (name, joined) = if first { (label, join) } else { ("Edit (more)", false) };
            batches.push(self.push(take, name, joined, false, now_ms)?);
            first = false;
        }
        Ok(batches)
    }

    /// The whole shared state arrived (a join, a resync after a refusal, a snapshot): show it. Batches waiting for an
    /// acknowledgement are dropped - the state is the host's truth.
    pub fn remote_state(&mut self, doc: &Value) -> Result<Vec<Arc<Block>>, ShimError> {
        self.view = Mirror::new(doc.clone());
        self.confirmed = None;
        self.pending.clear();
        let synced: HashMap<String, Arc<Block>> = self.ids.iter().cloned().zip(self.synced.iter().cloned()).collect();
        let (ids, blocks) = body_of(&self.view, |id, node| reuse(&synced, id, node))?;
        self.ids = ids;
        self.synced = blocks.clone();
        Ok(blocks)
    }

    /// ★ A REMOTE CHANGE arrived: apply the host's `changes` (and `ack`: the last local batch it applied), put the batches
    /// still in flight back on top, and answer the body to show - `None` when it is the body already shown.
    pub fn remote_delta(&mut self, changes: &[Change], ack: u64, now_ms: u64) -> Result<Option<Vec<Arc<Block>>>, ShimError> {
        let mut dirty: BTreeSet<String> = BTreeSet::new();
        for c in changes {
            note(&mut dirty, &c.path);
        }
        match self.confirmed.take() {
            Some(mut truth) => {
                // The host applied our batches up to `ack` before it read what it reports.
                while self.pending.first().is_some_and(|(seq, _, _)| *seq <= ack) {
                    let (_, _, ops) = self.pending.remove(0);
                    for op in &ops {
                        let _ = truth.apply(op);
                    }
                }
                for c in changes {
                    truth.apply_change(&c.path, c.value.as_ref())?;
                }
                let mut view = truth.clone();
                for (_, _, ops) in &self.pending {
                    for op in ops {
                        if let Some(p) = op.get("path").and_then(Value::as_array) {
                            let path: Vec<String> = p.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect();
                            note(&mut dirty, &path);
                            if let (Some("remove" | "new"), Some(key)) = (op.get("op").and_then(Value::as_str), op.get("key").and_then(Value::as_str))
                                && path == ["blocks"]
                            {
                                dirty.insert(key.to_owned());
                            }
                        }
                        let _ = view.apply(op);
                    }
                }
                self.view = view;
                self.confirmed = if self.pending.is_empty() { None } else { Some(truth) };
            }
            None => {
                for c in changes {
                    self.view.apply_change(&c.path, c.value.as_ref())?;
                }
            }
        }
        self.prune(now_ms)?;
        let synced: HashMap<String, Arc<Block>> = self.ids.iter().cloned().zip(self.synced.iter().cloned()).collect();
        let (ids, blocks) = body_of(&self.view, |id, node| if dirty.contains(id) { reuse_if_same(&synced, id, node) } else { synced.get(id).cloned() })?;
        let unchanged = ids == self.ids && blocks.len() == self.synced.len() && blocks.iter().zip(&self.synced).all(|(a, b)| Arc::ptr_eq(a, b));
        self.ids = ids;
        self.synced = blocks.clone();
        Ok(if unchanged { None } else { Some(blocks) })
    }
}

/// A change below `blocks.<id>` makes that block dirty.
fn note(dirty: &mut BTreeSet<String>, path: &[String]) {
    if let ("blocks", Some(id)) = (path.first().map(String::as_str).unwrap_or(""), path.get(1)) {
        dirty.insert(id.clone());
    }
}

/// The block already shown under `id`, when the shared node holds the same content.
fn reuse(synced: &HashMap<String, Arc<Block>>, id: &str, node: &Value) -> Option<Arc<Block>> {
    reuse_if_same(synced, id, node)
}

fn reuse_if_same(synced: &HashMap<String, Arc<Block>>, id: &str, node: &Value) -> Option<Arc<Block>> {
    let old = synced.get(id)?;
    let fresh = blocks::block_from_node(node).ok()?;
    same_content(old, &fresh).ok()?.then(|| old.clone())
}

#[cfg(test)]
mod tests;
