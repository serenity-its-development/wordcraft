//! WordCraft's collaboration shim for OneOS (the "Write" editor).
//!
//! The host (OneOS) keeps ONE replica of the file's shared document and speaks to the editor frame in the host bridge's
//! operation contract (`plans/mona/craft-integration-design-2026-10-25.md` section 12.3). This crate is the editor's
//! side of it, pure and testable without a browser:
//!
//! * [`Shim::seed`] / [`Shim::seed_all`] and [`Shim::local`] / [`Shim::local_all`] turn the editor's document into ops
//!   (`new`, `ensure`, `set`, `remove`, `text.insert`, `text.delete`, `text.mark`, `list.insert`, `list.delete`);
//! * [`Shim::remote_state`] and [`Shim::remote_delta`] turn what the host sends back (`craft:crdt:state`,
//!   `craft:crdt:delta`) into the new body to show, and [`Shim::take_shown`] hands over everything else that changed.
//!
//! ★ WHAT IS SHARED, AND HOW (the schema beneath the doc's `craft` root):
//!
//! | what | where | how |
//! |---|---|---|
//! | body | `body` (ids), `blocks.<id>` | paragraphs are CRDT text with a mark per character-formatting field ([`blocks`]) |
//! | headers, footers | `parts.<pid>` = `{kind, body}` | the part's `body` is a list of block ids in the SAME `blocks` map, so typing in a header merges like typing in the body |
//! | styles | `styles.<id>`, `styleDefaults` | one value per style (last writer wins per style) |
//! | last section | `lastSection` | one value |
//! | pictures | inside the `obj` mark | the picture's key is its content address `cas-<sha256>.<ext>`; the bytes are never in the room ([`media`]) |
//! | comments | the host's comments tier | add, reply, edit, resolve, reopen, delete messages ([`comments`]) |
//! | cursors | the room's awareness | [`Shim::cursor_for`] out, [`Shim::peer_cursor`] in |
//!
//! Not shared live: numbering, footnotes and endnotes, text boxes, tracked-change records, a picture used as a background or
//! fill. They are the file's, as opened; a change to them reaches other windows when the file is saved.
//!
//! The frame's own edits are not echoed back by the host, so the shim keeps its view of the shared tree itself: the host's
//! truth (`confirmed`) plus the local batches the host has not yet acknowledged (`pending`), re-applied over every delta
//! (the contract's client-side prediction). A batch counts as acknowledged when a message carries `ack >= seq`, or after
//! [`PENDING_MS`] (the host is in the same page: a batch is applied within a message turn).

pub mod blocks;
pub mod comments;
pub mod media;
pub mod mirror;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};
use wordcraft_doc::{Block, CharProps, Comment, Document, ParaProps, Part, PartKind, Pos, SectionProps, StoryRef, Style, StyleSheet};

use blocks::{EditKind, Shared, body_of, create_ops, edit_ops, same_content};
use comments::{CommentMsg, CommentSync, CommentsUpdate};
use media::MediaIndex;
use mirror::{Mirror, MirrorError};

/// A local batch is taken as applied by the host this long after it was sent, if no message acknowledged it sooner.
pub const PENDING_MS: u64 = 5_000;
/// At most this many local batches wait for an acknowledgement.
pub const MAX_PENDING: usize = 500;
/// The most ops one batch carries (the host's limit is 2000).
pub const BATCH_OPS: usize = 1_500;
/// The longest key the host takes for a map entry.
const MAX_KEY: usize = 128;
/// The largest JSON value one `set` carries (the host's limit is 64 KiB).
const MAX_VALUE: usize = 60_000;

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

/// The editor's document, beyond the body: what [`Shim::seed_all`] and [`Shim::local_all`] read.
pub struct Extras<'a> {
    pub styles: &'a StyleSheet,
    pub last_section: &'a SectionProps,
    pub parts: &'a BTreeMap<u32, Part>,
    pub comments: &'a BTreeMap<u32, Comment>,
    pub media: &'a BTreeMap<String, Arc<Vec<u8>>>,
}

impl<'a> Extras<'a> {
    /// The extras of an editor document.
    #[must_use]
    pub fn of(doc: &'a Document) -> Self {
        Extras { styles: &doc.styles, last_section: &doc.last_section, parts: &doc.parts, comments: &doc.comments, media: &doc.media }
    }
}

/// A style change to apply to the editor's stylesheet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StylesUpdate {
    /// The room's styles replace the stylesheet's (a window that joined): styles absent from `set` are removed.
    pub replace_all: bool,
    /// Styles to insert or replace, by id.
    pub set: Vec<Style>,
    /// Style ids to remove.
    pub remove: Vec<String>,
    /// The document defaults, when they changed: character, paragraph, and whether the file states a paragraph default.
    pub defaults: Option<(CharProps, ParaProps, bool)>,
}

impl StylesUpdate {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.replace_all && self.set.is_empty() && self.remove.is_empty() && self.defaults.is_none()
    }
}

/// Everything beside the body that a remote change altered, for the editor to apply ([`Shim::take_shown`]).
#[derive(Debug, Clone, Default)]
pub struct Shown {
    pub styles: StylesUpdate,
    pub last_section: Option<SectionProps>,
    /// The room's headers and footers replace the document's (a window that joined): those absent from `parts_set` are removed.
    pub parts_replace_all: bool,
    /// Header and footer parts to insert or replace, by part id.
    pub parts_set: BTreeMap<u32, Part>,
    /// Header and footer part ids that are gone.
    pub parts_remove: Vec<u32>,
    pub comments: CommentsUpdate,
    /// Content addresses of pictures the shared document uses that this window has no bytes for.
    pub wanted: Vec<String>,
}

