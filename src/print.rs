//! Printing the finished PDF, from the system's print dialog.
//!
//! - macOS: the print dialog, as a sheet on the window. PDFKit draws the pages.
//! - Windows: the print dialog. The pages are drawn here, and go to the
//!   printer chosen as pictures.
//! - Linux: the desktop's print dialog, which it shows through its portal
//!   and then gives the PDF to the printer chosen.
//!
//! Where no dialog can be shown (a desktop without the portal, say), the
//! finished PDF is kept in a folder of the app's own, to be opened in the
//! system's PDF viewer and printed from there.

use std::path::PathBuf;

use anyhow::Result;

/// The window a print dialog is shown on, as the system knows it: an `NSView`
/// on macOS, a window handle on Windows, an X11 window on Linux (none under
/// Wayland, where the dialog stands by itself).
#[derive(Clone, Copy)]
pub struct Parent(#[cfg_attr(test, allow(dead_code))] Option<usize>);

impl Parent {
    pub fn of(window: &impl raw_window_handle::HasWindowHandle) -> Self {
        #[cfg(not(test))]
        {
            use raw_window_handle::RawWindowHandle;
            Self(match window.window_handle().ok().map(|handle| handle.as_raw()) {
                Some(RawWindowHandle::AppKit(handle)) => Some(handle.ns_view.as_ptr() as usize),
                Some(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get() as usize),
                Some(RawWindowHandle::Xcb(handle)) => Some(handle.window.get() as usize),
                Some(RawWindowHandle::Xlib(handle)) => Some(handle.window as usize),
                _ => None,
            })
        }
        // (GPUI's test windows have no native view, and panic when asked for one.)
        #[cfg(test)]
        {
            let _ = window;
            Self(None)
        }
    }
}

/// What came of printing. (Which of these a system can answer with differs.)
#[allow(dead_code)]
pub enum Outcome {
    /// The system's print dialog is up, and sees to the rest itself.
    Asked,
    /// The document went to the printer chosen in the dialog.
    Sent,
    /// The dialog was closed without printing.
    Cancelled,
    /// There is no dialog: the PDF was kept as this file, to be opened in the system's viewer.
    Kept(PathBuf),
}

/// Why a system's own way of printing came to nothing.
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[cfg_attr(test, allow(dead_code))]
enum Trouble {
    /// Its dialog couldn't be shown: the document is opened in the viewer instead.
    NoDialog(anyhow::Error),
    /// A printer was chosen, and the document didn't get to it.
    Failed(anyhow::Error),
}

/// Print the PDF in `bytes`, a document called `name`. Call it on the main
/// thread, and not from inside an update of the app: a dialog runs there.
#[cfg(not(test))]
pub async fn print(bytes: Vec<u8>, name: &str, parent: Parent) -> Result<Outcome> {
    #[cfg(target_os = "macos")]
    if let Some(view) = parent.0 {
        if mac::print(&bytes, name, view) {
            return Ok(Outcome::Asked);
        }
    }
    #[cfg(target_os = "windows")]
    match win::print(&bytes, name, parent.0).await {
        Ok(outcome) => return Ok(outcome),
        Err(Trouble::Failed(e)) => return Err(e),
        Err(Trouble::NoDialog(e)) => eprintln!("printing from the PDF viewer instead: {e:#}"),
    }
    #[cfg(target_os = "linux")]
    {
        // The portal is given the file to print; without one, it is what the viewer opens.
        let kept = crate::clipboard::keep_to_print(name, &bytes)?;
        return match portal::print(&kept, name, parent.0).await {
            Ok(outcome) => Ok(outcome),
            Err(Trouble::Failed(e)) => Err(e),
            Err(Trouble::NoDialog(e)) => {
                eprintln!("printing from the PDF viewer instead: {e:#}");
                Ok(Outcome::Kept(kept))
            }
        };
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = parent;
        crate::clipboard::keep_to_print(name, &bytes).map(Outcome::Kept)
    }
}

/// Tests must never reach a printer: they note what would have been printed.
#[cfg(test)]
pub async fn print(bytes: Vec<u8>, name: &str, _: Parent) -> Result<Outcome> {
    tests::PRINTED.with(|printed| *printed.borrow_mut() = Some((name.to_string(), bytes)));
    Ok(Outcome::Asked)
}

/// What the test on this thread last printed: the document's name and the PDF.
#[cfg(test)]
pub fn last_printed() -> Option<(String, Vec<u8>)> {
    tests::PRINTED.with(|printed| printed.borrow().clone())
}

/// The most dots to the inch a page is drawn with for a printer that is given
/// pictures. One with more is given each dot several times over: a page at 600
/// would take hundreds of megabytes, for nothing the eye can tell.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const SHARPEST: f32 = 300.;

/// Where a page goes on a printer's paper, and how it is drawn for it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct Placed {
    /// On its side: a quarter turn, so that a wide page is on paper fed the tall way (or the other way about).
    turned: bool,
    /// The pixels to the point its picture is drawn with.
    scale: f32,
    /// Its top left corner, and its width and height, in the printer's dots.
    origin: (i32, i32),
    size: (i32, i32),
}

