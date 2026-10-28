//! Live editing for the host bridge: the frame side of the host's common collaboration adapter (design section 12). The
//! shim itself is `wordcraft-collab` (pure, tested natively and against the host's real engine); this module is its
//! wiring: it turns the editor session's edits into `craft:crdt:ops`, and the host's `state` / `delta` / `seed` / `export`
//! into the document the editor shows.
//!
//! What goes both ways: the body, styles, headers and footers, the last section, comments (through the host's comments
//! tier, as `craft:crdt:comment`), pictures (as content addresses; the bytes come from the file's saved version) and the
//! carets of the people in the room (`craft:crdt:cursor` out, `craft:crdt:peer` in).
//!
//! Order each frame: the host's messages that wait for the file to finish opening are held; before a remote change is
//! shown, the person's own edits since the last frame are sent (so none is overwritten); then the remote change is shown;
//! then any edit this frame made is sent. Undo and Redo are the room's (`craft:crdt:undo` / `redo`): the session's own
//! snapshots would also take back what others typed.

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Value, json};
use wordcraft_collab::media::sha256_hex;
use wordcraft_collab::{Batch, Extras, Shim, changes_of};
use wordcraft_engine::RemoteCursor;
use wordcraft_ui_egui::WordApp;

use crate::host::{Bridge, Event};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Not a live document (the host opened it without `collab`, or live editing ended).
    Off,
    /// Opened with `collab`: waiting for the host to seed the room or send its state.
    Joining,
    /// In the room.
    Live,
}

/// Another person in the room: their name, colour and last cursor body (resolved against the document every frame).
struct Peer {
    name: String,
    color: [u8; 3],
    cursor: Value,
}

/// My caret goes out when it moves (at most this often) and again this often while it stays, so the others' copy of it does not
/// expire (the host drops a cursor 6 s after its last frame).
const CURSOR_MIN_MS: u64 = 120;
const CURSOR_REPEAT_MS: u64 = 3_000;
/// Asking the host for the pictures other people inserted: first after this long, then less often while they are still missing.
const MEDIA_FIRST_MS: u64 = 400;
const MEDIA_MAX_MS: u64 = 120_000;
/// After inserting a picture the host is asked to land a version; asked again this often until one lands.
const PUBLISH_REPEAT_MS: u64 = 20_000;

pub struct Collab {
    bridge: Bridge,
    shim: Shim,
    phase: Phase,
    /// The document id when the host's `craft:open` arrived: the file has loaded once the session holds another.
    baseline: u64,
    loaded: bool,
    /// The document and revision last handed to (or taken from) the shim.
    doc_id: u64,
    rev: u64,
    held: VecDeque<Event>,
    peers: BTreeMap<String, Peer>,
    last_cursor: Option<Value>,
    last_cursor_ms: u64,
    /// Pictures: when to ask next, how long after that, and the version of the file already searched for them.
    media_next_ms: u64,
    media_backoff_ms: u64,
    media_version: String,
    /// A picture was inserted and no version has landed since (`saved` was this many when it was asked).
    publish_at: Option<(u64, u64)>,
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// Random bits for block ids, from the browser's crypto.
fn random_seed() -> u64 {
    let mut buf = [0u8; 8];
    if let Some(crypto) = web_sys::window().and_then(|w| w.crypto().ok()) {
        let _ = crypto.get_random_values_with_u8_array(&mut buf);
    }
    u64::from_le_bytes(buf) ^ (js_sys::Math::random() * 9.007_199_254_740_992e15) as u64
}

/// `#rrggbb` as bytes.
fn rgb_of(hex: &str) -> [u8; 3] {
    let h = hex.trim_start_matches('#');
    let byte = |i: usize| h.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(0x7c);
    [byte(0), byte(2), byte(4)]
}

impl Collab {
    pub fn new(bridge: Bridge) -> Self {
        Collab {
            bridge,
            shim: Shim::new(random_seed()),
            phase: Phase::Off,
            baseline: 0,
            loaded: false,
            doc_id: 0,
            rev: 0,
            held: VecDeque::new(),
            peers: BTreeMap::new(),
            last_cursor: None,
            last_cursor_ms: 0,
            media_next_ms: 0,
            media_backoff_ms: MEDIA_FIRST_MS,
            media_version: String::new(),
            publish_at: None,
        }
    }

