mod editor;
mod model;
mod pdf;
mod signatures;

use gpui::{Application, Menu, MenuItem};

use editor::{AddSignature, Open, Paste, Quit, Save, Undo, ZoomIn, ZoomOut, ZoomReset};

fn main() {
    let initial = std::env::args_os().nth(1).map(std::path::PathBuf::from);

    Application::new().run(move |cx| {
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
                    MenuItem::separator(),
                    MenuItem::action("Add Signature Image…", AddSignature),
                ],
            },
            Menu {
                name: "Edit".into(),
                items: vec![MenuItem::action("Undo", Undo), MenuItem::action("Paste", Paste)],
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Zoom In", ZoomIn),
                    MenuItem::action("Zoom Out", ZoomOut),
                    MenuItem::action("Actual Size", ZoomReset),
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
