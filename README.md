# PDF Signer

A small desktop app (Rust + [GPUI](https://gpui.rs)) for filling in PDFs: place text boxes, dates, checkmarks and signature images anywhere on a page, then save a flattened copy.

```sh
cargo run --release -- path/to/form.pdf   # the file argument is optional
```

## Using it

- **Open** a PDF with ⌘O, or drop it onto the window.
- **Fill in** (top of the left sidebar): *Text*, *Date* and *Checkmark*. Click one and then click the page, or drag it straight onto the page. Type into text boxes directly: Enter adds a line, Esc finishes, ⌘V pastes text.
- **Date format**: *Format ▾* next to the Date tool offers `13.11.2026`, `13/11/2026`, `11/13/2026`, `2026-11-13`, `13 Nov 2026` and `November 13, 2026`. The choice is remembered in `settings.json`.
- **Redact** (*Protect* in the sidebar): drag a box over anything to black it out. The box is see-through while selected, so you can line it up. Click instead of dragging for a default-sized box. Resize from the corner handle; *Copy to every page* repeats it on every page, e.g. for a header. Redaction is **real**: when you save, every page that has a redaction is rebuilt as a 216 dpi image with the boxes burned in, and that page's original text, fonts, images, links and form fields are deleted from the file. The covered content can't be selected, searched or extracted afterwards. The trade-off is that text on redacted pages is no longer selectable. Pages without redactions are untouched.
- **Watermark** (*Protect* in the sidebar): pick a preset (CONFIDENTIAL, DRAFT, COPY, …) or type your own, then choose a colour, strength, and diagonal or across. It's stamped on every page, see-through, over the page content and under your fields. The preview is drawn by the PDF renderer, so it matches the saved file.
- **Signatures**: use *+ Add signature image…* (PNG/JPEG). White paper is removed automatically and the result is kept in `~/Library/Application Support/pdf-signer/signatures`. Drag a signature from the sidebar onto a page, or drop an image file from Finder straight onto a page.
- **Edit**: drag items to move them, use the corner handle to resize images, press ⌫ or click ✕ to delete, and use arrow keys to nudge (hold Shift for 10pt steps). ⌘Z undoes and ⇧⌘Z redoes.
- **Copy fields**: ⌘C / ⌘X / ⌘V copy, cut and paste the selected field. Pasting puts it on the page you're looking at, in the same spot (or slightly offset if that spot is taken). ⌘D duplicates. *Copy to every page* (the stacked-pages button above a selected field) puts it on every other page at the same position, which is handy for initials.
- **Pages**: the right-hand rail shows a thumbnail of each page; click one to jump there. A blue badge counts the fields on that page.
- **Zoom**: pinch on the trackpad, or hold ⌘ (or Ctrl) and use the scroll wheel, to zoom around the pointer. The percentage in the title bar opens *Fit width* (⌘1, the default), *Fit page* (⌘2) and fixed sizes. − / + step the zoom, ⌘0 is 100%. The fit modes follow the window as it is resized.
- **Pan**: drag any empty part of the page (or the grey area around it) to move around a zoomed-in document. Scrolling works in both directions too.
- **Save** (⌘S) writes `<name>-signed.pdf`. The original file is never modified.
- **Discard** (in the title bar when there are unsaved changes, or File → Discard Changes) reverts to the last save. ⌘Z brings the changes back.
- **Recent documents**: every save adds the document to the start-up gallery, with a thumbnail. Clicking a card reopens the original with your fields still editable. If the original has moved, the saved copy opens instead. ⌘W (or the grid button) closes the document and returns to the gallery.

## How it works

| Module | Role |
| --- | --- |
| `pdf.rs` | Renders pages with `hayro` (pure Rust). Writes overlays with `lopdf` as a new content stream on each page: Helvetica text, an RGB image XObject with an alpha soft mask, a vector checkmark, and the watermark (Helvetica-Bold with an opacity graphics state). Redaction then replaces each affected page with a rendered image, removes its annotations and form fields, drops the structure tree, and prunes unreferenced objects. |
| `editor.rs` | GPUI view: title bar, Fill in / signature sidebar, page canvas, thumbnail rail, menus, mouse and keyboard handling, undo/redo and the field clipboard. |
| `ui.rs` | Light and dark palettes (following the system appearance, live) plus shared components: buttons, menus, tooltips. |
| `assets.rs` | Embedded SVG icon set. |
| `recents.rs` | Gallery store: `recents/index.json`, first-page thumbnails, and copies of the signature images each entry uses, so entries survive removing a signature from the library. |
| `gestures.rs` | Trackpad pinch: GPUI 0.2 has no gesture events, so this adds `magnifyWithEvent:` to GPUI's macOS view class and forwards pinches to the editor. |
| `settings.rs` | User preferences (`settings.json`), currently the date format. |
| `signatures.rs` | Imports signature images, removes the background, crops, and persists them. |
| `model.rs` | Item and page geometry, stored in "view space" (points, top-left origin). |

Each page's `view_to_user` matrix is the inverse of hayro's page transform. That handles `/Rotate` and offset crop boxes, so overlays land exactly where they appeared on screen. Text baselines use the measured metrics of the UI font.

App data lives in `~/Library/Application Support/pdf-signer`. Set `PDF_SIGNER_DATA_DIR` to use another directory, for example for development.

Building on macOS requires the Xcode Metal toolchain (`xcodebuild -downloadComponent MetalToolchain`). To build without it, enable gpui's `runtime_shaders` feature.

## Not yet supported

- Digital signatures.
- Filling AcroForm fields natively. Text is overlaid on top of the fields instead.
- Password-protected PDFs, beyond those with an empty user password.
- Non-Latin text. Output uses the standard Helvetica font with WinAnsi encoding.