    fn off(&mut self, app: &mut WordApp, why: &str) {
        self.phase = Phase::Off;
        app.session.collab_undo = false;
        app.session.live_comment_salt = 0;
        app.session.remote_cursors.clear();
        app.status(format!("Live editing is off: {why}"));
    }

    fn go_live(&mut self, app: &mut WordApp) {
        self.phase = Phase::Live;
        self.doc_id = app.session.document_id();
        self.rev = app.session.rev();
        app.session.collab_undo = true;
        // This window's new comment numbers must differ from every other window's.
        app.session.live_comment_salt = (random_seed() as u32) | 1;
    }

    fn post_batches(&self, batches: &[Batch]) {
        for b in batches {
            let mut body = json!({"ops": b.ops, "seq": b.seq, "undo": if b.join { "join" } else { "step" }, "label": b.label});
            if b.seed {
                body["seed"] = json!(true);
            }
            self.bridge.post_crdt("ops", &body);
        }
    }

    /// Send the comment messages the shim holds.
    fn post_comment_msgs(&mut self) {
        for m in self.shim.take_comment_msgs() {
            self.bridge.post_crdt("comment", &m.body());
        }
    }

    /// A batch that put a picture reference in the room: the other windows can only get its bytes from a saved version.
    fn note_pictures(&mut self, batches: &[Batch]) {
        let carries = batches
            .iter()
            .flat_map(|b| &b.ops)
            .any(|op| matches!(op.get("key").and_then(Value::as_str), Some("obj" | "v")) && op.to_string().contains("\"cas-"));
        if carries && self.publish_at.is_none() {
            self.publish_at = Some((0, self.bridge.saved_count()));
        }
    }

    /// Send what the person changed since the last frame.
    fn flush_local(&mut self, app: &mut WordApp) {
        if self.phase != Phase::Live {
            return;
        }
        if app.session.document_id() != self.doc_id {
            self.off(app, "another document was opened in this window");
            return;
        }
        if app.session.rev() == self.rev {
            return;
        }
        self.rev = app.session.rev();
        if self.bridge.opened().read_only {
            return;
        }
        let doc = &app.session.doc;
        match self.shim.local_all(&doc.body, &Extras::of(doc), now_ms()) {
            Ok(batches) => {
                self.post_batches(&batches);
                self.note_pictures(&batches);
                self.post_comment_msgs();
            }
            Err(e) => self.off(app, &e.to_string()),
        }
    }

    fn show(&mut self, app: &mut WordApp, body: Vec<std::sync::Arc<wordcraft_doc::Block>>) {
        app.session.replace_body_remote(body);
        self.rev = app.session.rev();
    }

    /// Take in what remote changes altered beyond the body.
    fn apply_shown(&mut self, app: &mut WordApp) {
        let shown = self.shim.take_shown();
        if shown.changes_document() {
            app.session.change_remote(|doc| shown.apply_to(doc));
            self.rev = app.session.rev();
        }
        self.post_comment_msgs();
    }

