// Release builds on Windows are GUI apps: no console window behind them.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod about;
mod assets;
mod clipboard;
mod editor;
mod fonts;
mod forms;
mod gestures;
mod model;
mod pdf;
mod recents;
mod search;
mod settings;
mod signatures;
mod ui;
mod updater;

use std::path::PathBuf;

use futures::StreamExt;
use gpui::{Application, Menu, MenuItem};

use editor::{
    About, AddPages, AddSignature, CheckForUpdates, CloseDocument, Copy, CopyPdf, Cut, DiscardChanges, Duplicate, Find, FindNext, FindPrevious, KeyboardShortcuts, MinimizeWindow, NewWindow, Open,
    Paste, Quit, Redo, Save, SaveWithPassword, ToggleLeftSidebar, ToggleRightSidebar, Undo, VisitWebsite, ZoomFitPage, ZoomFitWidth, ZoomIn, ZoomOut,
    ZoomReset, ZoomWindow,
};

fn main() {
    // The PDFs to open, each in a window of its own.
    let mut paths = std::env::args_os().skip(1).map(PathBuf::from);
    let initial = paths.next();
    let more: Vec<PathBuf> = paths.collect();

    let app = Application::new().with_assets(assets::Assets);
    // PDFs opened from the Finder ("Open With", or dropped on the Dock icon) arrive
    // as file URLs, possibly before the first window exists; queue them until it does.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();
    app.on_open_urls(move |urls| {
        let _ = open_tx.unbounded_send(urls);
    });

    app.run(move |cx| {
        editor::bind_keys(cx);
        signatures::secure_data_dir();
        clipboard::tidy();
        let updater = updater::init(settings::Settings::load().check_for_updates, cx);
        let self_updating = updater.read(cx).support().is_ok();
        cx.on_action(|_: &Quit, cx| editor::request_quit(cx));
        // Once the window the action came from is done with it: the new one is placed beside it.
        cx.on_action(|_: &NewWindow, cx| {
            cx.defer(|cx| {
                if let Err(e) = editor::open_editor(cx, None) {
                    eprintln!("failed to open window: {e:#}");
                }
            })
        });
        cx.set_menus(vec![
            Menu {
                name: "PatraPDF".into(),
                items: [MenuItem::action("About PatraPDF", About)]
                    .into_iter()
                    .chain(self_updating.then(|| MenuItem::action("Check for Updates…", CheckForUpdates)))
                    .chain([MenuItem::separator(), MenuItem::action("Quit PatraPDF", Quit)])
                    .collect(),
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("New Window", NewWindow),
                    MenuItem::action("Open…", Open),
                    MenuItem::action("Save As…", Save),
                    MenuItem::action("Save with Password…", SaveWithPassword),
                    MenuItem::action("Copy PDF to Clipboard", CopyPdf),
                    MenuItem::action("Discard Changes…", DiscardChanges),
                    MenuItem::action("Close Document", CloseDocument),
                    MenuItem::separator(),
                    MenuItem::action("Add Pages from PDF…", AddPages),
                    MenuItem::action("Add Signature Image…", AddSignature),
                ],
            },
            Menu {
                name: "Edit".into(),
                items: vec![
                    MenuItem::action("Undo", Undo),
                    MenuItem::action("Redo", Redo),
                    MenuItem::separator(),
                    MenuItem::action("Cut", Cut),
                    MenuItem::action("Copy", Copy),
                    MenuItem::action("Paste", Paste),
                    MenuItem::action("Duplicate", Duplicate),
                    MenuItem::separator(),
                    MenuItem::action("Find…", Find),
                    MenuItem::action("Find Next", FindNext),
                    MenuItem::action("Find Previous", FindPrevious),
                ],
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Zoom In", ZoomIn),
                    MenuItem::action("Zoom Out", ZoomOut),
                    MenuItem::action("Actual Size", ZoomReset),
                    MenuItem::action("Fit Width", ZoomFitWidth),
                    MenuItem::action("Fit Page", ZoomFitPage),
                    MenuItem::separator(),
                    MenuItem::action("Toggle Left Sidebar", ToggleLeftSidebar),
                    MenuItem::action("Toggle Right Sidebar", ToggleRightSidebar),
                ],
            },
            // macOS lists the open windows at the end of the menu with this name.
            Menu {
                name: "Window".into(),
                items: vec![MenuItem::action("Minimize", MinimizeWindow), MenuItem::action("Zoom", ZoomWindow)],
            },
            Menu {
                name: "Help".into(),
                items: vec![
                    MenuItem::action("Keyboard Shortcuts", KeyboardShortcuts),
                    MenuItem::separator(),
                    MenuItem::action("Visit Website", VisitWebsite),
                ],
            },
        ]);

        if let Err(e) = editor::open_editor(cx, initial) {
            eprintln!("failed to open window: {e:#}");
            cx.quit();
        }
        for path in more {
            editor::open_document(cx, path, None);
        }
        cx.spawn(async move |cx| {
            while let Some(urls) = open_rx.next().await {
                let _ = cx.update(|cx| {
                    for path in urls.iter().filter_map(|u| file_url_path(u)) {
                        editor::open_document(cx, path, None);
                    }
                });
            }
        })
        .detach();
        // Closing the last window quits.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

/// The local path of a `file://` URL (percent-decoded), or `None` for other URLs.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // Skip an optional host ("file://localhost/...").
    let rest = &rest[rest.find('/')?..];
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).and_then(|&b| hex(b)), bytes.get(i + 2).and_then(|&b| hex(b))) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    let path = String::from_utf8(out).ok()?;
    // "file:///C:/x.pdf" on Windows.
    let path = if cfg!(windows) { path.trim_start_matches('/').to_string() } else { path };
    Some(PathBuf::from(path))
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::file_url_path;
    use std::path::PathBuf;

    #[test]
    fn file_urls_become_paths() {
        assert_eq!(file_url_path("file:///Users/me/My%20Form.pdf"), Some(PathBuf::from("/Users/me/My Form.pdf")));
        assert_eq!(file_url_path("file://localhost/tmp/a.pdf"), Some(PathBuf::from("/tmp/a.pdf")));
        assert_eq!(file_url_path("file:///tmp/%C3%A9t%C3%A9.pdf"), Some(PathBuf::from("/tmp/été.pdf")));
        assert_eq!(file_url_path("https://example.com/a.pdf"), None);
    }
}
