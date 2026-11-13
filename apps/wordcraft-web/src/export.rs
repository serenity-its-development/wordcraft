//! The host's ask for the document's bytes with no save of the person's (`craft:export {session, request}`): OneOS keeps the
//! document for the person by itself (auto-save, save on window close), so its host asks for an EXPORT after a short quiet and
//! when the person leaves the editor. An export is answered by `craft:save {…, checkpoint: request}`: the bytes a Save would
//! write, taken as the document stands. It changes nothing in the editor: no typed field is committed, nothing is flattened,
//! and the document is not marked saved (the host says `craft:saved` when a version of its own lands).
//!
//! This is the judgement, kept apart from the browser so it is tested natively.

/// Why an ask is not taken.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The host has opened no document in this frame yet.
    NothingOpen,
    /// The ask names another open of the file than the one this frame holds (a stale ask, or another window's).
    OtherSession,
    /// The file is open to view: its bytes are the host's own, and nothing here may be saved from.
    ReadOnly,
    /// `request` must be a whole number from 1 (the host's own counter, echoed in the answer).
    BadRequest,
}

/// The largest request id taken: what an integer survives as a JS number.
const MAX_REQUEST: f64 = 9_007_199_254_740_991.0;

/// Judge one `craft:export` against the open this frame holds; the request id to answer with, or why not.
pub fn judge(open_session: &str, read_only: bool, asked_session: &str, request: Option<f64>) -> Result<u64, Refused> {
    if open_session.is_empty() {
        return Err(Refused::NothingOpen);
    }
    if asked_session != open_session {
        return Err(Refused::OtherSession);
    }
    if read_only {
        return Err(Refused::ReadOnly);
    }
    match request {
        Some(r) if r.is_finite() && r >= 1.0 && r <= MAX_REQUEST && r.fract() == 0.0 => Ok(r as u64),
        _ => Err(Refused::BadRequest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ask_for_the_open_this_frame_holds_is_taken_with_its_request_id() {
        assert_eq!(judge("s1", false, "s1", Some(1.0)), Ok(1));
        assert_eq!(judge("s1", false, "s1", Some(42.0)), Ok(42));
    }

    #[test]
    fn an_ask_before_any_open_or_for_another_open_is_not_answered() {
        assert_eq!(judge("", false, "", Some(1.0)), Err(Refused::NothingOpen));
        assert_eq!(judge("", false, "s1", Some(1.0)), Err(Refused::NothingOpen));
        assert_eq!(judge("s2", false, "s1", Some(1.0)), Err(Refused::OtherSession));
    }

    #[test]
    fn a_file_open_to_view_is_never_exported() {
        assert_eq!(judge("s1", true, "s1", Some(1.0)), Err(Refused::ReadOnly));
    }

    #[test]
    fn a_request_that_is_not_a_whole_number_from_one_is_refused() {
        for bad in [None, Some(0.0), Some(-3.0), Some(1.5), Some(f64::NAN), Some(f64::INFINITY), Some(1e300)] {
            assert_eq!(judge("s1", false, "s1", bad), Err(Refused::BadRequest), "{bad:?}");
        }
    }
}
