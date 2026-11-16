// Release builds on Windows are GUI apps: no console window behind them.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod about;
#[cfg(target_os = "macos")]
mod alloc;
mod assets;
mod clipboard;
mod editor;
mod fonts;
mod forms;
mod gestures;
mod model;
mod pdf;
mod print;
mod recents;
mod scan;
mod scrollbar;
mod search;
mod settings;
mod signatures;
mod ui;
mod updater;

use std::path::PathBuf;

use futures::StreamExt;
use gpui::Application;

use editor::{NewWindow, Quit};

/// See `alloc.rs`: what the pictures of pages take is given back when they go.
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOCATOR: alloc::Returning = alloc::Returning;

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
        cx.set_menus(editor::app_menus(self_updating, editor::draws_own_menus()));

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
