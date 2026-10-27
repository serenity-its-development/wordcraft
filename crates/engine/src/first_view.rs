//! Where the host's read-only first view was left, as a place in the document (OneOS progressive open, 2026-10-27).
//!
//! The viewer showed the document as a scrolling column; the host says how far down it was (a fraction of the scrollable height) and
//! which text, if any, was selected in it. The editor opens at the same TEXT, not the same pixels (its pages are laid out differently):
//! the selected text is found in the body (the occurrence nearest the scroll position), or, with no selection, the caret goes to the
//! start of the paragraph the first view's top edge was at. Pure: the document in, a selection out.

use wordcraft_doc::{Block, Document, Pos};

/// Where the first view's text is: the body block, and the byte range in it (a collapsed range is a caret).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirstViewPlace {
    pub block: usize,
    pub start: usize,
    pub end: usize,
}

impl FirstViewPlace {
    pub fn anchor(&self) -> Pos {
        Pos::body(self.block, self.start)
    }
    pub fn focus(&self) -> Pos {
        Pos::body(self.block, self.end)
    }
}

/// The place in the body the first view was at: the occurrence of `selection` (its first non-empty line) nearest the scroll position
/// `y_fraction` (0 = top, 1 = bottom of the scrollable height), else the start of the paragraph at that position. Only body
/// paragraphs are places (a table's cells are not); `None` for a document with none.
pub fn locate(doc: &Document, selection: &str, y_fraction: f64) -> Option<FirstViewPlace> {
    let fraction = if y_fraction.is_finite() { y_fraction.clamp(0.0, 1.0) } else { 0.0 };
    // Every body paragraph with the offset of its first byte in the whole body's text.
    let mut paras: Vec<(usize, usize, &str)> = Vec::new();
    let mut total = 0usize;
    for (i, block) in doc.body.iter().enumerate() {
        if let Block::Para(p) = block.as_ref() {
            paras.push((i, total, p.text.as_str()));
            total = total.saturating_add(p.text.len()).saturating_add(1);
        }
    }
    if paras.is_empty() {
        return None;
    }
    let target = (total as f64 * fraction) as usize;
    // The selection's first line is what is looked for (a selection across paragraphs is matched by where it starts).
    let wanted = selection.lines().map(str::trim).find(|l| !l.is_empty());
    if let Some(wanted) = wanted {
        let mut best: Option<(usize, FirstViewPlace)> = None;
        for (block, base, text) in &paras {
            let mut from = 0usize;
            while let Some(found) = text.get(from..).and_then(|rest| rest.find(wanted)) {
                let start = from + found;
                let end = start + wanted.len();
                let distance = (base + start).abs_diff(target);
                if best.as_ref().is_none_or(|(d, _)| distance < *d) {
                    best = Some((distance, FirstViewPlace { block: *block, start, end }));
                }
                // Past this occurrence, on a character boundary.
                from = text[start..].char_indices().nth(1).map_or(text.len(), |(k, _)| start + k);
                if from >= text.len() {
                    break;
                }
            }
        }
        if let Some((_, place)) = best {
            return Some(place);
        }
    }
    // No (found) selection: the caret at the start of the paragraph the top edge was at.
    let at = paras.iter().rev().find(|(_, base, _)| *base <= target).or_else(|| paras.first())?;
    Some(FirstViewPlace { block: at.0, start: 0, end: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use wordcraft_doc::props::CharProps;
    use wordcraft_doc::{Block, Paragraph};

    fn doc(paras: &[&str]) -> Document {
        let mut d = Document::new();
        d.body = paras.iter().map(|t| Arc::new(Block::Para(Paragraph::with_text(t, CharProps::default())))).collect();
        d
    }

    #[test]
    fn a_selection_is_found_at_its_byte_range() {
        let d = doc(&["alpha beta", "gamma delta", "epsilon"]);
        assert_eq!(locate(&d, "delta", 0.5), Some(FirstViewPlace { block: 1, start: 6, end: 11 }));
    }

    #[test]
    fn the_occurrence_nearest_the_scroll_position_wins() {
        let d = doc(&["the cat", "x", "x", "x", "x", "the cat"]);
        assert_eq!(locate(&d, "the cat", 0.0).map(|p| p.block), Some(0));
        assert_eq!(locate(&d, "the cat", 1.0).map(|p| p.block), Some(5));
    }

    #[test]
    fn a_multibyte_selection_lands_on_character_boundaries() {
        let d = doc(&["na\u{ef}ve caf\u{e9} caf\u{e9}"]);
        let p = locate(&d, "caf\u{e9}", 1.0).expect("found");
        assert_eq!((p.start, p.end), ("na\u{ef}ve caf\u{e9} ".len(), "na\u{ef}ve caf\u{e9} caf\u{e9}".len()));
    }

    #[test]
    fn no_selection_puts_the_caret_at_the_paragraph_at_the_top_edge() {
        let d = doc(&["aaaa", "bbbb", "cccc", "dddd"]);
        assert_eq!(locate(&d, "", 0.0), Some(FirstViewPlace { block: 0, start: 0, end: 0 }));
        assert_eq!(locate(&d, "", 0.6), Some(FirstViewPlace { block: 2, start: 0, end: 0 }));
        assert_eq!(locate(&d, "", f64::NAN), Some(FirstViewPlace { block: 0, start: 0, end: 0 }));
    }

    #[test]
    fn a_selection_not_in_the_document_falls_back_to_the_scroll_position() {
        let d = doc(&["aaaa", "bbbb"]);
        assert_eq!(locate(&d, "zzz", 1.0).map(|p| (p.block, p.start, p.end)), Some((1, 0, 0)));
    }
}
