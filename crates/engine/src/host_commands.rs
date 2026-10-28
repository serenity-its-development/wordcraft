//! The assistant's command surface for the OneOS host bridge (`craft:command {method: "command.batch"}`).
//!
//! The host relays an allow-listed run: `{label, steps: [{name, args?}]}`. The whole run is judged BEFORE anything runs (an id
//! outside the list, an argument that names a file or an address, a private `__` argument, or a step reference to a step that
//! has not run refuses the run), every step runs through the same registry the ribbon, the control channel and the MCP server
//! use (so a step the document's state forbids is refused exactly as it would be for the person), and the run is ONE undo
//! step named by `label`: a step that fails takes back every step before it, so a run is all or nothing. There is no
//! file-path or dialog command in the list: files move only through the host.
//!
//! A later step may name what an earlier step made: `{"$step": n}` stands for the `id` of step n's result,
//! `{"$step": n, "field": "..."}` for another field of it.
//!
//! This list is the one declared in the host's `platform-craft` registry as `write` commands (pinned equal there).

use serde_json::{Value, json};

use crate::Session;

/// Registry commands the assistant may run. Each acts at once on the document (no dialog, no file picker, no pointer gesture),
/// and none is Undo or Redo: undoing a run is the person's.
pub const ASSISTANT_COMMANDS: &[&str] = &[
    "text.insert",
    "text.newParagraph",
    "text.lineBreak",
    "text.pageBreak",
    "text.columnBreak",
    "text.tab",
    "text.backTab",
    "text.insertTab",
    "text.backspace",
    "text.delete",
    "text.deleteWordBack",
    "text.deleteWordForward",
    "text.nbsp",
    "text.nbHyphen",
    "text.optionalHyphen",
    "text.toggleUnicode",
    "text.typeCode",
    "text.emDash",
    "text.copyright",
    "text.registered",
    "text.trademark",
    "text.ellipsis",
    "caret.left",
    "caret.right",
    "caret.up",
    "caret.down",
    "caret.wordLeft",
    "caret.wordRight",
    "caret.home",
    "caret.end",
    "caret.paraUp",
    "caret.paraDown",
    "caret.docStart",
    "caret.docEnd",
    "caret.pageUp",
    "caret.pageDown",
    "caret.set",
    "select.all",
    "select.range",
    "select.word",
    "select.sentence",
    "select.paragraph",
    "select.line",
    "select.text",
    "select.collapse",
    "select.column",
    "edit.find",
    "edit.advancedFind",
    "edit.findNext",
    "edit.findPrevious",
    "edit.replace",
    "edit.findReplace",
    "edit.replaceAll",
    "edit.goto",
    "edit.goBack",
    "edit.copyFormat",
    "edit.pasteFormat",
    "format.bold",
    "format.italic",
    "format.underline",
    "format.doubleUnderline",
    "format.wordUnderline",
    "format.strikethrough",
    "format.border",
    "format.doubleStrikethrough",
    "format.subscript",
    "format.superscript",
    "format.allCaps",
    "format.smallCaps",
    "format.hidden",
    "format.outline",
    "format.shadow",
    "format.emboss",
    "format.engrave",
    "format.font",
    "format.size",
    "format.growFont",
    "format.shrinkFont",
    "format.growFont1",
    "format.shrinkFont1",
    "format.color",
    "format.underlineColor",
    "format.highlight",
    "format.shading",
    "format.changeCase",
    "format.clear",
    "format.resetChar",
    "format.spacing",
    "format.scale",
    "format.position",
    "format.charStyle",
    "format.set",
    "format.state",
    "font.list",
    "para.alignLeft",
    "para.alignCenter",
    "para.alignRight",
    "para.justify",
    "para.distribute",
    "para.align",
    "para.rtl",
    "para.ltr",
    "para.indent",
    "para.outdent",
    "para.hangingIndent",
    "para.removeHanging",
    "para.indents",
    "para.lineSpacing",
    "para.single",
    "para.double",
    "para.oneAndHalf",
    "para.spacing",
    "para.addSpaceBefore",
    "para.removeSpaceAfter",
    "para.reset",
    "para.style",
    "para.normal",
    "para.heading1",
    "para.heading2",
    "para.heading3",
    "para.bullets",
    "para.numbering",
    "para.multilevel",
    "para.listLevel",
    "para.restartNumbering",
    "para.moveUp",
    "para.moveDown",
    "para.promote",
    "para.demote",
    "para.shading",
    "para.borders",
    "para.sort",
    "para.keepNext",
    "para.keepLines",
    "para.pageBreakBefore",
    "para.widowControl",
    "para.asianTypography",
    "para.tabs",
    "para.outlineLevel",
    "para.set",
    "styles.create",
    "styles.modify",
    "styles.updateToMatch",
    "styles.delete",
    "styles.manage",
    "styles.setVisibility",
    "styles.list",
    "styles.pane",
    "styles.apply",
    "styles.addToGallery",
    "list.define",
    "list.get",
    "list.multilevelPreset",
    "styles.inspector",
    "styles.inspect",
    "styles.inspectorClear",
    "view.marks",
    "view.printLayout",
    "view.webLayout",
    "view.draft",
    "view.outline",
    "view.readMode",
    "view.readColumnWidth",
    "view.readTextSize",
    "view.readLineSpacing",
    "view.readPageColor",
    "view.hideWhiteSpace",
    "view.focus",
    "view.ruler",
    "view.gridlines",
    "table.viewGridlines",
    "view.navigationPane",
    "view.zoom",
    "view.zoom100",
    "view.zoomIn",
    "view.zoomOut",
    "view.onePage",
    "view.multiplePages",
    "view.pageWidth",
    "view.stylesPane",
    "view.commentsPane",
    "view.accessibilityPane",
    "view.altTextPane",
    "view.formatShapePane",
    "view.split",
    "view.state",
    "view.numeral",
    "outline.promoteToHeading1",
    "outline.promote",
    "outline.demote",
    "outline.demoteToBody",
    "outline.level",
    "outline.moveUp",
    "outline.moveDown",
    "outline.expand",
    "outline.collapse",
    "outline.toggle",
    "outline.selectSubtree",
    "outline.showLevel",
    "outline.showLevel1",
    "outline.showLevel2",
    "outline.showLevel3",
    "outline.showLevel4",
    "outline.showLevel5",
    "outline.showLevel6",
    "outline.showLevel7",
    "outline.showLevel8",
    "outline.showLevel9",
    "outline.showAll",
    "outline.firstLineOnly",
    "outline.showFormatting",
    "outline.close",
    "outline.state",
    "insert.pageBreak",
    "insert.blankPage",
    "insert.coverPage",
    "insert.table",
    "insert.icon",
    "insert.shape",
    "insert.textBox",
    "insert.link",
    "link.targets",
    "insert.removeLink",
    "insert.bookmark",
    "insert.deleteBookmark",
    "insert.header",
    "insert.footer",
    "insert.pageNumber",
    "insert.editHeader",
    "insert.editFooter",
    "insert.createHeader",
    "insert.createFooter",
    "insert.closeHeader",
    "insert.removeEmptyHeaders",
    "insert.removeHeader",
    "insert.removeFooter",
    "insert.dateTime",
    "insert.symbol",
    "insert.field",
    "insert.dateField",
    "insert.timeField",
    "insert.pageField",
    "insert.dropCap",
    "insert.horizontalLine",
    "insert.equation",
    "equation.edit",
    "equation.exit",
    "equation.type",
    "equation.insertStructure",
    "equation.insertSymbol",
    "equation.move",
    "equation.backspace",
    "equation.delete",
    "equation.enter",
    "equation.convert",
    "equation.inputFormat",
    "equation.normalText",
    "equation.display",
    "equation.justify",
    "equation.number",
    "equation.set",
    "equation.get",
    "equation.gallery",
    "equation.structureActions",
    "equation.structure",
    "layout.margins",
    "layout.orientation",
    "layout.size",
    "layout.columns",
    "layout.break",
    "layout.lineNumbers",
    "layout.hyphenation",
    "layout.manualHyphenation",
    "layout.pageSetup",
    "layout.verticalAlign",
    "layout.sectionRtl",
    "layout.textDirection",
    "layout.differentFirstPage",
    "layout.differentOddEven",
    "layout.pageNumberFormat",
    "layout.documentGrid",
    "layout.section",
    "table.insertRowAbove",
    "table.insertRowBelow",
    "table.insertColumnLeft",
    "table.insertColumnRight",
    "table.deleteRow",
    "table.deleteColumn",
    "table.deleteTable",
    "table.deleteCells",
    "table.merge",
    "table.split",
    "table.splitTable",
    "table.style",
    "table.look",
    "table.shading",
    "table.borders",
    "table.cellAlign",
    "table.textDirection",
    "table.autofit",
    "table.distributeColumns",
    "table.distributeRows",
    "table.columnWidth",
    "table.rowHeight",
    "table.repeatHeader",
    "table.altText",
    "table.sort",
    "table.toText",
    "table.formula",
    "table.selectTable",
    "table.selectRow",
    "table.selectColumn",
    "table.selectCell",
    "table.rtl",
    "table.properties",
    "table.fromText",
    "table.quick",
    "table.newStyle",
    "table.modifyStyle",
    "table.deleteStyle",
    "table.fitColumn",
    "review.newComment",
    "review.reply",
    "review.editComment",
    "review.deleteComment",
    "review.resolveComment",
    "review.nextComment",
    "review.previousComment",
    "review.comments",
    "review.trackChanges",
    "review.acceptAll",
    "review.rejectAll",
    "review.accept",
    "review.reject",
    "review.nextChange",
    "review.previousChange",
    "review.markup",
    "review.trackingOptions",
    "review.wordCount",
    "review.changes",
    "review.spelling",
    "review.issues",
    "review.suggestions",
    "review.addToDictionary",
    "review.ignoreAll",
    "review.applySuggestion",
    "review.proofing",
    "review.thesaurus",
    "document.inspect",
    "document.text",
    "document.paragraph",
    "document.selection",
    "document.layout",
    "document.setText",
    "text.overtype",
    "design.theme",
    "design.themeFonts",
    "design.themeColors",
    "design.styleSet",
    "design.paragraphSpacing",
    "design.watermark",
    "design.pageColor",
    "design.pageBorders",
    "design.setDefault",
    "design.themes",
    "references.toc",
    "references.updateToc",
    "references.removeToc",
    "references.addText",
    "references.footnote",
    "references.endnote",
    "references.convertNotes",
    "references.nextFootnote",
    "references.notes",
    "references.caption",
    "references.updateFields",
    "view.fieldCodes",
    "fields.toggleCode",
    "fields.setCode",
    "fields.unlink",
    "fields.lock",
    "fields.unlock",
    "fields.next",
    "fields.previous",
    "fields.selected",
    "mailings.start",
    "mailings.insertField",
    "mailings.addressBlock",
    "mailings.greetingLine",
    "mailings.rules",
    "mailings.matchFields",
    "mailings.highlightFields",
    "mailings.preview",
    "mailings.next",
    "mailings.previous",
    "mailings.first",
    "mailings.last",
    "mailings.goToRecord",
    "mailings.findRecipient",
    "mailings.checkErrors",
    "references.citation",
    "references.citationStyle",
    "references.bibliography",
    "references.tableOfFigures",
    "references.updateFigures",
    "references.markCitation",
    "references.tableOfAuthorities",
    "references.updateAuthorities",
    "insert.crossReference",
    "references.noteOptions",
    "references.researcher",
    "references.markEntry",
    "references.index",
    "references.updateIndex",
    "picture.size",
    "picture.crop",
    "picture.altText",
    "object.altText",
    "picture.corrections",
    "picture.color",
    "picture.iconColor",
    "picture.effects",
    "picture.transparency",
    "picture.removeBackground",
    "picture.compress",
    "picture.reset",
    "picture.style",
    "picture.border",
    "arrange.rotate",
    "arrange.rotation",
    "arrange.wrap",
    "arrange.wrapText",
    "arrange.moveWithText",
    "arrange.position",
    "arrange.bringForward",
    "arrange.sendBackward",
    "arrange.align",
    "arrange.selectionPane",
    "arrange.bounds",
    "arrange.copyTo",
    "arrange.nudge",
    "arrange.group",
    "arrange.ungroup",
    "select.addObject",
    "select.objects",
    "shape.outline",
    "shape.effects",
    "shape.change",
    "shape.textDirection",
    "shape.alignText",
    "shape.link",
    "shape.breakLink",
    "shape.fill",
    "insert.smartArt",
    "smartArt.items",
    "smartArt.textPane",
    "smartArt.addShape",
    "smartArt.promote",
    "smartArt.demote",
    "smartArt.layout",
    "smartArt.colors",
    "smartArt.reset",
    "insert.chart",
    "chart.type",
    "chart.editData",
    "chart.title",
    "chart.legend",
    "chart.dataLabels",
    "draw.lassoMove",
    "draw.lassoDelete",
    "draw.lassoColor",
    "review.hideInk",
    "insert.canvas",
    "canvas.fit",
    "canvas.expand",
    "canvas.scale",
    "canvas.member",
    "insert.wordArt",
    "wordArt.style",
    "wordArt.textFill",
    "wordArt.textOutline",
    "wordArt.transform",
    "shape.connect",
    "insert.quickParts",
    "insert.autoText",
    "insert.docProperty",
    "insert.signatureLine",
    "layout.linkToPrevious",
    "hf.next",
    "hf.previous",
    "hf.position",
    "table.cellMargins",
    "para.setNumberingValue",
    "para.defineBullet",
    "para.defineNumber",
    "review.language",
    "review.showMarkup",
    "review.editor",
    "select.similar",
    "select.extend",
    "design.effects",
    "view.immersive",
    "view.vertical",
    "view.sideToSide",
    "styles.rename",
    "review.restrict",
    "caret.pastCitation",
    "developer.richText",
    "developer.plainText",
    "developer.picture",
    "developer.buildingBlock",
    "developer.checkBox",
    "developer.comboBox",
    "developer.dropDown",
    "developer.datePicker",
    "developer.repeatingSection",
    "developer.designMode",
    "developer.properties",
    "developer.control",
    "developer.toggleCheckBox",
    "developer.chooseItem",
    "developer.setDate",
    "developer.removeControl",
];

