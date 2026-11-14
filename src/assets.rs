//! Embedded SVG icons. GPUI renders SVGs as an alpha mask tinted with the
//! element's text colour, so the stroke colour here doesn't matter.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

macro_rules! icons {
    ($($name:literal => $body:literal),* $(,)?) => {
        const ICONS: &[(&str, &str)] = &[$(($name, $body)),*];
    };
}

icons! {
    "select" => r#"<path d="M6 4l13 7.5-5.5 1.6L11 19z"/>"#,
    "text" => r#"<path d="M5 7V5h14v2M12 5v14M9 19h6"/>"#,
    "calendar" => r#"<rect x="4" y="5.5" width="16" height="14.5" rx="2.5"/><path d="M4 10h16M8.5 3.5v4M15.5 3.5v4"/>"#,
    "check" => r#"<path d="M5 12.5l4.5 4.5L19 7.5"/>"#,
    "open" => r#"<path d="M4 18.5V6.5a1.5 1.5 0 0 1 1.5-1.5h3.8l2 2h6.2a1.5 1.5 0 0 1 1.5 1.5V10"/><path d="M4 18.5L6.6 11h14.4l-2.6 7.5z"/>"#,
    "save" => r#"<path d="M12 4v11M7.5 10.5L12 15l4.5-4.5M5 19.5h14"/>"#,
    "undo" => r#"<path d="M9 13.5L4.5 9 9 4.5"/><path d="M4.5 9H14a5.5 5.5 0 0 1 0 11h-3"/>"#,
    "plus" => r#"<path d="M12 5v14M5 12h14"/>"#,
    "minus" => r#"<path d="M5 12h14"/>"#,
    "trash" => r#"<path d="M4.5 7h15M10 11v5.5M14 11v5.5M6.5 7l.9 12a1.5 1.5 0 0 0 1.5 1.4h6.2a1.5 1.5 0 0 0 1.5-1.4l.9-12M9.5 7V4.5h5V7"/>"#,
    "file" => r#"<path d="M13.5 3.5H7.5A2 2 0 0 0 5.5 5.5v13a2 2 0 0 0 2 2h9a2 2 0 0 0 2-2V8.5z"/><path d="M13.5 3.5v5h5"/>"#,
    "signature" => r#"<path d="M3 16c2.5-.5 4-7 6.5-7 2 0 .2 6.5 2.8 6.5 2 0 2.4-3 4.4-3 1.6 0 1.8 2 4.3 2"/><path d="M3 20.5h18"/>"#,
    "close" => r#"<path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/>"#,
    "upload" => r#"<path d="M12 19.5V8M7.5 12.5L12 8l4.5 4.5M5 4.5h14"/>"#,
    "text-smaller" => r#"<path d="M3 18l4.5-11L12 18M4.6 14.5h5.8M15 13h6"/>"#,
    "grid" => r#"<rect x="4" y="4" width="6.5" height="6.5" rx="1.5"/><rect x="13.5" y="4" width="6.5" height="6.5" rx="1.5"/><rect x="4" y="13.5" width="6.5" height="6.5" rx="1.5"/><rect x="13.5" y="13.5" width="6.5" height="6.5" rx="1.5"/>"#,
    "revert" => r#"<path d="M4.5 12a7.5 7.5 0 1 0 2.2-5.3"/><path d="M4.5 4.5v4h4"/>"#,
    "reveal" => r#"<path d="M4 18.5V6.5a1.5 1.5 0 0 1 1.5-1.5h3.8l2 2h7.2A1.5 1.5 0 0 1 20 8.5v10a1.5 1.5 0 0 1-1.5 1.5h-13A1.5 1.5 0 0 1 4 18.5z"/><path d="M10 13.5h5M12.5 11l2.5 2.5-2.5 2.5"/>"#,
    "chevron-down" => r#"<path d="M7 10l5 5 5-5"/>"#,
    "redo" => r#"<path d="M15 13.5l4.5-4.5L15 4.5"/><path d="M19.5 9H10a5.5 5.5 0 0 0 0 11h3"/>"#,
    "copy" => r#"<rect x="8.5" y="8.5" width="11" height="11" rx="2"/><path d="M15.5 8.5V6.5a2 2 0 0 0-2-2h-7a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h2"/>"#,
    "layers" => r#"<path d="M12 4l8.5 4.5L12 13 3.5 8.5z"/><path d="M3.5 12.5L12 17l8.5-4.5"/><path d="M3.5 16.5L12 21l8.5-4.5"/>"#,
    "redact" => r#"<rect x="3.5" y="7.5" width="17" height="9" rx="1.5"/><path d="M8 16.5l5-9M13 16.5l5-9"/>"#,
    "watermark" => r#"<path d="M12 3.5c3 4 6 7.2 6 10.5a6 6 0 0 1-12 0c0-3.3 3-6.5 6-10.5z"/><path d="M9.2 14.5a2.8 2.8 0 0 0 2.8 2.8"/>"#,
    "window-minimize" => r#"<path d="M6 12h12"/>"#,
    "window-maximize" => r#"<rect x="6.5" y="6.5" width="11" height="11" rx="1"/>"#,
    "window-restore" => r#"<rect x="6" y="9" width="9" height="9" rx="1"/><path d="M9 9V7a1 1 0 0 1 1-1h7a1 1 0 0 1 1 1v7a1 1 0 0 1-1 1h-2"/>"#,
    "info" => r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 11v5.5M12 7.8v.2"/>"#,
    "search" => r#"<circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.5 15.5L20.5 20.5"/>"#,
    "lock" => r#"<rect x="5" y="10.5" width="14" height="10" rx="2"/><path d="M8.5 10.5V7.5a3.5 3.5 0 0 1 7 0v3"/>"#,
    "sidebar-left" => r#"<rect x="3.5" y="5" width="17" height="14" rx="2.5"/><path d="M9.5 5v14"/>"#,
    "sidebar-right" => r#"<rect x="3.5" y="5" width="17" height="14" rx="2.5"/><path d="M14.5 5v14"/>"#,
    "help" => r#"<circle cx="12" cy="12" r="8.5"/><path d="M9.6 9.6a2.5 2.5 0 1 1 3.6 2.3c-.8.4-1.2.9-1.2 1.8M12 16.6v.2"/>"#,
    "link" => r#"<path d="M10.5 6.5H7A1.5 1.5 0 0 0 5.5 8v9A1.5 1.5 0 0 0 7 18.5h9a1.5 1.5 0 0 0 1.5-1.5v-3.5"/><path d="M14 5.5h4.5V10M18.5 5.5L11.5 12.5"/>"#,
    "text-larger" => r#"<path d="M3 18l4.5-11L12 18M4.6 14.5h5.8M15 13h6M18 10v6"/>"#,
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let name = path.strip_prefix("icons/").and_then(|p| p.strip_suffix(".svg"));
        Ok(name
            .and_then(|name| ICONS.iter().find(|(n, _)| *n == name))
            .map(|(_, body)| {
                let svg = format!(
                    r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">{body}</svg>"#
                );
                Cow::Owned(svg.into_bytes())
            }))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(n, _)| SharedString::from(format!("icons/{n}.svg")))
            .filter(|p| p.starts_with(path))
            .collect())
    }
}
