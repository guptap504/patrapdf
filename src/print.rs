//! Printing the finished PDF.
//!
//! On macOS it goes to the system's print dialog, as a sheet on the window.
//! GPUI has no way to a print dialog anywhere else, so there the finished PDF
//! is kept in a folder of the app's own and opened in the system's PDF viewer,
//! to print from.

use std::path::PathBuf;

use anyhow::Result;

/// The window a print dialog is shown on.
#[derive(Clone, Copy)]
pub struct Parent(#[cfg_attr(any(test, not(target_os = "macos")), allow(dead_code))] Option<usize>);

impl Parent {
    pub fn of(window: &gpui::Window) -> Self {
        #[cfg(all(target_os = "macos", not(test)))]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            Self(match HasWindowHandle::window_handle(window).ok().map(|handle| handle.as_raw()) {
                Some(RawWindowHandle::AppKit(handle)) => Some(handle.ns_view.as_ptr() as usize),
                _ => None,
            })
        }
        // (GPUI's test windows have no native view, and panic when asked for one.)
        #[cfg(not(all(target_os = "macos", not(test))))]
        {
            let _ = window;
            Self(None)
        }
    }
}

/// Print the PDF in `bytes`, a document called `name`. Call it on the main
/// thread, and not from inside an update of the app: the dialog runs there.
///
/// Returns nothing once the print dialog is up. Where there is none it returns
/// the file the PDF was kept as, to be opened in the system's viewer.
#[cfg(not(test))]
pub fn print(bytes: &[u8], name: &str, parent: Parent) -> Result<Option<PathBuf>> {
    #[cfg(target_os = "macos")]
    if let Some(view) = parent.0 {
        if mac::print(bytes, name, view) {
            return Ok(None);
        }
    }
    let _ = parent;
    crate::clipboard::keep_to_print(name, bytes).map(Some)
}

/// Tests must never reach a printer: they note what would have been printed.
#[cfg(test)]
pub fn print(bytes: &[u8], name: &str, _: Parent) -> Result<Option<PathBuf>> {
    tests::PRINTED.with(|printed| *printed.borrow_mut() = Some((name.to_string(), bytes.to_vec())));
    Ok(None)
}

/// What the test on this thread last printed: the document's name and the PDF.
#[cfg(test)]
pub fn last_printed() -> Option<(String, Vec<u8>)> {
    tests::PRINTED.with(|printed| printed.borrow().clone())
}

// objc 0.2's macros check a `cargo-clippy` feature that this crate doesn't declare.
#[cfg(all(target_os = "macos", not(test)))]
#[allow(unexpected_cfgs)]
mod mac {
    use std::ffi::c_void;

    use objc::runtime::{Class, Object, YES};
    use objc::{msg_send, sel, sel_impl};

    // PDFKit, which draws the pages for the printer, is part of Quartz.
    #[link(name = "Quartz", kind = "framework")]
    unsafe extern "C" {}

    /// `kPDFPrintPageScaleToFit`: each page fills the paper, whatever size it was made for.
    const SCALE_TO_FIT: isize = 1;
    const UTF8: usize = 4;

    /// Show the print dialog for the PDF on the window of `view` (an `NSView`).
    /// False if it couldn't be shown.
    pub fn print(bytes: &[u8], name: &str, view: usize) -> bool {
        let (Some(pdf_document), Some(ns_data), Some(ns_string), Some(ns_print_info)) =
            (Class::get("PDFDocument"), Class::get("NSData"), Class::get("NSString"), Class::get("NSPrintInfo"))
        else {
            return false;
        };
        // SAFETY: on the main thread, with `view` the live NSView of a window of
        // this app. Every object is either autoreleased or released here once the
        // print operation (which keeps what it needs) has been started.
        unsafe {
            let window: *mut Object = msg_send![view as *mut Object, window];
            if window.is_null() {
                return false;
            }
            let data: *mut Object = msg_send![ns_data, dataWithBytes: bytes.as_ptr() as *const c_void length: bytes.len()];
            let document: *mut Object = msg_send![pdf_document, alloc];
            let document: *mut Object = msg_send![document, initWithData: data];
            if document.is_null() {
                return false;
            }
            // A copy of the shared settings: what is chosen for this job stays with it.
            let shared: *mut Object = msg_send![ns_print_info, sharedPrintInfo];
            let info: *mut Object = msg_send![shared, copy];
            let operation: *mut Object =
                msg_send![document, printOperationForPrintInfo: info scalingMode: SCALE_TO_FIT autoRotate: YES];
            let shown = !operation.is_null();
            if shown {
                let title: *mut Object = msg_send![ns_string, alloc];
                let title: *mut Object =
                    msg_send![title, initWithBytes: name.as_ptr() as *const c_void length: name.len() encoding: UTF8];
                if !title.is_null() {
                    let _: () = msg_send![operation, setJobTitle: title];
                    let _: () = msg_send![title, release];
                }
                let none = std::ptr::null::<c_void>();
                let _: () = msg_send![operation, runOperationModalForWindow: window delegate: none didRunSelector: none contextInfo: none];
            }
            let _: () = msg_send![info, release];
            let _: () = msg_send![document, release];
            shown
        }
    }
}

#[cfg(test)]
mod tests {
    thread_local! {
        pub(super) static PRINTED: std::cell::RefCell<Option<(String, Vec<u8>)>> = const { std::cell::RefCell::new(None) };
    }
}