/// The window's own commands, beside the registry's: none. (Reading the document is `document.inspect`, `document.text` and the other
/// registry commands that do not mutate.)
pub const ARGUMENT_COMMANDS: &[&str] = &[];

/// Every command the app has and the assistant may NOT run, with the reason: together with `ASSISTANT_COMMANDS` this is the whole
/// command set of the registry (a test holds the two to it, so a command can neither be forgotten nor listed twice). The window's
/// own `ui.*` commands (tabs, dialogs, pointer and key input, screenshots) and its printing are not offered either: a run
/// reaches the document through the engine registry only.
pub const LEFT_OUT: &[(&str, &[&str])] = &[
    (
        "Files move only through the host: a document is opened, saved, exported, printed, closed, recovered, shared or versioned by the host, never by a command id the assistant runs. This also holds the commands whose argument is a path or a file's bytes (a picture, text, an object, a font, a template, a style set, a bibliography or a recipient list read from a file), and the file-level settings that belong to the host's one file.",
        &[
            "font.add",
            "font.remove",
            "font.loadSystem",
            "insert.picture",
            "insert.textFromFile",
            "file.rename",
            "file.new",
            "file.open",
            "file.save",
            "file.saveAs",
            "file.encrypt",
            "file.exportPdf",
            "file.exportPng",
            "file.printMarkup",
            "file.printPdf",
            "file.print",
            "file.close",
            "file.properties",
            "file.info",
            "file.setAuthor",
            "file.privacyOptions",
            "file.privacy",
            "file.options",
            "mailings.recipients",
            "mailings.editRecipients",
            "mailings.filterRecipients",
            "mailings.sortRecipients",
            "mailings.finish",
            "mailings.printDocuments",
            "mailings.envelopes",
            "mailings.labels",
            "references.sources",
            "picture.change",
            "review.compare",
            "review.combine",
            "file.accessibility",
            "file.inspect",
            "file.protect",
            "file.versions",
            "file.recover",
            "file.newFromTemplate",
            "file.saveTemplate",
            "file.autosave",
            "file.compatibility",
            "file.share",
            "insert.object",
            "insert.spreadsheet",
            "tools.templates",
            "tools.attachTemplate",
            "tools.detachTemplate",
            "tools.linkStyles",
            "tools.updateStyles",
            "styles.organizer",
            "styles.copyFrom",
            "file.listTemplates",
        ],
    ),
    (
        "Undo, redo and the history belong to the person: a run is one undo step, and undoing it is theirs.",
        &["edit.undo", "edit.undoTo", "edit.undoHistory", "edit.redo", "edit.repeat"],
    ),
    (
        "The system clipboard is the person's (cut, copy and paste read or write what they hold outside the app), and so are the Spike and the clipboard pane.",
        &[
            "edit.cut",
            "edit.copy",
            "edit.spike",
            "edit.insertSpike",
            "edit.paste",
            "edit.pasteText",
            "edit.pasteMerge",
            "edit.clipboardPane",
            "edit.clipboardItems",
            "edit.pasteClipboardItem",
            "edit.pasteAllClipboard",
            "edit.deleteClipboardItem",
            "edit.clearClipboard",
            "edit.pasteSpecial",
        ],
    ),
    (
        "Interactive: it opens a dialog, begins a pointer gesture or inks with a pen, which only a person's pointer can finish (use the command that takes the numbers: para.set, table.columnWidth, select.range, ...).",
        &[
            "select.dragUnit",
            "edit.formatPainter",
            "format.fontDialog",
            "format.enclose",
            "format.phonetic",
            "para.dialog",
            "table.draw",
            "table.eraser",
            "table.resizeDrag",
            "table.selectAt",
            "table.insertAt",
            "table.moveDrag",
            "draw.select",
            "draw.lasso",
            "draw.eraser",
            "draw.pen",
            "draw.pencil",
            "draw.highlighter",
            "draw.addPen",
            "draw.removePen",
            "draw.stroke",
            "draw.erase",
            "draw.inkToShape",
            "draw.convertInk",
            "draw.replay",
            "format.textEffects",
            "table.borderPainter",
        ],
    ),
    (
        "The window, its appearance, sound and macros are the person's, not the document's: new windows, full screen, the colour theme, reading aloud, recording and running macros, AutoCorrect options, keyboard and ribbon customisation.",
        &[
            "view.fullScreenView",
            "view.darkMode",
            "view.interfaceTheme",
            "view.newWindow",
            "view.fullScreen",
            "tools.recordMacro",
            "tools.macros",
            "tools.autocorrect",
            "tools.customizeKeyboard",
            "tools.customizeRibbon",
            "tools.search",
            "review.readAloud",
            "readAloud.playPause",
            "readAloud.next",
            "readAloud.previous",
            "readAloud.stop",
            "readAloud.speed",
            "readAloud.skipCitations",
            "readAloud.status",
        ],
    ),
];