    fn handle(&mut self, app: &mut WordApp, kind: &str, body: &Value) {
        match kind {
            "state" => {
                let read_only = body.get("readOnly").and_then(Value::as_bool) == Some(true);
                if read_only {
                    app.session.doc.settings.protection = Some("readOnly".to_owned());
                }
                let doc = body.get("state").and_then(|s| s.get("doc")).cloned().unwrap_or(Value::Null);
                // An empty room has nothing to show: the host asks for the seed.
                if doc.as_object().is_none_or(|o| o.get("body").is_none()) {
                    return;
                }
                self.flush_local(app);
                match self.shim.remote_state(&doc) {
                    Ok(blocks) => {
                        self.show(app, blocks);
                        if let Some(comments) = body.get("state").and_then(|s| s.get("comments")) {
                            self.shim.remote_comments(comments);
                        }
                        self.apply_shown(app);
                        self.go_live(app);
                    }
                    Err(e) => self.off(app, &e.to_string()),
                }
            }
            "seed" => {
                let doc = &app.session.doc;
                match self.shim.seed_all(&doc.body, &Extras::of(doc), now_ms()) {
                    Ok(batches) => {
                        // The opened file already carries its pictures: nothing to publish for a seed.
                        self.post_batches(&batches);
                        self.post_comment_msgs();
                        self.go_live(app);
                    }
                    Err(e) => self.off(app, &e.to_string()),
                }
            }
            "delta" => {
                if self.phase != Phase::Live {
                    return;
                }
                self.flush_local(app);
                if self.phase != Phase::Live {
                    return;
                }
                let ack = body.get("ack").and_then(Value::as_u64).unwrap_or(0);
                // The shim reads what is below `doc`, and the host's comments as one value at `comments`.
                let all = changes_of(body.get("changes").unwrap_or(&Value::Null));
                self.shim.remote_comment_changes(&all);
                let doc_changes: Vec<_> = all
                    .into_iter()
                    .filter_map(|mut c| {
                        (c.path.first().map(String::as_str) == Some("doc")).then(|| {
                            c.path.remove(0);
                            c
                        })
                    })
                    .collect();
                match self.shim.remote_delta(&doc_changes, ack, now_ms()) {
                    Ok(Some(blocks)) => self.show(app, blocks),
                    Ok(None) => {}
                    Err(e) => {
                        self.off(app, &e.to_string());
                        return;
                    }
                }
                self.apply_shown(app);
            }
            "export" => {
                self.flush_local(app);
                let request = body.get("request").and_then(Value::as_u64);
                let opened = self.bridge.opened();
                let opts = wordcraft_engine::cmd::file::export_options(&app.session, &json!({}));
                match wordcraft_engine::io::save_bytes_with(&opened.name, &app.session.doc, None, &opts) {
                    Ok(bytes) => {
                        if let Err(e) = self.bridge.post_save(&opened.name, &bytes, false, request) {
                            app.status(format!("The checkpoint was not sent: {e}"));
                        }
                    }
                    Err(e) => app.status(format!("The checkpoint could not be made: {e}")),
                }
            }
            "peer" => {
                let Some(party) = body.get("party").and_then(Value::as_str) else { return };
                match body.get("cursor") {
                    Some(c) if !c.is_null() => {
                        let name = body.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("Someone").to_owned();
                        let color = rgb_of(body.get("color").and_then(Value::as_str).unwrap_or("#7c3aed"));
                        self.peers.insert(party.to_owned(), Peer { name, color, cursor: c.clone() });
                    }
                    _ => {
                        self.peers.remove(party);
                    }
                }
            }
            "unplaced" => app.status("Some of your offline edits could not be placed in the live document; they are shown beside it."),
            _ => {}
        }
    }

    /// The host sent the file's latest saved version: the pictures other people inserted are in it.
    fn media_file(&mut self, app: &mut WordApp, name: &str, version_id: &str, bytes: Option<Vec<u8>>) {
        if !version_id.is_empty() {
            self.media_version = version_id.to_owned();
        }
        let Some(bytes) = bytes else { return };
        let wanted: std::collections::BTreeSet<String> = self.shim.media().wanted().into_iter().collect();
        if wanted.is_empty() {
            return;
        }
        let Ok(found) = wordcraft_engine::io::pictures_in_file(name, &bytes) else { return };
        let mut stored = Vec::new();
        for (key, data) in found {
            let hash = sha256_hex(&data);
            if wanted.contains(&hash)
                && let Some(local_key) = self.shim.media_key_for(&hash, &data, &key)
            {
                stored.push((local_key, data));
            }
        }
        if stored.is_empty() {
            return;
        }
        app.session.change_remote(|doc| {
            for (key, data) in stored {
                doc.media.insert(key, data);
            }
        });
        self.rev = app.session.rev();
        self.shim.media_stored(&app.session.doc.media);
        // What is still missing is asked for again soon: the picture may not be in a saved version yet.
        self.media_backoff_ms = MEDIA_FIRST_MS;
        self.media_next_ms = now_ms() + MEDIA_FIRST_MS;
    }

    /// Once per frame BEFORE the app's own pass: a `craft:open` is noted against the document the session holds right now
    /// (the app opens the file from the inbox during its pass, so the open has happened once the session holds another).
    pub fn pre(&mut self, app: &WordApp) {
        for ev in self.bridge.take_events() {
            match ev {
                Event::Opened(o) => {
                    self.phase = if o.collab { Phase::Joining } else { Phase::Off };
                    self.baseline = app.session.document_id();
                    self.loaded = false;
                    // Messages for the file that was open before are not for this one.
                    self.held.clear();
                    self.peers.clear();
                    self.last_cursor = None;
                    self.media_version.clear();
                    self.publish_at = None;
                }
                other => self.held.push_back(other),
            }
        }
    }