/// Place a `width` × `height` point page on paper with `area` dots to print
/// on, `dpi` of them to the inch across and down: as large as there is room
/// for, whatever size it was made for, and in the middle.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn place((width, height): (f32, f32), area: (i32, i32), dpi: (i32, i32)) -> Placed {
    let dots = (dpi.0 as f32 / 72., dpi.1 as f32 / 72.);
    let paper = (area.0 as f32 / dots.0, area.1 as f32 / dots.1);
    let turned = (width > height) != (paper.0 > paper.1);
    let (width, height) = if turned { (height, width) } else { (width, height) };
    let fit = (paper.0 / width.max(1.)).min(paper.1 / height.max(1.));
    let size = (((width * fit * dots.0).round() as i32).clamp(1, area.0.max(1)), ((height * fit * dots.1).round() as i32).clamp(1, area.1.max(1)));
    Placed { turned, scale: (fit * dots.0.max(dots.1)).min(SHARPEST / 72.), origin: ((area.0 - size.0) / 2, (area.1 - size.1) / 2), size }
}

/// The bytes a row of a `width` pixel bitmap takes at three a pixel: rows are a multiple of four long.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn bitmap_row_bytes(width: usize) -> usize {
    (width * 3 + 3) & !3
}

/// Rows `top .. top + rows` of a picture (`bgra`, `width` pixels wide, from
/// the top down) as a bitmap of the kind printers have always been given:
/// three bytes a pixel, rows from the bottom up.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn bitmap_strip(bgra: &[u8], width: usize, top: usize, rows: usize, bitmap: &mut [u8]) {
    let row_bytes = bitmap_row_bytes(width);
    for row in 0..rows {
        let from = &bgra[(top + rows - 1 - row) * width * 4..][..width * 4];
        let to = &mut bitmap[row * row_bytes..][..width * 3];
        for (dot, pixel) in to.chunks_exact_mut(3).zip(from.chunks_exact(4)) {
            dot.copy_from_slice(&pixel[..3]);
        }
    }
}

/// Windows: its print dialog, and then the pages drawn on the printer chosen.
#[cfg(target_os = "windows")]
#[cfg_attr(test, allow(dead_code))]
mod win {
    use std::ffi::c_void;