/// Every command id a run may name, in the order the registry declares them.
pub fn allowed() -> Vec<&'static str> {
    ASSISTANT_COMMANDS.iter().chain(ARGUMENT_COMMANDS).copied().collect()
}

/// The most steps one run may hold.
pub const MAX_STEPS: usize = 50;
/// The longest label of a run.
pub const MAX_LABEL: usize = 120;
/// Argument names that would point a command at a file or a network address: refused anywhere in a step's arguments.
const FORBIDDEN_ARGS: &[&str] = &["path", "paths", "dir", "folder", "file", "url", "uri", "href", "server"];

/// The first forbidden argument name anywhere in `v`: a file or address, a file's bytes (`data` as base64 text; a chart's `data` is an
/// object of numbers and is fine), or a private (`__`) key the interface uses between its own dialogs.
fn forbidden_arg(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => o.iter().find_map(|(k, x)| {
            if FORBIDDEN_ARGS.contains(&k.as_str()) || k.starts_with("__") || (k == "data" && x.is_string()) {
                Some(k.clone())
            } else {
                forbidden_arg(x)
            }
        }),
        Value::Array(a) => a.iter().find_map(forbidden_arg),
        _ => None,
    }
}

/// A run, judged: its label and its steps (the command id and its arguments).
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub label: String,
    pub steps: Vec<(String, Value)>,
}