impl Shown {
    /// Whether the editor's document has anything to take in (the wanted pictures are asked of the host, not applied).
    #[must_use]
    pub fn changes_document(&self) -> bool {
        !self.styles.is_empty() || self.last_section.is_some() || self.parts_replace_all || !self.parts_set.is_empty() || !self.parts_remove.is_empty() || !self.comments.is_empty()
    }

    /// Apply to the editor's document what remote changes altered beyond its body: styles, the last section, headers and
    /// footers, comments (each comment's text is a `Comment` part). Nothing here is an edit of this person's, so the shim already
    /// counts it as synced.
    pub fn apply_to(&self, doc: &mut Document) {
        if self.styles.replace_all {
            doc.styles.styles.clear();
        }
        for s in &self.styles.set {
            match doc.styles.styles.iter_mut().find(|x| x.id == s.id) {
                Some(slot) => *slot = s.clone(),
                None => doc.styles.styles.push(s.clone()),
            }
        }
        doc.styles.styles.retain(|s| !self.styles.remove.contains(&s.id));
        if let Some((chr, para, present)) = &self.styles.defaults {
            doc.styles.default_chr = chr.clone();
            doc.styles.default_para = para.clone();
            doc.styles.default_para_present = *present;
        }
        if let Some(s) = &self.last_section {
            doc.last_section = s.clone();
        }
        if self.parts_replace_all {
            doc.parts.retain(|_, p| !matches!(p.kind, PartKind::Header | PartKind::Footer));
        }
        for (pid, part) in &self.parts_set {
            doc.parts.insert(*pid, part.clone());
        }
        for pid in &self.parts_remove {
            doc.parts.remove(pid);
        }
        for c in &self.comments.upsert {
            let blocks = vec![wordcraft_doc::para_block(wordcraft_doc::Paragraph::with_text(&c.text, CharProps::default()))];
            let held = doc.comments.get(&c.wc).map(|x| x.part).filter(|p| doc.parts.contains_key(p));
            let part = match held {
                Some(p) => {
                    if let Some(slot) = doc.parts.get_mut(&p) {
                        slot.blocks = blocks;
                    }
                    p
                }
                None => doc.try_add_part(PartKind::Comment, blocks).unwrap_or(0),
            };
            let (durable_id, date_utc) = doc.comments.get(&c.wc).map_or((None, Some(c.date.clone())), |x| (x.durable_id.clone(), x.date_utc.clone()));
            doc.comments.insert(
                c.wc,
                Comment { author: c.author.clone(), initials: c.initials.clone(), date: c.date.clone(), date_utc, parent: c.parent, resolved: c.resolved, part, durable_id },
            );
        }
        for wc in &self.comments.remove {
            if let Some(c) = doc.comments.remove(wc) {
                doc.parts.remove(&c.part);
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
            && self.last_section.is_none()
            && !self.parts_replace_all
            && self.parts_set.is_empty()
            && self.parts_remove.is_empty()
            && self.comments.is_empty()
            && self.wanted.is_empty()
    }
}

/// Where another person is, resolved against this window's document.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerCursor {
    pub anchor: Pos,
    pub focus: Pos,
}

/// One body (the document's, or a header's or footer's): the block ids the shared list holds, and the blocks shown for them.
#[derive(Debug, Clone, Default)]
struct BodySync {
    ids: Vec<String>,
    synced: Vec<Arc<Block>>,
}

/// A shared header or footer.
#[derive(Debug, Clone)]
struct PartSync {
    kind: PartKind,
    body: BodySync,
}

/// What the tree holds for the styles, as values (so a diff is a comparison, not a re-serialisation).
#[derive(Debug, Clone, Default)]
struct StylesSync {
    by_id: BTreeMap<String, Value>,
    defaults: Option<Value>,
}

/// Block ids: random per window, and never one the tree or this window already uses.
struct Minter {
    rng: u64,
    minted: HashSet<String>,
}

impl Minter {
    fn mint(&mut self, taken: &dyn Fn(&str) -> bool) -> String {
        loop {
            // xorshift64*: ids only need to differ between the windows of one file; the seed is random per window.
            let mut x = self.rng;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.rng = x;
            let id = format!("b{:012x}", x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 16);
            if !self.minted.contains(&id) && !taken(&id) {
                self.minted.insert(id.clone());
                return id;
            }
        }
    }
}

pub struct Shim {
    minter: Minter,
    main: BodySync,
    parts: BTreeMap<u32, PartSync>,
    styles: StylesSync,
    /// The stylesheet and last section as last compared with the tree: an edit that touches neither skips the comparison.
    styles_seen: Option<(StyleSheet, SectionProps)>,
    last_section: Option<Value>,
    media: MediaIndex,
    comments: CommentSync,
    comment_msgs: Vec<CommentMsg>,
    /// The host's comments as last told (the whole value; a delta changes a part of it).
    host_comments: Value,
    /// The shared tree as this window shows it (the host's truth plus the batches not yet acknowledged).
    view: Mirror,
    /// The host's truth, kept only while batches are pending.
    confirmed: Option<Mirror>,
    pending: Vec<(u64, u64, Vec<Value>)>,
    seq: u64,
    last_typing_ms: Option<u64>,
    shown: Shown,
}

/// The root keys of the extras, and what container each is.
const ROOTS: [(&str, &str); 2] = [("styles", "map"), ("parts", "map")];

fn json_of<T: serde::Serialize>(v: &T) -> Result<Value, ShimError> {
    serde_json::to_value(v).map_err(|e| ShimError::Model(e.to_string()))
}

fn bounded(what: &str, v: Value) -> Result<Value, ShimError> {
    let n = v.to_string().len();
    if n > MAX_VALUE { Err(ShimError::TooLarge(format!("{what} is {n} bytes of data - more than one shared value holds ({MAX_VALUE})"))) } else { Ok(v) }
}

fn part_kind_name(k: PartKind) -> Option<&'static str> {
    match k {
        PartKind::Header => Some("header"),
        PartKind::Footer => Some("footer"),
        _ => None,
    }
}

