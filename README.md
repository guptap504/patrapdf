<p align="center"><img src="packaging/icon/icon-256.png" width="128" alt=""></p>

# PDF Signer

A small desktop app (Rust + [GPUI](https://gpui.rs)) for filling in PDFs: place text boxes, dates, checkmarks and signature images anywhere on a page, redact, watermark, delete and reorder pages, then save a flattened copy. Everything happens on your computer; nothing is uploaded.

## Install

Download the package for your system from the [latest release](https://github.com/guptap504/pdf-signer/releases/latest).

## Build from source

```sh
cargo run --release -- path/to/form.pdf   # the file argument is optional
```

On Debian/Ubuntu, install the system libraries first with `packaging/linux/install-build-deps.sh`. Windows needs the MSVC build tools.

## Not yet supported

- Digital signatures.
- Filling AcroForm fields natively. Text is overlaid on top of the fields instead.
- Password-protected PDFs, beyond those with an empty user password.
- Non-Latin text. Output uses the standard Helvetica font with WinAnsi encoding.

## License

[MIT](LICENSE)