/// Judge a `command.batch` request before anything runs.
pub fn judge(params: &Value) -> Result<Run, String> {
    let label = params.get("label").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if label.chars().count() > MAX_LABEL {
        return Err(format!("label-too-long: a run's label is at most {MAX_LABEL} characters"));
    }
    let steps = params.get("steps").and_then(Value::as_array).ok_or("run-empty: a run states its steps")?;
    if steps.is_empty() {
        return Err("run-empty: a run states at least one command".into());
    }
    if steps.len() > MAX_STEPS {
        return Err(format!("run-too-long: a run is at most {MAX_STEPS} steps"));
    }
    let mut named = Vec::with_capacity(steps.len());
    for (i, step) in steps.iter().enumerate() {
        let name = step.get("name").and_then(Value::as_str).unwrap_or("");
        if !ASSISTANT_COMMANDS.contains(&name) && !ARGUMENT_COMMANDS.contains(&name) {
            return Err(format!("command-not-allowed: Write does not allow the command '{name}' -- nothing was run"));
        }
        let args = step.get("args").cloned().unwrap_or_else(|| json!({}));
        if !(args.is_object() || args.is_null()) {
            return Err(format!("argument-not-allowed: step {} ('{name}') takes its arguments as an object -- nothing was run", i + 1));
        }
        if let Some(k) = forbidden_arg(&args) {
            return Err(format!("argument-not-allowed: step '{name}' names '{k}', and a run does not reach files or addresses -- nothing was run"));
        }
        named.push((name.to_string(), args));
    }
    let label = if label.is_empty() { "Assistant" } else { label };
    Ok(Run { label: label.to_string(), steps: named })
}