    use anyhow::{Context as _, Result, anyhow, bail};
    use windows::Win32::Foundation::{ERROR_CANCELLED, GetLastError, GlobalFree, HWND};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, COLORONCOLOR, DIB_RGB_COLORS, DeleteDC, GetDeviceCaps, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, SRCCOPY,
        SetStretchBltMode, StretchDIBits, VERTRES,
    };
    use windows::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
    use windows::Win32::UI::Controls::Dialogs::{
        PD_HIDEPRINTTOFILE, PD_NOCURRENTPAGE, PD_NOSELECTION, PD_PAGENUMS, PD_RESULT_PRINT, PD_RETURNDC, PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGEXW,
        PRINTPAGERANGE, PrintDlgExW, START_PAGE_GENERAL,
    };
    use windows::core::PCWSTR;

    use super::{Outcome, Placed, Trouble, bitmap_row_bytes, bitmap_strip, place};
    use crate::pdf::{Pictures, RenderedPage};

    /// A page goes to the printer in strips of about this many bytes: some
    /// printers' drivers don't take a whole page as one picture.
    const STRIP_BYTES: usize = 8 << 20;
    /// The most runs of pages ("1-3, 7, 9-12") the dialog takes.
    const MOST_RANGES: usize = 16;

    /// A printer chosen in the print dialog, ready to be drawn on, and the
    /// pages asked for. (The device context is kept as a number, so that
    /// another thread can be given it: one draws on it at a time.)
    struct Job {
        dc: isize,
        pages: Vec<usize>,
    }

    impl Job {
        fn dc(&self) -> HDC {
            HDC(self.dc as *mut c_void)
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the device context is this job's own, from the print dialog.
            unsafe {
                let _ = DeleteDC(self.dc());
            }
        }
    }

    /// Show the print dialog on the window `owner` for a document of
    /// `page_count` pages. Nothing if it is closed without printing.
    fn ask(owner: usize, page_count: usize) -> Result<Option<Job>> {
        let mut ranges = [PRINTPAGERANGE::default(); MOST_RANGES];
        let mut dialog = PRINTDLGEXW {
            lStructSize: size_of::<PRINTDLGEXW>() as u32,
            hwndOwner: HWND(owner as *mut c_void),
            // Copies are left to the printer (the dialog offers them only if it
            // can make them). There is no selection to print, nor a "current page".
            Flags: PD_RETURNDC | PD_NOSELECTION | PD_NOCURRENTPAGE | PD_HIDEPRINTTOFILE | PD_USEDEVMODECOPIESANDCOLLATE,
            nMaxPageRanges: MOST_RANGES as u32,
            lpPageRanges: ranges.as_mut_ptr(),
            nMinPage: 1,
            nMaxPage: page_count as u32,
            nCopies: 1,
            nStartPage: START_PAGE_GENERAL,
            ..Default::default()
        };
        // SAFETY: the structure is filled in as the dialog asks, `ranges`
        // outlives the call, and `owner` is a window of this app's. The dialog
        // runs its own loop on this, the main thread, as the file dialogs do.
        let shown = unsafe { PrintDlgExW(&mut dialog) };
        let (dc, flags, action, range_count) = (dialog.hDC, dialog.Flags, dialog.dwResultAction, dialog.nPageRanges);
        // SAFETY: what the dialog allocated for the printer's settings is ours to free.
        unsafe {
            for memory in [dialog.hDevMode, dialog.hDevNames] {
                if !memory.0.is_null() {
                    let _ = GlobalFree(Some(memory));
                }
            }
        }
        // (Dropped wherever this returns early: the device context is not left behind.)
        let job = (!dc.is_invalid()).then(|| Job { dc: dc.0 as isize, pages: Vec::new() });
        shown.context("the print dialog could not be shown")?;
        if action != PD_RESULT_PRINT {
            return Ok(None);
        }
        let mut job = job.context("the print dialog gave no printer")?;
        if flags.contains(PD_PAGENUMS) {
            let asked = &ranges[..(range_count as usize).min(MOST_RANGES)];
            job.pages = asked
                .iter()
                .flat_map(|range| range.nFromPage.min(range.nToPage)..=range.nFromPage.max(range.nToPage))
                .filter(|number| (1..=page_count as u32).contains(number))
                .map(|number| number as usize - 1)
                .collect();
        }
        if job.pages.is_empty() {
            job.pages = (0..page_count).collect();
        }
        Ok(Some(job))
    }

    impl Job {
        /// Draw the pages asked for on the printer. False if it was called off
        /// where the printer asks for more (the file to print to, say).
        fn send(self, pictures: &Pictures, name: &str) -> Result<bool> {
            let dc = self.dc();
            // SAFETY (here and below): `dc` is the live device context of this job.
            let caps = |index| unsafe { GetDeviceCaps(Some(dc), index) };
            // What of the paper can be printed on, in the printer's dots, and how many of those there are to the inch.
            let (area, dpi) = ((caps(HORZRES), caps(VERTRES)), (caps(LOGPIXELSX), caps(LOGPIXELSY)));
            if area.0 <= 0 || area.1 <= 0 || dpi.0 <= 0 || dpi.1 <= 0 {
                bail!("the printer did not say how large its paper is");
            }
            let sizes = pictures.sizes();
            let title: Vec<u16> = name.encode_utf16().chain([0]).collect();
            let document = DOCINFOW { cbSize: size_of::<DOCINFOW>() as i32, lpszDocName: PCWSTR(title.as_ptr()), ..Default::default() };
            if unsafe { StartDocW(dc, &document) } <= 0 {
                if unsafe { GetLastError() } == ERROR_CANCELLED {
                    return Ok(false);
                }
                bail!("the printer did not take the document");
            }
            unsafe { SetStretchBltMode(dc, COLORONCOLOR) };
            for &page in &self.pages {
                let drawn = sizes.get(page).context("no such page").and_then(|size| self.page(pictures, page, *size, area, dpi));
                if let Err(e) = drawn {
                    unsafe { AbortDoc(dc) };
                    return Err(e);
                }
            }
            if unsafe { EndDoc(dc) } <= 0 {
                bail!("the printer did not take the end of the document");
            }
            Ok(true)
        }

        /// One page, of `size` in points: as large as the paper has room for,
        /// in the middle of it, and on its side if that is how it fits.
        fn page(&self, pictures: &Pictures, index: usize, size: (f32, f32), area: (i32, i32), dpi: (i32, i32)) -> Result<()> {
            let dc = self.dc();
            let Placed { turned, scale, origin, size } = place(size, area, dpi);
            let picture = pictures.page(index, scale).with_context(|| format!("page {} could not be drawn", index + 1))?;
            let picture = if turned { picture.turned(1) } else { picture };
            if unsafe { StartPage(dc) } <= 0 {
                bail!("the printer did not take page {}", index + 1);
            }
            strips(dc, &picture, origin, size);
            if unsafe { EndPage(dc) } <= 0 {
                bail!("the printer did not take page {}", index + 1);
            }
            Ok(())
        }
    }

    /// Draw `picture` on the printer, filling `size` dots from `origin`: strip
    /// by strip down the page, each a bitmap of its own.
    fn strips(dc: HDC, picture: &RenderedPage, origin: (i32, i32), size: (i32, i32)) {
        let (width, height) = (picture.width as usize, picture.height as usize);
        if width == 0 || height == 0 || picture.bgra.len() != width * height * 4 {
            return;
        }
        let row_bytes = bitmap_row_bytes(width);
        let most_rows = (STRIP_BYTES / row_bytes).max(1);
        let mut dots = vec![0u8; row_bytes * most_rows.min(height)];
        // Where a row of the picture comes on the paper.
        let down = |row: usize| origin.1 + (row as i64 * size.1 as i64 / height as i64) as i32;
        let mut top = 0;
        while top < height {
            let rows = most_rows.min(height - top);
            bitmap_strip(&picture.bgra, width, top, rows, &mut dots);
            let header = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: rows as i32,
                    biPlanes: 1,
                    biBitCount: 24,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            // SAFETY: `dots` holds `rows` rows of `row_bytes` bytes, as the header says.
            unsafe {
                StretchDIBits(
                    dc,
                    origin.0,
                    down(top),
                    size.0,
                    down(top + rows) - down(top),
                    0,
                    0,
                    width as i32,
                    rows as i32,
                    Some(dots.as_ptr() as *const c_void),
                    &header,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }
            top += rows;
        }
    }

    /// Ask for a printer in the print dialog on the window `owner`, and print
    /// the PDF on it. The pages are drawn and sent on a thread of their own:
    /// a long document takes a while.
    pub async fn print(bytes: &[u8], name: &str, owner: Option<usize>) -> Result<Outcome, Trouble> {
        let owner = owner.context("the window has no handle to show the print dialog on").map_err(Trouble::NoDialog)?;
        let pictures = Pictures::of(bytes.to_vec()).map_err(Trouble::Failed)?;
        let Some(job) = ask(owner, pictures.sizes().len()).map_err(Trouble::NoDialog)? else {
            return Ok(Outcome::Cancelled);
        };
        let (done, sent) = futures::channel::oneshot::channel();
        let name = name.to_string();
        std::thread::spawn(move || {
            let _ = done.send(job.send(&pictures, &name));
        });
        match sent.await {
            Ok(Ok(true)) => Ok(Outcome::Sent),
            Ok(Ok(false)) => Ok(Outcome::Cancelled),
            Ok(Err(e)) => Err(Trouble::Failed(e)),
            Err(_) => Err(Trouble::Failed(anyhow!("printing stopped before the document was sent"))),
        }
    }
}