fn part_kind_of(name: &str) -> Option<PartKind> {
    match name {
        "header" => Some(PartKind::Header),
        "footer" => Some(PartKind::Footer),
        _ => None,
    }
}

/// The value a map property holds (an object property is `{"$value": v}`).
fn plain(v: &Value) -> &Value {
    v.get("$value").unwrap_or(v)
}

fn taken_fn<'a>(view: &'a Mirror, ids: &'a [String]) -> impl Fn(&str) -> bool + 'a {
    move |id| ids.iter().any(|i| i == id) || view.get(&["blocks", id]).is_some()
}

impl Shim {
    /// `seed`: random bits for block ids (the caller draws them from the platform's randomness).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Shim {
            minter: Minter { rng: seed | 1, minted: HashSet::new() },
            main: BodySync::default(),
            parts: BTreeMap::new(),
            styles: StylesSync::default(),
            styles_seen: None,
            last_section: None,
            media: MediaIndex::default(),
            comments: CommentSync::default(),
            comment_msgs: Vec::new(),
            host_comments: Value::Null,
            view: Mirror::default(),
            confirmed: None,
            pending: Vec::new(),
            seq: 0,
            last_typing_ms: None,
            shown: Shown::default(),
        }
    }

    /// The body as the shim last synced it.
    #[must_use]
    pub fn body(&self) -> &[Arc<Block>] {
        &self.main.synced
    }

    /// The block ids of the body, in order.
    #[must_use]
    pub fn ids(&self) -> &[String] {
        &self.main.ids
    }

    /// The shared tree as this window shows it.
    #[must_use]
    pub fn view(&self) -> &Value {
        &self.view.root
    }

    /// The pictures this window knows (read-only).
    #[must_use]
    pub fn media(&self) -> &MediaIndex {
        &self.media
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

    /// Send `ops` as one or more batches (the host takes at most 2000 ops a batch; every prefix of the ops leaves a valid tree).
    fn emit(&mut self, ops: Vec<Value>, label: &str, join: bool, now_ms: u64, out: &mut Vec<Batch>) -> Result<(), ShimError> {
        let mut rest = ops;
        let mut first = true;
        while !rest.is_empty() {
            let take: Vec<Value> = rest.drain(..BATCH_OPS.min(rest.len())).collect();
            let (name, joined) = if first { (label.to_owned(), join) } else { (format!("{label} (more)"), false) };
            out.push(self.push(take, &name, joined, false, now_ms)?);
            first = false;
        }
        Ok(())
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

    // ── seed ──────────────────────────────────────────────────────────────────────────────────────────────────────

    /// ★ SEED: the whole body as ops, in batches of whole blocks. The first batch is the seed (`seed: true`, one untracked
    /// change on the host); the rest follow as ordinary batches, each leaving the body complete up to its last block.
    pub fn seed(&mut self, body: &[Arc<Block>], now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        self.seed_inner(body, None, now_ms)
    }

    /// [`Shim::seed`], and the rest of the document too: headers and footers, styles, the last section, comments.
    pub fn seed_all(&mut self, body: &[Arc<Block>], extras: &Extras<'_>, now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        self.seed_inner(body, Some(extras), now_ms)
    }

    fn seed_inner(&mut self, body: &[Arc<Block>], extras: Option<&Extras<'_>>, now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        self.main = BodySync::default();
        self.parts.clear();
        self.styles = StylesSync::default();
        self.styles_seen = None;
        self.last_section = None;
        self.comments = CommentSync::default();
        self.comment_msgs.clear();
        self.shown = Shown::default();
        self.view = Mirror::default();
        self.confirmed = None;
        self.pending.clear();
        self.minter.minted.clear();
        if let Some(e) = extras {
            self.media.refresh(e.media);
        }
        let mut current = vec![json!({"op": "ensure", "path": [], "key": "body", "kind": "list"}), json!({"op": "ensure", "path": [], "key": "blocks", "kind": "map"})];
        if extras.is_some() {
            current.extend(ROOTS.iter().map(|(k, kind)| json!({"op": "ensure", "path": [], "key": k, "kind": kind})));
        }
        let mut batches = Vec::new();
        let mut is_first = true;
        let mut bs = BodySync::default();
        for (at, block) in body.iter().enumerate() {
            let mut ops = Vec::new();
            let id = {
                let cx = Shared::new(&self.media);
                let id = self.minter.mint(&taken_fn(&self.view, &bs.ids));
                create_ops(&["body"], &id, block, at, &mut ops, &cx)?;
                id
            };
            if !current.is_empty() && current.len() + ops.len() > BATCH_OPS {
                batches.push(self.push(std::mem::take(&mut current), if is_first { "Opened the file" } else { "Opened the file (more)" }, false, is_first, now_ms)?);
                is_first = false;
            }
            current.extend(ops);
            bs.ids.push(id);
            bs.synced.push(block.clone());
        }
        self.main = bs;
        if !current.is_empty() {
            batches.push(self.push(current, if is_first { "Opened the file" } else { "Opened the file (more)" }, false, is_first, now_ms)?);
        }
        if let Some(e) = extras {
            self.extras_local(e, now_ms, &mut batches)?;
        }
        Ok(batches)
    }

    // ── local edits ───────────────────────────────────────────────────────────────────────────────────────────────

    /// Diff one body against what it showed last: the ops, the kinds of change, and the body synced afterwards (`None`: the
    /// same). Nothing is committed to the shim; an error leaves it as it was.
    #[allow(clippy::too_many_arguments)]
    fn diff_body(
        minter: &mut Minter,
        view: &Mirror,
        cx: &Shared<'_>,
        list: &[&str],
        bs: &BodySync,
        body: &[Arc<Block>],
        ops: &mut Vec<Value>,
        kinds: &mut Vec<EditKind>,
    ) -> Result<Option<BodySync>, ShimError> {
        let same = |a: &Arc<Block>, b: &Arc<Block>| Arc::ptr_eq(a, b) || **a == **b;
        let old = &bs.synced;
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
            return Ok(None);
        }
        let mut ids = bs.ids.clone();
        let mut synced = bs.synced.clone();
        let paired = old_mid.len().min(new_mid.len());
        // Pair up the changed blocks by position: an edit in place, or a replacement when the kind changed.
        let mut replaced: Vec<(usize, Arc<Block>)> = Vec::new();
        for k in 0..paired {
            let i = pre + k;
            match edit_ops(&bs.ids[i], &old[i], &body[i], ops, cx)? {
                Some(kind) => kinds.push(kind),
                None => replaced.push((i, body[i].clone())),
            }
            synced[i] = body[i].clone();
        }
        // A block that became another kind: remove it and make the new one in its place.
        for (i, block) in &replaced {
            let id = minter.mint(&taken_fn(view, &ids));
            ops.push(json!({"op": "list.delete", "path": list, "at": i, "len": 1}));
            ops.push(json!({"op": "remove", "path": ["blocks"], "key": ids[*i]}));
            create_ops(list, &id, block, *i, ops, cx)?;
            ids[*i] = id;
            kinds.push(EditKind::Structure);
        }
        let tail = pre + paired;
        if old_mid.len() > paired {
            let extra = old_mid.len() - paired;
            ops.push(json!({"op": "list.delete", "path": list, "at": tail, "len": extra}));
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
            let id = minter.mint(&taken_fn(view, &ids));
            create_ops(list, &id, block, tail + k, ops, cx)?;
            ids.insert(tail + k, id);
            synced.insert(tail + k, block.clone());
            kinds.push(EditKind::Structure);
        }
        Ok(Some(BodySync { ids, synced }))
    }

    /// ★ A LOCAL EDIT of the body: the ops that take what the peers hold (the last synced body) to `body`, or nothing when
    /// the body is the same. Typing in one paragraph joins the previous undo step when it follows within 1.5 s.
    pub fn local(&mut self, body: &[Arc<Block>], now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        let mut ops = Vec::new();
        let mut kinds: Vec<EditKind> = Vec::new();
        self.minter.minted.clear();
        let next = {
            let cx = Shared::new(&self.media);
            Self::diff_body(&mut self.minter, &self.view, &cx, &["body"], &self.main, body, &mut ops, &mut kinds)?
        };
        let Some(next) = next else { return Ok(Vec::new()) };
        if ops.is_empty() {
            // The blocks differ only in ways the shared tree does not hold (runs merged, a revision counter): nothing to say.
            self.main = next;
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
        self.main = next;
        let mut batches = Vec::new();
        self.emit(ops, label, join, now_ms, &mut batches)?;
        Ok(batches)
    }

    /// [`Shim::local`], and the rest of the document: pictures, headers and footers, styles, the last section, and the comments
    /// ([`Shim::take_comment_msgs`] has the messages for those).
    pub fn local_all(&mut self, body: &[Arc<Block>], extras: &Extras<'_>, now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        self.media.refresh(extras.media);
        let mut batches = self.local(body, now_ms)?;
        self.extras_local(extras, now_ms, &mut batches)?;
        Ok(batches)
    }

    /// Everything beside the body.
    fn extras_local(&mut self, e: &Extras<'_>, now_ms: u64, out: &mut Vec<Batch>) -> Result<(), ShimError> {
        // A room seeded before these existed lacks their roots: made now (`ensure` merges two people doing it at once).
        let missing: Vec<Value> = ROOTS
            .iter()
            .filter(|(k, _)| self.view.get(&[*k]).is_none())
            .map(|(k, kind)| json!({"op": "ensure", "path": [], "key": k, "kind": kind}))
            .collect();
        if !missing.is_empty() {
            self.emit(missing, "Opened the file", false, now_ms, out)?;
        }
        self.minter.minted.clear();
        self.parts_local(e.parts, now_ms, out)?;
        self.styles_local(e.styles, e.last_section, now_ms, out)?;
        // Comments: the body is synced first, so an anchor can be found in it.
        let main = &self.main;
        let anchor_of = |wc: u32| anchor_in(main, wc);
        let msgs = self.comments.local(e.comments, e.parts, &anchor_of);
        self.comment_msgs.extend(msgs);
        Ok(())
    }

    fn parts_local(&mut self, parts: &BTreeMap<u32, Part>, now_ms: u64, out: &mut Vec<Batch>) -> Result<(), ShimError> {
        if self.view.get(&["parts"]).is_none() {
            return Ok(());
        }
        let wanted: BTreeMap<u32, &Part> = parts.iter().filter(|(_, p)| part_kind_name(p.kind).is_some()).map(|(k, p)| (*k, p)).collect();
        // Gone here.
        let gone: Vec<u32> = self.parts.keys().filter(|k| !wanted.contains_key(k)).copied().collect();
        for pid in gone {
            let sync = self.parts.remove(&pid);
            let mut ops = vec![json!({"op": "remove", "path": ["parts"], "key": pid.to_string()})];
            for id in sync.iter().flat_map(|s| s.body.ids.iter()) {
                ops.push(json!({"op": "remove", "path": ["blocks"], "key": id}));
            }
            self.emit(ops, "Remove a header or footer", false, now_ms, out)?;
        }
        for (pid, part) in wanted {
            let key = pid.to_string();
            let kind = part_kind_name(part.kind).unwrap_or("header");
            let label = if kind == "header" { "Header" } else { "Footer" };
            let list = ["parts", key.as_str(), "body"];
            let mut ops = Vec::new();
            let mut kinds = Vec::new();
            let next = match self.parts.get(&pid) {
                None => {
                    ops.push(json!({"op": "new", "path": ["parts"], "key": key, "kind": "map"}));
                    ops.push(json!({"op": "set", "path": ["parts", &key], "key": "kind", "value": kind}));
                    ops.push(json!({"op": "new", "path": ["parts", &key], "key": "body", "kind": "list"}));
                    let cx = Shared::new(&self.media);
                    let mut bs = BodySync::default();
                    for (at, block) in part.blocks.iter().enumerate() {
                        let id = self.minter.mint(&taken_fn(&self.view, &bs.ids));
                        create_ops(&list, &id, block, at, &mut ops, &cx)?;
                        bs.ids.push(id);
                        bs.synced.push(block.clone());
                    }
                    Some(bs)
                }
                Some(have) => {
                    let cx = Shared::new(&self.media);
                    Self::diff_body(&mut self.minter, &self.view, &cx, &list, &have.body, &part.blocks, &mut ops, &mut kinds)?
                }
            };
            let Some(next) = next else { continue };
            self.parts.entry(pid).or_insert_with(|| PartSync { kind: part.kind, body: BodySync::default() }).body = next;
            if !ops.is_empty() {
                self.emit(ops, label, false, now_ms, out)?;
            }
        }
        Ok(())
    }

    fn styles_local(&mut self, sheet: &StyleSheet, last_section: &SectionProps, now_ms: u64, out: &mut Vec<Batch>) -> Result<(), ShimError> {
        if self.styles_seen.as_ref().is_some_and(|(s, l)| s == sheet && l == last_section) {
            return Ok(());
        }
        let mut ops = Vec::new();
        if self.view.get(&["styles"]).is_some() {
            let mut now: BTreeMap<String, Value> = BTreeMap::new();
            for s in &sheet.styles {
                if s.id.is_empty() || s.id.len() > MAX_KEY {
                    return Err(ShimError::TooLarge(format!("the style id '{}' is not a key the host takes (1 to {MAX_KEY} bytes)", s.id)));
                }
                now.insert(s.id.clone(), bounded(&format!("the style {}", s.id), json_of(s)?)?);
            }
            for (id, v) in &now {
                if self.styles.by_id.get(id) != Some(v) {
                    ops.push(json!({"op": "set", "path": ["styles"], "key": id, "value": v}));
                }
            }
            for id in self.styles.by_id.keys() {
                if !now.contains_key(id) {
                    ops.push(json!({"op": "remove", "path": ["styles"], "key": id}));
                }
            }
            let defaults = bounded(
                "the document defaults",
                json!({"chr": json_of(&sheet.default_chr)?, "para": json_of(&sheet.default_para)?, "present": sheet.default_para_present}),
            )?;
            if self.styles.defaults.as_ref() != Some(&defaults) {
                ops.push(json!({"op": "set", "path": [], "key": "styleDefaults", "value": defaults}));
            }
            self.styles.by_id = now;
            self.styles.defaults = Some(defaults);
        }
        let section = bounded("the last section", json_of(last_section)?)?;
        if self.last_section.as_ref() != Some(&section) {
            ops.push(json!({"op": "set", "path": [], "key": "lastSection", "value": section}));
            self.last_section = Some(section);
        }
        if !ops.is_empty() {
            self.emit(ops, "Styles and page setup", false, now_ms, out)?;
        }
        self.styles_seen = Some((sheet.clone(), last_section.clone()));
        Ok(())
    }

    /// The comment messages the last local calls produced. Send each as `craft:crdt:comment`.
    pub fn take_comment_msgs(&mut self) -> Vec<CommentMsg> {
        std::mem::take(&mut self.comment_msgs)
    }

    // ── cursors ───────────────────────────────────────────────────────────────────────────────────────────────────

    /// The body of a `craft:crdt:cursor` message for a selection in the body or a header or footer: the block's shared id and the
    /// character offset, and the same for the other end when text is selected. `None` for a place the shared tree does not hold.
    #[must_use]
    pub fn cursor_for(&self, anchor: &Pos, focus: &Pos) -> Option<Value> {
        let end = |p: &Pos| -> Option<Value> {
            let (sync, story) = match p.story {
                StoryRef::Body => (&self.main, Value::Null),
                StoryRef::Part(pid) => (&self.parts.get(&pid)?.body, json!(pid)),
            };
            let block = *p.path.0.first()? as usize;
            let id = sync.ids.get(block)?;
            // Inside a table the place is the table; in a paragraph it is a character.
            let at = match (p.path.0.len(), sync.synced.get(block).map(|b| &**b)) {
                (1, Some(Block::Para(para))) => para.text.get(..p.off.min(para.text.len())).map_or(0, |t| t.chars().count()),
                _ => 0,
            };
            Some(json!({"s": story, "b": id, "o": at}))
        };
        let mut body = json!({"f": end(focus)?});
        if anchor != focus
            && let Some(a) = end(anchor)
        {
            body["a"] = a;
        }
        Some(body)
    }

    /// Another person's cursor body, resolved against this window's document.
    #[must_use]
    pub fn peer_cursor(&self, cursor: &Value) -> Option<PeerCursor> {
        let place = |v: &Value| -> Option<Pos> {
            let id = v.get("b")?.as_str()?;
            let at = usize::try_from(v.get("o")?.as_u64()?).ok()?;
            let (story, sync) = match v.get("s").and_then(Value::as_u64) {
                Some(pid) => {
                    let pid = u32::try_from(pid).ok()?;
                    (StoryRef::Part(pid), &self.parts.get(&pid)?.body)
                }
                None => (StoryRef::Body, &self.main),
            };
            let block = sync.ids.iter().position(|i| i == id)?;
            let off = match &**sync.synced.get(block)? {
                Block::Para(p) => p.text.char_indices().nth(at).map_or(p.text.len(), |(b, _)| b),
                Block::Table(_) => 0,
            };
            Some(Pos { story, path: wordcraft_doc::Path::top(block), off })
        };
        let focus = place(cursor.get("f")?)?;
        let anchor = cursor.get("a").and_then(place).unwrap_or_else(|| focus.clone());
        Some(PeerCursor { anchor, focus })
    }

    // ── remote changes ────────────────────────────────────────────────────────────────────────────────────────────

    fn want_all(&mut self, wanted: Vec<String>) {
        for h in wanted {
            self.media.want(&h);
        }
    }

    /// Recompute everything beside the main body from the view. `dirty`: the block ids a change touched; `touched`: the root keys
    /// it touched (`None` for both: everything).
    fn read_extras(&mut self, dirty: Option<&BTreeSet<String>>, touched: Option<&BTreeSet<String>>) -> Result<(), ShimError> {
        let is_touched = |k: &str| touched.is_none_or(|t| t.contains(k));
        let cx = Shared::new(&self.media);
        // Headers and footers.
        if (is_touched("parts") || dirty.is_none_or(|d| !d.is_empty()))
            && let Some(map) = self.view.get(&["parts"]).and_then(Value::as_object)
        {
            let mut present: BTreeSet<u32> = BTreeSet::new();
            for (key, node) in map {
                let Ok(pid) = key.parse::<u32>() else { continue };
                let Some(kind) = node.get("kind").map(plain).and_then(Value::as_str).and_then(part_kind_of) else { continue };
                present.insert(pid);
                let list = ["parts", key.as_str(), "body"];
                let old = self.parts.get(&pid);
                let synced: HashMap<String, Arc<Block>> =
                    old.map(|o| o.body.ids.iter().cloned().zip(o.body.synced.iter().cloned()).collect()).unwrap_or_default();
                let (ids, blocks) = body_of(&self.view, &list, &cx, |id, node| match dirty {
                    Some(d) if !d.contains(id) => synced.get(id).cloned(),
                    _ => reuse_if_same(&synced, id, node, &cx),
                })?;
                let unchanged = old.is_some_and(|o| {
                    o.kind == kind && o.body.ids == ids && blocks.len() == o.body.synced.len() && blocks.iter().zip(&o.body.synced).all(|(a, b)| Arc::ptr_eq(a, b))
                });
                if unchanged {
                    continue;
                }
                self.shown.parts_set.insert(pid, Part { kind, blocks: blocks.clone(), ..Default::default() });
                self.parts.insert(pid, PartSync { kind, body: BodySync { ids, synced: blocks } });
            }
            let gone: Vec<u32> = self.parts.keys().filter(|k| !present.contains(k)).copied().collect();
            for pid in gone {
                self.parts.remove(&pid);
                self.shown.parts_set.remove(&pid);
                self.shown.parts_remove.push(pid);
            }
        }
        // Styles. What the room holds comes back through the CRDT with its numbers written another way (20 for 20.0), so a value is
        // compared in the model's own form: parsed, and written again.
        if is_touched("styles")
            && let Some(map) = self.view.get(&["styles"]).and_then(Value::as_object)
        {
            let mut remote: BTreeMap<String, (Value, Style)> = BTreeMap::new();
            for (id, v) in map {
                let style = serde_json::from_value::<Style>(plain(v).clone()).map_err(|e| ShimError::Model(format!("style {id}: {e}")))?;
                remote.insert(id.clone(), (json_of(&style)?, style));
            }
            for (id, (canon, style)) in &remote {
                if self.styles.by_id.get(id) != Some(canon) {
                    self.shown.styles.set.push(style.clone());
                }
            }
            for id in self.styles.by_id.keys() {
                if !remote.contains_key(id) {
                    self.shown.styles.remove.push(id.clone());
                }
            }
            self.styles.by_id = remote.into_iter().map(|(k, (canon, _))| (k, canon)).collect();
        }
        if is_touched("styleDefaults")
            && let Some(d) = self.view.get(&["styleDefaults"]).map(|v| plain(v).clone())
        {
            let chr: CharProps = serde_json::from_value(d.get("chr").cloned().unwrap_or(Value::Null)).map_err(|e| ShimError::Model(format!("document defaults: {e}")))?;
            let para: ParaProps = serde_json::from_value(d.get("para").cloned().unwrap_or(Value::Null)).map_err(|e| ShimError::Model(format!("document defaults: {e}")))?;
            let present = d.get("present").and_then(Value::as_bool) == Some(true);
            let canon = json!({"chr": json_of(&chr)?, "para": json_of(&para)?, "present": present});
            if self.styles.defaults.as_ref() != Some(&canon) {
                self.shown.styles.defaults = Some((chr, para, present));
                self.styles.defaults = Some(canon);
            }
        }
        // The last section.
        if is_touched("lastSection")
            && let Some(v) = self.view.get(&["lastSection"]).map(|v| plain(v).clone())
        {
            let section: SectionProps = serde_json::from_value(v).map_err(|e| ShimError::Model(format!("last section: {e}")))?;
            let canon = json_of(&section)?;
            if self.last_section.as_ref() != Some(&canon) {
                self.shown.last_section = Some(section);
                self.last_section = Some(canon);
            }
        }
        let wanted: Vec<String> = cx.wanted.borrow().iter().cloned().collect();
        drop(cx);
        self.want_all(wanted);
        Ok(())
    }

    /// The whole shared state arrived (a join, a resync after a refusal, a snapshot): show it. Batches waiting for an
    /// acknowledgement are dropped - the state is the host's truth. Call [`Shim::take_shown`] after it.
    pub fn remote_state(&mut self, doc: &Value) -> Result<Vec<Arc<Block>>, ShimError> {
        self.view = Mirror::new(doc.clone());
        self.confirmed = None;
        self.pending.clear();
        let synced: HashMap<String, Arc<Block>> = self.main.ids.iter().cloned().zip(self.main.synced.iter().cloned()).collect();
        let cx = Shared::new(&self.media);
        let (ids, blocks) = body_of(&self.view, &["body"], &cx, |id, node| reuse_if_same(&synced, id, node, &cx))?;
        let wanted: Vec<String> = cx.wanted.borrow().iter().cloned().collect();
        drop(cx);
        self.want_all(wanted);
        self.main = BodySync { ids, synced: blocks.clone() };
        // The state replaces what was synced of everything else: the editor gets the room's styles and headers whole.
        self.styles = StylesSync::default();
        self.styles_seen = None;
        self.last_section = None;
        self.parts.clear();
        self.shown = Shown::default();
        self.shown.styles.replace_all = self.view.get(&["styles"]).is_some();
        self.shown.parts_replace_all = self.view.get(&["parts"]).is_some();
        self.read_extras(None, None)?;
        Ok(blocks)
    }

    /// ★ A REMOTE CHANGE arrived: apply the host's `changes` (and `ack`: the last local batch it applied), put the batches
    /// still in flight back on top, and answer the body to show - `None` when it is the body already shown. Call
    /// [`Shim::take_shown`] after it for the rest.
    pub fn remote_delta(&mut self, changes: &[Change], ack: u64, now_ms: u64) -> Result<Option<Vec<Arc<Block>>>, ShimError> {
        let mut dirty: BTreeSet<String> = BTreeSet::new();
        let mut touched: BTreeSet<String> = BTreeSet::new();
        for c in changes {
            note(&mut dirty, &c.path);
            if let Some(first) = c.path.first() {
                touched.insert(first.clone());
            }
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
                            if let Some(first) = path.first() {
                                touched.insert(first.clone());
                            }
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
        let synced: HashMap<String, Arc<Block>> = self.main.ids.iter().cloned().zip(self.main.synced.iter().cloned()).collect();
        let cx = Shared::new(&self.media);
        let (ids, blocks) = body_of(&self.view, &["body"], &cx, |id, node| if dirty.contains(id) { reuse_if_same(&synced, id, node, &cx) } else { synced.get(id).cloned() })?;
        let wanted: Vec<String> = cx.wanted.borrow().iter().cloned().collect();
        drop(cx);
        self.want_all(wanted);
        let unchanged = ids == self.main.ids && blocks.len() == self.main.synced.len() && blocks.iter().zip(&self.main.synced).all(|(a, b)| Arc::ptr_eq(a, b));
        self.main = BodySync { ids, synced: blocks.clone() };
        self.read_extras(Some(&dirty), Some(&touched))?;
        Ok(if unchanged { None } else { Some(blocks) })
    }

    /// The host's comments arrived (`craft:crdt:state`'s `comments`, or a delta at `["comments"]`): the comments to show are in
    /// [`Shim::take_shown`], and any message that was waiting on a host id is in [`Shim::take_comment_msgs`].
    pub fn remote_comments(&mut self, host: &Value) {
        self.host_comments = host.clone();
        self.apply_host_comments();
    }

    /// A host delta's changes: those below `["comments"]` change the host's comments (the whole value, or one entry or field of
    /// it); the rest are not comments' and are ignored here.
    pub fn remote_comment_changes(&mut self, changes: &[Change]) {
        let mut touched = false;
        for c in changes {
            if c.path.first().map(String::as_str) != Some("comments") {
                continue;
            }
            touched = true;
            if c.path.len() == 1 {
                self.host_comments = c.value.clone().unwrap_or(Value::Null);
                continue;
            }
            if !self.host_comments.is_object() {
                self.host_comments = Value::Object(serde_json::Map::new());
            }
            set_nested(&mut self.host_comments, &c.path[1..], c.value.as_ref());
        }
        if touched {
            self.apply_host_comments();
        }
    }

    fn apply_host_comments(&mut self) {
        let host = self.host_comments.clone();
        let (update, deletes) = self.comments.remote(&host);
        self.shown.comments.upsert.extend(update.upsert);
        self.shown.comments.remove.extend(update.remove);
        self.comment_msgs.extend(deletes);
    }

    /// Everything beside the body that remote changes altered since the last call.
    pub fn take_shown(&mut self) -> Shown {
        let mut shown = std::mem::take(&mut self.shown);
        shown.wanted = self.media.wanted();
        shown
    }

    /// A picture's bytes were found (in the file's saved version): the key to store them under in the document's media, or `None`
    /// when these bytes are not the picture `hash` names (a host that answers with other bytes is not believed).
    #[must_use]
    pub fn media_key_for(&self, hash: &str, bytes: &[u8], name_hint: &str) -> Option<String> {
        MediaIndex::is_address_of(hash, bytes).then(|| media::shared_key(hash, name_hint))
    }

    /// The editor stored pictures: bring the index up to date, so the pictures they were waited for now resolve.
    pub fn media_stored(&mut self, media: &BTreeMap<String, Arc<Vec<u8>>>) {
        self.media.refresh(media);
    }

    /// Whether `a` and `b` hold the same content as the shared tree sees them.
    pub fn same(&self, a: &Block, b: &Block) -> Result<bool, ShimError> {
        let cx = Shared::new(&self.media);
        same_content(a, b, &cx)
    }

    /// Show again, after pictures arrived, the body blocks that referred to them: their content addresses now resolve to keys.
    pub fn reshow(&mut self) -> Result<Vec<Arc<Block>>, ShimError> {
        let cx = Shared::new(&self.media);
        let (ids, blocks) = body_of(&self.view, &["body"], &cx, |_, _| None)?;
        drop(cx);
        self.main = BodySync { ids, synced: blocks.clone() };
        Ok(blocks)
    }
}

/// Set (or remove, with no value) the entry at `path` below the object `at`, making the objects on the way.
fn set_nested(at: &mut Value, path: &[String], value: Option<&Value>) {
    let Some((first, rest)) = path.split_first() else { return };
    if !at.is_object() {
        *at = Value::Object(serde_json::Map::new());
    }
    let Some(obj) = at.as_object_mut() else { return };
    if rest.is_empty() {
        match value {
            Some(v) => {
                obj.insert(first.clone(), v.clone());
            }
            None => {
                obj.remove(first);
            }
        }
    } else {
        set_nested(obj.entry(first.clone()).or_insert_with(|| Value::Object(serde_json::Map::new())), rest, value);
    }
}

/// A change below `blocks.<id>` makes that block dirty.
fn note(dirty: &mut BTreeSet<String>, path: &[String]) {
    if let ("blocks", Some(id)) = (path.first().map(String::as_str).unwrap_or(""), path.get(1)) {
        dirty.insert(id.clone());
    }
}

/// The block already shown under `id`, when the shared node holds the same content.
fn reuse_if_same(synced: &HashMap<String, Arc<Block>>, id: &str, node: &Value, cx: &Shared<'_>) -> Option<Arc<Block>> {
    let old = synced.get(id)?;
    let fresh = blocks::block_from_node(node, cx).ok()?;
    same_content(old, &fresh, cx).ok()?.then(|| old.clone())
}

/// Where comment `wc` is anchored in the body: the paragraph (its shared block id) and the character range from its start
/// object to its end object (the end of the paragraph when the end is elsewhere).
fn anchor_in(main: &BodySync, wc: u32) -> Option<(String, usize, usize)> {
    use wordcraft_doc::InlineObject;
    use wordcraft_doc::para::OBJ;
    for (i, block) in main.synced.iter().enumerate() {
        let Block::Para(p) = &**block else { continue };
        let mut k = 0usize;
        let (mut start, mut end) = (None, None);
        for (at, c) in p.text.chars().enumerate() {
            if c != OBJ {
                continue;
            }
            match p.objects.get(k) {
                Some(InlineObject::CommentStart { id }) if *id == wc => start = Some(at),
                Some(InlineObject::CommentEnd { id }) if *id == wc => end = Some(at + 1),
                _ => {}
            }
            k += 1;
        }
        if let Some(s) = start {
            let len = p.text.chars().count();
            return Some((main.ids.get(i)?.clone(), s, end.filter(|e| *e > s).unwrap_or(len)));
        }
    }
    None
}

#[cfg(test)]
mod tests;
