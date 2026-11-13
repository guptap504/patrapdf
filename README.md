# PDF Signer

A small desktop app (Rust + [GPUI](https://gpui.rs)) for filling in PDFs: place text boxes, dates, checkmarks and signature images anywhere on a page, then save a flattened copy.

```sh
cargo run --release -- path/to/form.pdf   # the file argument is optional
```

## Using it

- **Open** a PDF with ⌘O, or drop it onto the window.
- **Text / Date / ✓ Check**: pick a tool, then click on the page. Type into text boxes directly. Enter adds a line, Esc finishes, ⌘V pastes.
- **Signatures**: use *+ Add signature image…* (PNG/JPEG). White paper is removed automatically and the result is kept in `~/Library/Application Support/pdf-signer/signatures`. Drag a signature from the sidebar onto a page, or drop an image file from Finder straight onto a page.
- **Edit**: drag items to move them, use the corner handle to resize images, press ⌫ or click ✕ to delete, and use arrow keys to nudge (hold Shift for 10pt steps). ⌘Z undoes.
- **Save** (⌘S) writes `<name>-signed.pdf`. The original file is never modified.

## How it works

| Module | Role |
| --- | --- |
| `pdf.rs` | Renders pages with `hayro` (pure Rust). Writes overlays with `lopdf` as a new content stream on each page: Helvetica text, an RGB image XObject with an alpha soft mask, and a vector checkmark. |
| `editor.rs` | GPUI view: toolbar, signature sidebar, page canvas, mouse and keyboard handling. |
| `signatures.rs` | Imports signature images, removes the background, crops, and persists them. |
| `model.rs` | Item and page geometry, stored in "view space" (points, top-left origin). |

Each page's `view_to_user` matrix is the inverse of hayro's page transform. That handles `/Rotate` and offset crop boxes, so overlays land exactly where they appeared on screen. Text baselines use the measured metrics of the UI font.

GPUI is built with `runtime_shaders`, so the Xcode Metal toolchain isn't required.

## Not yet supported

- Digital signatures.
- Filling AcroForm fields natively. Text is overlaid on top of the fields instead.
- Password-protected PDFs, beyond those with an empty user password.
- Non-Latin text. Output uses the standard Helvetica font with WinAnsi encoding.
