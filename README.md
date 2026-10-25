# Write

Write is OneOS's word processor: it opens, edits and saves Word (.docx), OpenDocument (.odt) and RTF documents, running in the
browser as WebAssembly inside OneOS, and edits one document live with other people through OneOS's collaboration room.

It is based on **WordCraft** by the ArtCraft team (MIT OR Apache-2.0). This repository is OneOS's fork: it keeps the upstream crate
names and file layout so upstream changes merge cleanly, and carries the OneOS host bridge and the OneOS identity.

## What is different from upstream

- **Host bridge.** The web build (`apps/wordcraft-web`) talks to OneOS over `postMessage` to the one parent origin the frame
  page declares: open from bytes, report unsaved changes, save as a new version of the file with a stated base version
  (`src/host.rs`), and edit live (`src/collab.rs`).
- **Live editing shim.** `crates/collab` (`wordcraft-collab`) maps the document body onto OneOS's shared operations: paragraphs are
  CRDT text with a mark per character-formatting field, inline objects are marks on their characters, tables travel whole. It is
  pure Rust, tested natively here and against OneOS's real CRDT engine from the OneOS repository. Not shared live: styles, numbering,
  headers and footers, comments, tracked-change records and pictures' bytes; they are the file's, as opened.
- **Undo and Redo** go to the shared document while a document is live, so they never take back what others typed.
- **Size build.** `cargo build --profile web-release --target wasm32-unknown-unknown -p wordcraft-web --locked`, then
  `wasm-bindgen --target web` and `wasm-opt -Oz`. The OneOS build script (`workspace/scripts/build-craft-libs.mjs`) does all of it.
- **Identity.** The product is called Write; the app id is `one.wkspace.write`; the upstream community links and brand marks are
  removed. Strings that are keys into the upstream translation catalogs (craft-i18n) still say WordCraft until that crate is forked.

## Building the desktop app

```
cargo run --release -p wordcraft
```

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. The upstream copyright notice is kept in
[LICENSE-MIT](LICENSE-MIT) and [NOTICE](NOTICE); third-party material is listed in [ATTRIBUTION.md](ATTRIBUTION.md).
