<p align="center"><img src="packaging/icon/icon-256.png" width="128" alt=""></p>

# PatraPDF

A small desktop app (Rust + [GPUI](https://gpui.rs)) for filling in, signing, redacting and rearranging PDFs. Everything happens on your computer; nothing is uploaded.

*Patra* (पत्र) is Sanskrit for a leaf, and so for a page or a document.

<p align="center"><img src="docs/fill-and-sign.png" width="880" alt="A loan form in PatraPDF, filled in, ticked, highlighted, redacted and signed"></p>

## Features

- **Fill in forms.** Type into a PDF's own form fields, tick its check boxes and pick from its lists. The saved copy is still a fillable form.
- **Add to any page.** Place text boxes, dates, checkmarks and signature images anywhere, and keep a library of signatures.
- **Annotate.** Highlight text, draw freehand, and add rectangles, ovals, lines and arrows, in a choice of colours and line widths.
- **Redact.** Cover anything with a black or white box; what's underneath is removed from the saved file.
- **Watermark** every page, **delete, reorder or rotate** pages, and **add pages** from another PDF.
- **Clean up scans and photos of pages**, as a phone's scanner does. Crop a page by dragging its four corners (⌘K / Ctrl+K): what is between them is pulled straight, so a page photographed at an angle comes out square. Then choose its colours: enhanced (the paper made white, shadows taken off it), grayscale, or black and white. A page this is done to is saved as a picture of itself, so its own text can no longer be selected; what was cropped off is not in the saved file.
- **Passwords.** Protected PDFs open with their password. A saved copy can keep it, change it or drop it, and any PDF can be saved with a password (AES-256).
- **Any language.** Text in scripts other than Latin (Chinese, Japanese, Korean, Arabic, Hebrew, Hindi, Thai and more) is written with a font from your computer, and input methods work for typing it.
- **Find** a word or phrase in the document (⌘F / Ctrl+F) and step through the matches.
- **Go to a page** by its number (⌥⌘G / Ctrl+Alt+G), or move through the document with Page Up, Page Down, Home and End.
- **Copy text.** Pick the Select text tool, drag over the document's text (or double-click a word), and copy it (⌘C / Ctrl+C).
- **Several documents at once**, each in its own window.
- **Copy the finished PDF to the clipboard** instead of saving it, and paste it straight into an email, a chat or a folder.
- **Print** the finished document (⌘P / Ctrl+P) from the system's print dialog. On Linux that is the desktop's own, through its print portal; on a desktop without one, the finished PDF opens in your PDF viewer, to print from there.
- **Menus** for everything: in the menu bar on macOS, and in a menu bar at the top of each window on Windows and Linux (F10 opens it).
- **Recents.** Saved documents appear in a searchable gallery and reopen with everything you placed still editable.
- **Automatic updates** on macOS, Windows and the AppImage, verified against the project's signing key.

A photographed page, cropped by its corners, then straightened and enhanced:

<p align="center">
  <img src="docs/scan-crop.png" width="49%" alt="Cropping a photo of a page: handles on its four corners and sides">
  <img src="docs/scan-cleaned.png" width="49%" alt="The same page pulled straight, its paper white and its shadow gone">
</p>

## Install

Download the package for your system from the [latest release](https://github.com/guptap504/patrapdf/releases/latest).

## Build from source

```sh
cargo run --release -- path/to/form.pdf   # the file argument is optional
```

On Debian/Ubuntu, install the system libraries first with `packaging/linux/install-build-deps.sh`. Windows needs the MSVC build tools.

## Not yet supported

- Digital signatures, and signature, password, file-picker and button form fields.
- XFA forms. Their AcroForm fields are filled in where the file has them.
- Filling in form fields on pages added from another PDF. They're kept as they look.
- Finding or selecting text you added yourself or typed into form fields, or text in a scan: find and Select text read the document's own text.
- Emoji and other characters that only colour fonts can draw. Saving reports them.

## License

[MIT](LICENSE)