/// Linux: the desktop's print dialog, through its portal
/// (`org.freedesktop.portal.Print`), which GNOME, KDE and the others answer
/// with their own. It is then given the PDF for the printer chosen.
#[cfg(target_os = "linux")]
#[cfg_attr(test, allow(dead_code))]
mod portal {
    use std::path::Path;

    use anyhow::Context as _;
    use ashpd::WindowIdentifier;
    use ashpd::desktop::ResponseError;
    use ashpd::desktop::print::{PageSetup, PrintProxy, Settings};

    use super::{Outcome, Trouble};

    /// Show the print dialog (on the X11 window `window`, if there is one)
    /// and print the PDF kept as `file`, a document called `name`.
    pub async fn print(file: &Path, name: &str, window: Option<usize>) -> Result<Outcome, Trouble> {
        let no_dialog = |e: ashpd::Error| Trouble::NoDialog(anyhow::Error::new(e).context("the desktop's print dialog could not be shown"));
        let failed = |e: ashpd::Error| Trouble::Failed(anyhow::Error::new(e).context("the document could not be sent to the printer"));
        let parent = window.map(|window| WindowIdentifier::from_xid(window as std::os::raw::c_ulong));
        let portal = PrintProxy::new().await.map_err(no_dialog)?;
        let asked = portal
            .prepare_print(parent.as_ref(), "Print", Settings::default(), PageSetup::default(), None::<&str>, true)
            .await
            .map_err(no_dialog)?;
        let chosen = match asked.response() {
            Ok(chosen) => chosen,
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => return Ok(Outcome::Cancelled),
            Err(e) => return Err(no_dialog(e)),
        };
        let pdf = std::fs::File::open(file).with_context(|| format!("couldn't read {}", file.display())).map_err(Trouble::Failed)?;
        // With what the dialog gave: the printer and its settings are those chosen, and it isn't shown again.
        match portal.print(parent.as_ref(), name, &pdf, Some(chosen.token), true).await.and_then(|sent| sent.response()) {
            Ok(()) => Ok(Outcome::Sent),
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(Outcome::Cancelled),
            Err(e) => Err(failed(e)),
        }
    }
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
    use super::*;

