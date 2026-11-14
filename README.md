<p align="center"><img src="packaging/icon/icon-256.png" width="128" alt=""></p>

# PDF Signer

A small desktop app (Rust + [GPUI](https://gpui.rs)) for filling in and signing PDFs. Everything happens on your computer; nothing is uploaded.

## Features

- **Fill in forms.** Type into a PDF's own form fields, tick its check boxes and pick from its lists. The saved copy is still a fillable form.
- **Add to any page.** Place text boxes, dates, checkmarks and signature images anywhere, and keep a library of signatures.
- **Redact.** Cover anything with a black or white box; what's underneath is removed from the saved file.
- **Watermark** every page, and **delete or reorder** pages.
- **Passwords.** Protected PDFs open with their password. A saved copy can keep it, change it or drop it, and any PDF can be saved with a password (AES-256).
- **Any language.** Text in scripts other than Latin (Chinese, Japanese, Korean, Arabic, Hebrew, Hindi, Thai and more) is written with a font from your computer, and input methods work for typing it.
- **Recents.** Saved documents appear in a searchable gallery and reopen with everything you placed still editable.
- **Automatic updates** on macOS, Windows and the AppImage, verified against the project's signing key.

## Install

Download the package for your system from the [latest release](https://github.com/guptap504/pdf-signer/releases/latest).

## Build from source

```sh
cargo run --release -- path/to/form.pdf   # the file argument is optional
```

On Debian/Ubuntu, install the system libraries first with `packaging/linux/install-build-deps.sh`. Windows needs the MSVC build tools.

## Not yet supported

- Digital signatures, and signature, password, file-picker and button form fields.
- XFA forms. Their AcroForm fields are filled in where the file has them.
- Emoji and other characters that only colour fonts can draw. Saving reports them.

## License

[MIT](LICENSE)
