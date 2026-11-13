mod assets;
mod editor;
mod model;
mod pdf;
mod recents;
mod settings;
mod signatures;
mod ui;

use gpui::{Application, Menu, MenuItem};

use editor::{
    AddSignature, CloseDocument, Copy, Cut, DiscardChanges, Duplicate, Open, Paste, Quit, Redo, Save, Undo, ZoomFitPage,
    ZoomFitWidth, ZoomIn, ZoomOut, ZoomReset,
};

fn main() {
    let initial = std::env::args_os().nth(1).map(std::path::PathBuf::from);

    Application::new().with_assets(assets::Assets).run(move |cx| {
        editor::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![
            Menu {
                name: "PDF Signer".into(),
                items: vec![MenuItem::action("Quit PDF Signer", Quit)],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("Open…", Open),
                    MenuItem::action("Save As…", Save),
                    MenuItem::action("Discard Changes…", DiscardChanges),
                    MenuItem::action("Close Document", CloseDocument),
                    MenuItem::separator(),
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
                ],
            },
        ]);

        if let Err(e) = editor::open_editor(cx, initial.clone()) {
            eprintln!("failed to open window: {e:#}");
            cx.quit();
        }
        cx.activate(true);
    });
}