    thread_local! {
        pub(super) static PRINTED: std::cell::RefCell<Option<(String, Vec<u8>)>> = const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn a_page_fills_the_paper_from_the_middle_and_lies_on_its_side_if_it_fits_so() {
        // A4 at 600 dots to the inch, with a margin the printer can't print on.
        let (area, dpi) = ((4760, 6814), (600, 600));
        // An A4 page: as large as the margins leave room for, drawn at the sharpest a picture is.
        let page = place((595., 842.), area, dpi);
        assert_eq!((page.turned, page.scale), (false, SHARPEST / 72.));
        assert_eq!(page.size.0, area.0);
        assert!((page.size.1 as f32 / page.size.0 as f32 - 842. / 595.).abs() < 0.001, "{page:?}");
        assert_eq!(page.origin, (0, (area.1 - page.size.1) / 2));
        // The same page made to lie sideways is turned, and then no different.
        assert_eq!(place((842., 595.), area, dpi), Placed { turned: true, ..page });
        // A small page is made larger, a large one smaller: each fills the paper one way.
        for size in [(200., 300.), (2000., 2400.), (300., 300.)] {
            let placed = place(size, area, dpi);
            assert!(placed.size.0 == area.0 || placed.size.1 == area.1, "{placed:?}");
            assert!(placed.origin.0 >= 0 && placed.origin.1 >= 0 && placed.size.0 <= area.0 && placed.size.1 <= area.1, "{placed:?}");
            assert!(!placed.turned);
        }
        // A printer with few dots is given no more than it has; one with more across than down has its dots counted each way.
        assert!((place((595., 842.), (1190, 1684), (144, 144)).scale - 2.).abs() < 0.001);
        let uneven = place((612., 792.), (6120, 3960), (720, 360));
        assert_eq!((uneven.turned, uneven.size, uneven.origin), (false, (6120, 3960), (0, 0)));
    }

    #[test]
    fn a_picture_goes_to_the_printer_as_a_bitmap_from_the_bottom_up() {
        assert_eq!([1, 2, 4, 5].map(bitmap_row_bytes), [4, 8, 12, 16]);
        // Three pixels wide, three rows: 1.. on the first, 4.. on the second, 7.. on the third.
        let bgra: Vec<u8> = (1..=9u8).flat_map(|n| [n, n + 10, n + 20, 255]).collect();
        let mut bitmap = vec![0xEE; 12 * 2];
        // The last two rows: the bottom one first, each padded to twelve bytes, without its alpha.
        bitmap_strip(&bgra, 3, 1, 2, &mut bitmap);
        assert_eq!(bitmap[..9], [7, 17, 27, 8, 18, 28, 9, 19, 29]);
        assert_eq!(bitmap[12..21], [4, 14, 24, 5, 15, 25, 6, 16, 26]);
        // The first row alone.
        bitmap_strip(&bgra, 3, 0, 1, &mut bitmap);
        assert_eq!(bitmap[..9], [1, 11, 21, 2, 12, 22, 3, 13, 23]);
    }
}