/// Replace every `{"$step": n}` / `{"$step": n, "field": "..."}` in `v` with that field (default `id`) of `results[n]`, the result of
/// an earlier step.
pub fn resolve_step_refs(v: &Value, results: &[Value], step: usize, depth: usize) -> Result<Value, String> {
    if depth > 64 {
        return Err(format!("step {}: arguments nest deeper than 64 levels", step + 1));
    }
    match v {
        Value::Object(o) if o.contains_key("$step") => {
            if let Some(k) = o.keys().find(|k| *k != "$step" && *k != "field") {
                return Err(format!("step {}: a `$step` reference takes only `field`, not `{k}`", step + 1));
            }
            let n = o.get("$step").and_then(Value::as_u64).ok_or_else(|| format!("step {}: `$step` must be a step index (0, 1, ...)", step + 1))?;
            let Some(result) = usize::try_from(n).ok().filter(|n| *n < step).and_then(|n| results.get(n)) else {
                return Err(format!("step {}: `$step` {n} must be an earlier step", step + 1));
            };
            let field = match o.get("field") {
                None => "id",
                Some(Value::String(f)) => f.as_str(),
                Some(_) => return Err(format!("step {}: `field` must be a string", step + 1)),
            };
            result.get(field).cloned().ok_or_else(|| format!("step {}: step {n}'s result has no `{field}`", step + 1))
        }
        Value::Object(o) => {
            let mut out = serde_json::Map::with_capacity(o.len());
            for (k, x) in o {
                out.insert(k.clone(), resolve_step_refs(x, results, step, depth + 1)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(a) => a.iter().map(|x| resolve_step_refs(x, results, step, depth + 1)).collect::<Result<Vec<_>, _>>().map(Value::Array),
        _ => Ok(v.clone()),
    }
}

/// Run a judged run: every step through `step` (the app's one run path for the interface; the engine alone, for a headless session),
/// the whole of it as ONE undo step named by the label, or none of it. `session` reaches the engine session inside `c`.
pub fn run_batch<C>(
    c: &mut C,
    session: impl Fn(&mut C) -> &mut Session,
    mut step: impl FnMut(&mut C, &str, &Value) -> Result<Value, String>,
    params: &Value,
) -> Result<Value, String> {
    let run = judge(params)?;
    // A run starts a step of its own: typing an open group would otherwise swallow it, and the person's Undo would take back more.
    session(c).close_typing();
    let mark = session(c).undo_mark();
    let before = session(c).undo_depth();
    let snap = session(c).edit_snapshot();
    let mut results: Vec<Value> = Vec::with_capacity(run.steps.len());
    let mut failed: Option<String> = None;
    for (i, (name, args)) in run.steps.iter().enumerate() {
        let outcome = resolve_step_refs(args, &results, i, 0).and_then(|args| step(c, name, &args));
        match outcome {
            Ok(result) => results.push(result),
            Err(e) => {
                failed = Some(format!("step-failed: step {} ({name}): {e} -- nothing was changed", i + 1));
                break;
            }
        }
    }
    if let Some(e) = failed {
        session(c).restore(snap);
        return Err(e);
    }
    let s = session(c);
    s.merge_undo_since(mark);
    s.name_undo_step_since(mark, &run.label);
    s.close_typing();
    let undo_step = s.undo_depth() > before;
    let steps: Vec<Value> = run.steps.iter().zip(results).map(|((name, _), result)| json!({ "name": name, "result": result })).collect();
    Ok(json!({ "steps": steps, "undoStep": undo_step }))
}

impl Session {
    /// One step of a run: through the registry, as the control channel runs it (no dialog, no question first).
    pub fn run_host_step(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        self.run(name, args).map_err(|e| e.to_string())
    }

    /// One `command.batch` on this session alone (a headless session, the node build the assistant tests drive): all of it as ONE
    /// undo step, or none of it.
    pub fn run_command_batch(&mut self, params: &Value) -> Result<Value, String> {
        run_batch(self, |s| s, |s, name, args| s.run_host_step(name, args), params)
    }
}

#[cfg(test)]
mod tests;