    /// The assistant's runs the host relayed (`craft:command`): once the document is drawn, each is judged and run as ONE undo step
    /// through the window (so the live room hears it as any local edit), or answered with the reason it did not run.
    fn run_host_commands(&mut self, app: &mut WordApp) {
        if app.session.document_id() == self.baseline {
            return;
        }
        let session = self.bridge.opened().session;
        for run in self.bridge.take_runs() {
            let outcome = if run.method == "command.batch" {
                app.run_command_batch(&run.params)
            } else {
                Err(format!("unknown-method: '{}' is not a command this editor answers", run.method))
            };
            self.bridge.post_result(&session, run.id, outcome);
        }
    }

    /// Once per frame AFTER the app's own pass.
    pub fn post(&mut self, app: &mut WordApp) {
        if !self.loaded && self.phase != Phase::Off && app.session.document_id() != self.baseline {
            self.loaded = true;
            self.doc_id = app.session.document_id();
            self.rev = app.session.rev();
            app.session.collab_undo = false;
        }
        while self.loaded || matches!(self.held.front(), Some(Event::Refused { .. })) {
            match self.held.pop_front() {
                Some(Event::Refused { code, reason }) => app.status(if code.is_empty() { reason } else { format!("{reason} ({code})") }),
                Some(Event::Crdt { kind, body }) => self.handle(app, &kind, &body),
                Some(Event::MediaFile { name, version_id, bytes }) => self.media_file(app, &name, &version_id, bytes),
                Some(Event::Opened(_)) | None => break,
            }
        }
        self.run_host_commands(app);
        if self.loaded {
            self.flush_local(app);
        }
        for undo in std::mem::take(&mut app.session.collab_undo_requests) {
            self.bridge.post_crdt(if undo { "undo" } else { "redo" }, &json!({}));
        }
        if self.phase == Phase::Live {
            self.share_cursor(app);
            self.show_peers(app);
            self.ask_for_pictures();
        }
    }

    /// Say where my caret is: when it moves, and now and then while it stays.
    fn share_cursor(&mut self, app: &WordApp) {
        let now = now_ms();
        let sel = &app.session.sel;
        let Some(body) = self.shim.cursor_for(&sel.anchor, &sel.focus) else { return };
        let moved = self.last_cursor.as_ref() != Some(&body);
        let due = if moved {
            now.saturating_sub(self.last_cursor_ms) >= CURSOR_MIN_MS
        } else {
            now.saturating_sub(self.last_cursor_ms) >= CURSOR_REPEAT_MS
        };
        if !due {
            return;
        }
        self.bridge.post_crdt("cursor", &json!({"cursor": body}));
        self.last_cursor = Some(body);
        self.last_cursor_ms = now;
    }

    /// The other people's carets, resolved against the document as it is now.
    fn show_peers(&mut self, app: &mut WordApp) {
        let cursors: Vec<RemoteCursor> = self
            .peers
            .values()
            .filter_map(|p| {
                self.shim.peer_cursor(&p.cursor).map(|c| RemoteCursor { name: p.name.clone(), color: p.color, anchor: c.anchor, focus: c.focus })
            })
            .collect();
        if cursors != app.session.remote_cursors {
            app.session.remote_cursors = cursors;
        }
    }

    /// Pictures the shared document uses and this window has no bytes for, and the picture this window inserted.
    fn ask_for_pictures(&mut self) {
        let now = now_ms();
        let wanted = self.shim.media().wanted();
        if wanted.is_empty() {
            self.media_backoff_ms = MEDIA_FIRST_MS;
            self.media_next_ms = 0;
        } else {
            if self.media_next_ms == 0 {
                self.media_next_ms = now + MEDIA_FIRST_MS;
            }
            if now >= self.media_next_ms {
                self.bridge.post_media_need(&wanted, &self.media_version);
                self.media_next_ms = now + self.media_backoff_ms;
                self.media_backoff_ms = (self.media_backoff_ms * 2).min(MEDIA_MAX_MS);
            }
        }
        if let Some((asked_ms, saved_then)) = self.publish_at {
            if self.bridge.saved_count() > saved_then {
                self.publish_at = None;
            } else if now.saturating_sub(asked_ms) >= PUBLISH_REPEAT_MS {
                self.bridge.post_media_publish();
                self.publish_at = Some((now, saved_then));
            }
        }
    }
}
