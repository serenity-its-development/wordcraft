//! Comments, through the comments tier the host already holds (design 12.5, 12.9 Q2).
//!
//! The host owns comments: each is a map in the file's sibling comments room, `{by, name, at, text, resolved, parent?,
//! anchor?}`, stamped with the signed-in party (a frame cannot claim an author), and the host moves a text anchor with a
//! `cmt:<id>` mark on its characters. This shim does not put comments in the document tree. It only translates:
//!
//! * the editor's `Document::comments` (and each comment's text, the paragraphs of its `Comment` part) become
//!   `craft:crdt:comment` messages ([`CommentMsg`]): add, reply, edit, resolve, reopen, delete;
//! * the host's `comments` map (in `craft:crdt:state`, and in a delta whose path is `["comments"]`) becomes comments in the
//!   editor ([`CommentsUpdate`]).
//!
//! The editor's comment number (`CommentStart { id }` in the text, a `u32`) travels in the comment's anchor as `wc`, so every
//! window names a comment by the same number: the anchor objects themselves travel in the body, with the text they bracket.
//! A comment made through the host's own panel carries no `wc`; its number is derived from its id ([`derived_wc`]), the same in
//! every window.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};
use wordcraft_doc::{Comment, Part};

/// One `craft:crdt:comment` message (without the session).
#[derive(Debug, Clone, PartialEq)]
pub enum CommentMsg {
    Add { wc: u32, text: String, anchor: Value },
    Reply { parent: String, wc: u32, text: String },
    Edit { id: String, text: String },
    Resolve { id: String },
    Reopen { id: String },
    Delete { id: String },
}

impl CommentMsg {
    /// The frame message body: `{action, id?, text?, anchor?}`.
    #[must_use]
    pub fn body(&self) -> Value {
        match self {
            CommentMsg::Add { text, anchor, .. } => json!({"action": "add", "text": text, "anchor": anchor}),
            CommentMsg::Reply { parent, wc, text } => json!({"action": "reply", "id": parent, "text": text, "anchor": {"wc": wc}}),
            CommentMsg::Edit { id, text } => json!({"action": "edit", "id": id, "text": text}),
            CommentMsg::Resolve { id } => json!({"action": "resolve", "id": id}),
            CommentMsg::Reopen { id } => json!({"action": "reopen", "id": id}),
            CommentMsg::Delete { id } => json!({"action": "delete", "id": id}),
        }
    }
}

/// A comment the host holds that the editor should show.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteComment {
    pub wc: u32,
    pub author: String,
    pub initials: String,
    /// ISO 8601, UTC.
    pub date: String,
    pub parent: Option<u32>,
    pub resolved: bool,
    pub text: String,
}

/// What the host's comments changed in the editor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommentsUpdate {
    pub upsert: Vec<RemoteComment>,
    pub remove: Vec<u32>,
}

impl CommentsUpdate {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upsert.is_empty() && self.remove.is_empty()
    }
}

/// What both sides agree a comment is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Known {
    text: String,
    resolved: bool,
    parent: Option<u32>,
}

/// The comment number of a comment the host made without one: stable across windows, in the top half of the id space so it
/// cannot meet the numbers an editor counts up from.
#[must_use]
pub fn derived_wc(host_id: &str) -> u32 {
    // FNV-1a.
    let mut h: u32 = 0x811c_9dc5;
    for b in host_id.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    0x8000_0000 | (h & 0x7fff_ffff)
}

/// Initials of a display name ("Ann Lee" is "AL").
#[must_use]
pub fn initials_of(name: &str) -> String {
    let mut out: String = name.split_whitespace().filter_map(|w| w.chars().next()).flat_map(char::to_uppercase).take(3).collect();
    if out.is_empty() {
        out.push('?');
    }
    out
}

