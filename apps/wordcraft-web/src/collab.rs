//! Live editing for the host bridge: the frame side of the host's common collaboration adapter (design section 12). The
//! shim itself is `wordcraft-collab` (pure, tested natively and against the host's real engine); this module is its
//! wiring: it turns the editor session's edits into `craft:crdt:ops`, and the host's `state` / `delta` / `seed` / `export`
//! into the body the editor shows.
//!
//! Order each frame: the host's messages that wait for the file to finish opening are held; before a remote change is
//! shown, the person's own edits since the last frame are sent (so none is overwritten); then the remote change is shown;
//! then any edit this frame made is sent. Undo and Redo are the room's (`craft:crdt:undo` / `redo`): the session's own
//! snapshots would also take back what others typed.

use std::collections::VecDeque;

use serde_json::{Value, json};
use wordcraft_collab::{Batch, Shim, changes_of};
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

impl Collab {
    pub fn new(bridge: Bridge) -> Self {
        Collab { bridge, shim: Shim::new(random_seed()), phase: Phase::Off, baseline: 0, loaded: false, doc_id: 0, rev: 0, held: VecDeque::new() }
    }

    fn off(&mut self, app: &mut WordApp, why: &str) {
        self.phase = Phase::Off;
        app.session.collab_undo = false;
        app.status(format!("Live editing is off: {why}"));
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
        match self.shim.local(&app.session.doc.body, now_ms()) {
            Ok(batches) => self.post_batches(&batches),
            Err(e) => self.off(app, &e.to_string()),
        }
    }

    fn show(&mut self, app: &mut WordApp, body: Vec<std::sync::Arc<wordcraft_doc::Block>>) {
        app.session.replace_body_remote(body);
        self.rev = app.session.rev();
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
                        self.phase = Phase::Live;
                        self.doc_id = app.session.document_id();
                        app.session.collab_undo = true;
                    }
                    Err(e) => self.off(app, &e.to_string()),
                }
            }
            "seed" => {
                match self.shim.seed(&app.session.doc.body, now_ms()) {
                    Ok(batches) => {
                        self.post_batches(&batches);
                        self.phase = Phase::Live;
                        self.doc_id = app.session.document_id();
                        self.rev = app.session.rev();
                        app.session.collab_undo = true;
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
                let changes = changes_of(body.get("changes").unwrap_or(&Value::Null));
                // The shim reads only what is below `doc`.
                let changes: Vec<_> = changes.into_iter().filter_map(|mut c| (c.path.first().map(String::as_str) == Some("doc")).then(|| { c.path.remove(0); c })).collect();
                match self.shim.remote_delta(&changes, ack, now_ms()) {
                    Ok(Some(blocks)) => self.show(app, blocks),
                    Ok(None) => {}
                    Err(e) => self.off(app, &e.to_string()),
                }
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
            "unplaced" => app.status("Some of your offline edits could not be placed in the live document; they are shown beside it."),
            // Presence, the other people's cursors: this editor does not draw them yet.
            _ => {}
        }
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
                }
                other => self.held.push_back(other),
            }
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
                Some(Event::Opened(_)) | None => break,
            }
        }
        if self.loaded {
            self.flush_local(app);
        }
        for undo in std::mem::take(&mut app.session.collab_undo_requests) {
            self.bridge.post_crdt(if undo { "undo" } else { "redo" }, &json!({}));
        }
    }
}
