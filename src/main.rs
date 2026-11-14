// Release builds on Windows are GUI apps: no console window behind them.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod about;
mod assets;
mod editor;
mod fonts;
mod forms;
mod gestures;
mod model;
mod pdf;
mod recents;
mod settings;
mod signatures;
mod ui;
mod updater;

use std::path::PathBuf;

use futures::StreamExt;
use gpui::{Application, Menu, MenuItem};

use editor::{
    About, AddPages, AddSignature, CheckForUpdates, CloseDocument, Copy, Cut, DiscardChanges, Duplicate, KeyboardShortcuts, Open, Paste, Quit, Redo, Save,
    SaveWithPassword, ToggleLeftSidebar, ToggleRightSidebar, Undo, VisitWebsite, ZoomFitPage, ZoomFitWidth, ZoomIn, ZoomOut, ZoomReset,
};

fn main() {
    let initial = std::env::args_os().nth(1).map(PathBuf::from);

    let app = Application::new().with_assets(assets::Assets);
    // PDFs opened from the Finder ("Open With", or dropped on the Dock icon) arrive
    // as file URLs, possibly before the window exists; queue them until it does.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();
    app.on_open_urls(move |urls| {
        let _ = open_tx.unbounded_send(urls);
    });

    app.run(move |cx| {
        editor::bind_keys(cx);
        let updater = updater::init(settings::Settings::load().check_for_updates, cx);
        let self_updating = updater.read(cx).support().is_ok();
        cx.on_action(|_: &Quit, cx| cx.quit());
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
                    MenuItem::action("Open…", Open),
                    MenuItem::action("Save As…", Save),
                    MenuItem::action("Save with Password…", SaveWithPassword),
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
            Menu {
                name: "Help".into(),
                items: vec![
                    MenuItem::action("Keyboard Shortcuts", KeyboardShortcuts),
                    MenuItem::separator(),
                    MenuItem::action("Visit Website", VisitWebsite),
                ],
            },
        ]);

        match editor::open_editor(cx, initial.clone()) {
            Ok(window) => {
                cx.spawn(async move |cx| {
                    while let Some(urls) = open_rx.next().await {
                        for path in urls.iter().filter_map(|u| file_url_path(u)) {
                            let _ = window.update(cx, |editor, window, cx| {
                                window.activate_window();
                                editor.request_open(Some(path), window, cx);
                            });
                        }
                    }
                })
                .detach();
            }
            Err(e) => {
                eprintln!("failed to open window: {e:#}");
                cx.quit();
            }
        }
        // A single-window app: closing the window quits.
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