/// A millisecond Unix time as ISO 8601 UTC (`2026-10-26T01:02:03Z`).
#[must_use]
pub fn iso_of(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The text of a comment part: its paragraphs, joined by newlines.
#[must_use]
pub fn text_of_part(part: &Part) -> String {
    part.blocks
        .iter()
        .filter_map(|b| match &**b {
            wordcraft_doc::Block::Para(p) => Some(p.text.replace(wordcraft_doc::para::OBJ, "")),
            wordcraft_doc::Block::Table(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}


/// The comment sync state of one window.
#[derive(Debug, Default)]
pub struct CommentSync {
    /// Comments both sides hold, as last agreed.
    known: BTreeMap<u32, Known>,
    /// The host's id of each comment number.
    host_of: HashMap<u32, String>,
    /// Comments deleted here before the host told us their id: deleted once it does.
    deleted_unknown: HashSet<u32>,
    /// Messages waiting on a host id.
    outbox: Vec<CommentMsg>,
}

impl CommentSync {
    /// The messages that take the host's comments to the editor's. `anchor_of(wc)` finds where a new comment is anchored in the
    /// shared body.
    pub fn local(&mut self, comments: &BTreeMap<u32, Comment>, parts: &BTreeMap<u32, Part>, anchor_of: &dyn Fn(u32) -> Option<(String, usize, usize)>) -> Vec<CommentMsg> {
        let mut out = std::mem::take(&mut self.outbox);
        let text_of = |c: &Comment| parts.get(&c.part).map(text_of_part).unwrap_or_default();
        // Deleted here.
        let gone: Vec<u32> = self.known.keys().filter(|wc| !comments.contains_key(wc)).copied().collect();
        for wc in gone {
            self.known.remove(&wc);
            match self.host_of.get(&wc) {
                Some(id) => out.push(CommentMsg::Delete { id: id.clone() }),
                None => {
                    self.deleted_unknown.insert(wc);
                }
            }
        }
        for (wc, c) in comments {
            let want = Known { text: text_of(c), resolved: c.resolved, parent: c.parent };
            match self.known.get(wc) {
                None => {
                    if want.text.trim().is_empty() {
                        // The host takes no empty comment: it is sent when it has words.
                        continue;
                    }
                    let msg = match c.parent.and_then(|p| self.host_of.get(&p)) {
                        Some(parent) => CommentMsg::Reply { parent: parent.clone(), wc: *wc, text: want.text.clone() },
                        None if c.parent.is_some() => continue, // its parent has no host id yet
                        None => {
                            let mut anchor = json!({"wc": wc});
                            if let Some((block, start, end)) = anchor_of(*wc) {
                                anchor = json!({"wc": wc, "path": ["blocks", block, "text"], "start": start, "end": end});
                            }
                            CommentMsg::Add { wc: *wc, text: want.text.clone(), anchor }
                        }
                    };
                    out.push(msg);
                    self.known.insert(*wc, want);
                }
                Some(have) if *have != want => {
                    let Some(id) = self.host_of.get(wc).cloned() else { continue };
                    if have.text != want.text && !want.text.trim().is_empty() {
                        out.push(CommentMsg::Edit { id: id.clone(), text: want.text.clone() });
                    }
                    if have.resolved != want.resolved {
                        out.push(if want.resolved { CommentMsg::Resolve { id } } else { CommentMsg::Reopen { id } });
                    }
                    self.known.insert(*wc, want);
                }
                Some(_) => {}
            }
        }
        out
    }

    /// The host's `comments` map arrived: what the editor should show, and any delete that was waiting on an id.
    pub fn remote(&mut self, host: &Value) -> (CommentsUpdate, Vec<CommentMsg>) {
        let mut update = CommentsUpdate::default();
        let mut deletes = Vec::new();
        let Some(map) = host.as_object() else {
            return (update, deletes);
        };
        // The numbers first, so a reply can name its parent.
        let mut by_host: HashMap<&str, u32> = HashMap::new();
        for (id, c) in map {
            let wc = c.get("anchor").and_then(|a| a.get("wc")).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or_else(|| derived_wc(id));
            by_host.insert(id.as_str(), wc);
        }
        for (id, wc) in &by_host {
            self.host_of.insert(*wc, (*id).to_owned());
        }
        for (id, c) in map {
            let wc = by_host[id.as_str()];
            if self.deleted_unknown.remove(&wc) {
                deletes.push(CommentMsg::Delete { id: id.clone() });
                continue;
            }
            let text = c.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            let resolved = c.get("resolved").and_then(Value::as_bool) == Some(true);
            let parent = c.get("parent").and_then(Value::as_str).and_then(|p| by_host.get(p).copied());
            let want = Known { text: text.clone(), resolved, parent };
            if self.known.get(&wc) == Some(&want) {
                continue;
            }
            self.known.insert(wc, want);
            let name = c.get("name").and_then(Value::as_str).unwrap_or("Someone").to_owned();
            let at = c.get("at").and_then(Value::as_u64).unwrap_or(0);
            update.upsert.push(RemoteComment { wc, initials: initials_of(&name), author: name, date: iso_of(at), parent, resolved, text });
        }
        // Gone from the host.
        let live: HashSet<u32> = by_host.values().copied().collect();
        let removed: Vec<u32> = self.known.keys().filter(|wc| !live.contains(wc) && self.host_of.contains_key(wc)).copied().collect();
        for wc in removed {
            self.known.remove(&wc);
            self.host_of.remove(&wc);
            update.remove.push(wc);
        }
        (update, deletes)
    }

    /// Put messages that could not be sent yet back.
    pub fn hold(&mut self, msgs: Vec<CommentMsg>) {
        self.outbox.extend(msgs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::{Paragraph, PartKind, para_block};

    fn part(text: &str) -> Part {
        Part { kind: PartKind::Comment, blocks: vec![para_block(Paragraph::with_text(text, Default::default()))], ..Default::default() }
    }

    fn comment(part: u32) -> Comment {
        Comment { author: "Ann".into(), part, ..Default::default() }
    }

    fn no_anchor(_: u32) -> Option<(String, usize, usize)> {
        None
    }

    #[test]
    fn iso_and_initials() {
        assert_eq!(iso_of(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_of(1_700_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso_of(1_782_435_723_000), "2026-06-26T01:02:03Z");
        assert_eq!(initials_of("ann lee"), "AL");
        assert_eq!(initials_of(""), "?");
        assert_eq!(derived_wc("c1"), derived_wc("c1"));
        assert!(derived_wc("c1") >= 0x8000_0000);
    }

    #[test]
    fn a_new_comment_is_added_once_and_its_changes_wait_for_the_host_id() {
        let mut sync = CommentSync::default();
        let mut comments = BTreeMap::from([(5u32, comment(1))]);
        let mut parts = BTreeMap::from([(1u32, part("first"))]);
        let anchor = |wc: u32| (wc == 5).then(|| ("b1".to_string(), 2, 6));
        let sent = sync.local(&comments, &parts, &anchor);
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0], CommentMsg::Add { wc: 5, text, anchor } if text == "first" && anchor["path"] == json!(["blocks", "b1", "text"]) && anchor["start"] == 2));
        assert!(sync.local(&comments, &parts, &anchor).is_empty(), "sent once");
        // An edit before the host named the comment is held until it does.
        parts.insert(1, part("first, edited"));
        assert!(sync.local(&comments, &parts, &anchor).is_empty());
        // The host's map names it (its anchor carries our number): nothing to show, and now the edit can go.
        let host = json!({"h1": {"by": "p", "name": "Ann Lee", "at": 1000, "text": "first", "resolved": false, "anchor": {"wc": 5}}});
        let (update, deletes) = sync.remote(&host);
        assert!(update.is_empty() && deletes.is_empty(), "our own comment coming back changes nothing: {update:?}");
        let sent = sync.local(&comments, &parts, &anchor);
        assert_eq!(sent, vec![CommentMsg::Edit { id: "h1".into(), text: "first, edited".into() }]);
        comments.get_mut(&5).unwrap().resolved = true;
        assert_eq!(sync.local(&comments, &parts, &anchor), vec![CommentMsg::Resolve { id: "h1".into() }]);
        comments.remove(&5);
        assert_eq!(sync.local(&comments, &parts, &anchor), vec![CommentMsg::Delete { id: "h1".into() }]);
    }

    #[test]
    fn a_reply_names_its_parent_and_waits_for_it() {
        let mut sync = CommentSync::default();
        let comments = BTreeMap::from([(1u32, comment(1)), (2u32, Comment { parent: Some(1), ..comment(2) })]);
        let parts = BTreeMap::from([(1u32, part("q")), (2u32, part("a"))]);
        let sent = sync.local(&comments, &parts, &no_anchor);
        assert_eq!(sent.len(), 1, "the reply waits for its parent's id: {sent:?}");
        sync.remote(&json!({"hp": {"name": "Ann", "at": 1, "text": "q", "anchor": {"wc": 1}}}));
        let sent = sync.local(&comments, &parts, &no_anchor);
        assert_eq!(sent, vec![CommentMsg::Reply { parent: "hp".into(), wc: 2, text: "a".into() }]);
    }

    #[test]
    fn the_hosts_comments_reach_the_editor_and_leave_it() {
        let mut sync = CommentSync::default();
        let host = json!({
            "h1": {"name": "Ben Ray", "at": 1_700_000_000_000u64, "text": "look here", "resolved": false, "anchor": {"wc": 9}},
            "h2": {"name": "Cy", "at": 1_700_000_001_000u64, "text": "agreed", "resolved": true, "parent": "h1"},
        });
        let (update, _) = sync.remote(&host);
        assert_eq!(update.upsert.len(), 2);
        let h1 = update.upsert.iter().find(|c| c.wc == 9).unwrap();
        assert_eq!((h1.author.as_str(), h1.initials.as_str(), h1.text.as_str(), h1.parent), ("Ben Ray", "BR", "look here", None));
        let h2 = update.upsert.iter().find(|c| c.wc == derived_wc("h2")).unwrap();
        assert_eq!((h2.parent, h2.resolved), (Some(9), true));
        assert!(sync.remote(&host).0.is_empty(), "the same map again shows nothing new");
        // An edit and a delete.
        let host = json!({"h1": {"name": "Ben Ray", "at": 1_700_000_000_000u64, "text": "look there", "resolved": false, "anchor": {"wc": 9}}});
        let (update, _) = sync.remote(&host);
        assert_eq!(update.upsert.len(), 1);
        assert_eq!(update.upsert[0].text, "look there");
        assert_eq!(update.remove, vec![derived_wc("h2")]);
        // Our delete that raced the host's id goes out once it is known.
        let mut sync = CommentSync::default();
        let comments = BTreeMap::from([(7u32, comment(1))]);
        let parts = BTreeMap::from([(1u32, part("x"))]);
        sync.local(&comments, &parts, &no_anchor);
        assert_eq!(sync.local(&BTreeMap::new(), &parts, &no_anchor), vec![]);
        let (_, deletes) = sync.remote(&json!({"h7": {"name": "Me", "at": 1, "text": "x", "anchor": {"wc": 7}}}));
        assert_eq!(deletes, vec![CommentMsg::Delete { id: "h7".into() }]);
    }
}
