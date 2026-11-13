//! The main editor window: toolbar, signature library sidebar and page view.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::oneshot;

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, CursorStyle, Entity, ExternalPaths, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, PathBuilder,
    PathPromptOptions, Pixels, Point, PromptLevel, RenderImage, ScrollHandle, ScrollWheelEvent, SharedString,
    Subscription, Task, Window, actions, anchored, canvas, deferred, div, font, img, point, prelude::*, px, rgb, rgba,
    size,
};

use crate::model::{Item, ItemKind, LINE_HEIGHT, PageGeom, Signature, TextMetrics};
use crate::pdf::{self, LoadedPdf};
use crate::recents::{self, RecentEntry};
use crate::settings::{self, Settings};
use crate::signatures;
use crate::ui;

actions!(
    pdf_signer,
    [
        Open,
        Save,
        Undo,
        Redo,
        Copy,
        Cut,
        Paste,
        Duplicate,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ZoomFitWidth,
        ZoomFitPage,
        AddSignature,
        CloseDocument,
        DiscardChanges,
        Quit
    ]
);

const MIN_ZOOM: f32 = 0.3;
const MAX_ZOOM: f32 = 4.0;
const MAX_RENDER_SCALE: f32 = 6.0;
const DEFAULT_FONT_SIZE: f32 = 12.0;
const DEFAULT_SIG_WIDTH: f32 = 150.0;
const DEFAULT_SIG_MAX_HEIGHT: f32 = 55.0;
const DEFAULT_CHECK_SIZE: f32 = 14.0;
const SIDEBAR_W: f32 = 248.;
const THUMB_RAIL_W: f32 = 148.;
/// Thumbnail display width (points) and render width (pixels, 2× for Retina).
const THUMB_W: f32 = 100.;
const THUMB_RENDER_PX: f32 = 2. * THUMB_W;
/// Signature thumbnail box: sidebar minus list padding (2×12), card padding (2×12) and border (2).
const SIG_THUMB_W: f32 = SIDEBAR_W - 50.;
const SIG_THUMB_H: f32 = 58.;
/// Padding (in screen px) between an item's content and its selection border.
const ITEM_PAD: f32 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Select,
    Text,
    Date,
    Check,
}

/// How the zoom level is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoomMode {
    /// A fixed percentage.
    Fixed,
    /// Fit the widest page to the viewport width (recomputed on resize).
    FitWidth,
    /// Fit a whole page in the viewport.
    FitPage,
}

/// Payload when dragging a tool (Text / Date / Checkmark) from the sidebar.
#[derive(Clone, Copy)]
struct ToolDrag(Tool);

impl Render for ToolDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (icon_name, label) = tool_label(self.0);
        div()
            .font_family(".SystemUIFont")
            .flex()
            .items_center()
            .gap_1p5()
            .px_2p5()
            .py_1()
            .rounded_md()
            .bg(ui::accent())
            .text_color(gpui::white())
            .text_sm()
            .shadow(ui::lifted_shadow())
            .child(ui::icon(icon_name, gpui::white()).size(px(14.)))
            .child(label)
    }
}

fn tool_label(tool: Tool) -> (&'static str, &'static str) {
    match tool {
        Tool::Select => ("select", "Select"),
        Tool::Text => ("text", "Text"),
        Tool::Date => ("calendar", "Date"),
        Tool::Check => ("check", "Checkmark"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Move,
    Resize,
}

struct DragState {
    id: u64,
    mode: DragMode,
    start_mouse: Point<Pixels>,
    start: (f32, f32, f32, f32),
    snapshot: Vec<Item>,
    moved: bool,
}

struct PageView {
    image: Option<Arc<RenderImage>>,
    /// Small render for the page thumbnail rail.
    thumb: Option<Arc<RenderImage>>,
    rendered_scale: f32,
    /// Screen bounds of the page, captured during the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

struct OpenDoc {
    path: PathBuf,
    loaded: LoadedPdf,
    pages: Vec<PageView>,
    /// Where this document was last saved to (suggested on the next save).
    last_output: Option<PathBuf>,
}

/// Payload when dragging a signature from the sidebar.
#[derive(Clone)]
struct SignatureDrag {
    sig: Arc<Signature>,
    size: gpui::Size<Pixels>,
}

/// The floating preview shown while dragging a signature.
struct SignaturePreview {
    sig: Arc<Signature>,
    size: gpui::Size<Pixels>,
    cursor_offset: Point<Pixels>,
}

impl Render for SignaturePreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // The preview's origin sits at (mouse - cursor_offset); shift so the
        // image is centered on the cursor, which is where it will be dropped.
        div()
            .pl(self.cursor_offset.x - self.size.width / 2.)
            .pt(self.cursor_offset.y - self.size.height / 2.)
            .child(
                div()
                    .w(self.size.width)
                    .h(self.size.height)
                    .rounded(px(3.))
                    .border_1()
                    .border_dashed()
                    .border_color(ui::accent())
                    .bg(ui::accent().opacity(0.06))
                    .shadow(ui::lifted_shadow())
                    .child(img(self.sig.path.clone()).size_full().object_fit(ObjectFit::Fill)),
            )
    }
}

pub struct Editor {
    focus: FocusHandle,
    doc: Option<OpenDoc>,
    signatures: Vec<Arc<Signature>>,
    items: Vec<Item>,
    /// The fields as of the last open/save; "Discard changes" reverts to this.
    saved_items: Vec<Item>,
    /// Recently saved documents, shown in the gallery when no document is open.
    recents: Vec<RecentEntry>,
    next_id: u64,
    selected: Option<u64>,
    editing: Option<u64>,
    typing_checkpointed: bool,
    tool: Tool,
    zoom: f32,
    drag: Option<DragState>,
    history: Vec<Vec<Item>>,
    /// Undone states, for redo. Cleared by any new edit.
    future: Vec<Vec<Item>>,
    /// A copied field, pasted with ⌘V.
    field_clipboard: Option<Item>,
    zoom_mode: ZoomMode,
    zoom_menu_open: bool,
    date_menu_open: bool,
    /// Set when a click outside closed a menu, so that same click can't reopen it.
    menu_dismissed: bool,
    settings: Settings,
    /// Scroll state of the page list (children are the pages, in order).
    pages_scroll: ScrollHandle,
    status: SharedString,
    metrics: TextMetrics,
    dirty: bool,
    /// Bumped on every edit, so a finished save knows whether edits happened meanwhile.
    revision: u64,
    font_size: f32,
    render_task: Option<Task<()>>,
    thumb_task: Option<Task<()>>,
    _appearance_observer: Subscription,
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Editor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);

        // Follow the system light/dark setting, including live changes.
        ui::set_appearance(window.appearance());
        let appearance_observer = cx.observe_window_appearance(window, |_, window, cx| {
            ui::set_appearance(window.appearance());
            cx.notify();
        });

        // Measure the UI font so text lands on the same baseline in the PDF.
        let ts = window.text_system();
        let font_id = ts.resolve_font(&font("Helvetica"));
        let a = ts.ascent(font_id, px(1000.)) / px(1000.);
        let d = ts.descent(font_id, px(1000.)) / px(1000.);
        let metrics = TextMetrics {
            baseline_ratio: (LINE_HEIGHT - a - d) / 2. + a,
        };

        Self {
            focus,
            doc: None,
            signatures: signatures::load_library(),
            items: Vec::new(),
            saved_items: Vec::new(),
            recents: recents::load(),
            next_id: 1,
            selected: None,
            editing: None,
            typing_checkpointed: false,
            tool: Tool::Select,
            zoom: 1.0,
            drag: None,
            history: Vec::new(),
            future: Vec::new(),
            field_clipboard: None,
            zoom_mode: ZoomMode::FitWidth,
            zoom_menu_open: false,
            date_menu_open: false,
            menu_dismissed: false,
            settings: Settings::load(),
            pages_scroll: ScrollHandle::new(),
            status: "Ready".into(),
            metrics,
            dirty: false,
            revision: 0,
            font_size: DEFAULT_FONT_SIZE,
            render_task: None,
            thumb_task: None,
            _appearance_observer: appearance_observer,
        }
    }

    // -----------------------------------------------------------------------
    // Document lifecycle
    // -----------------------------------------------------------------------

    /// Ask before throwing away unsaved fields. `None` means there's nothing to lose.
    fn confirm_discard(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<oneshot::Receiver<usize>> {
        self.dirty.then(|| {
            window.prompt(
                PromptLevel::Warning,
                "Discard unsaved changes?",
                Some("Fields you added or changed since the last save will be lost."),
                &["Discard", "Cancel"],
                cx,
            )
        })
    }

    fn request_open(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let confirm = self.confirm_discard(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            if let Some(answer) = confirm {
                if answer.await.ok() != Some(0) {
                    return;
                }
            }
            let path = match path {
                Some(p) => p,
                None => {
                    let picker = cx
                        .update(|_, cx| {
                            cx.prompt_for_paths(PathPromptOptions {
                                files: true,
                                directories: false,
                                multiple: false,
                                prompt: Some("Open PDF".into()),
                            })
                        })
                        .ok();
                    let Some(picker) = picker else { return };
                    match picker.await {
                        Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                        _ => return,
                    }
                }
            };
            this.update_in(cx, |this, window, cx| this.open_path(path, window, cx)).ok();
        })
        .detach();
    }

    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let loaded = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(pdf::load);
        let loaded = match loaded {
            Ok(l) => l,
            Err(e) => {
                self.show_error(&format!("Couldn't open {}", file_name(&path)), &e.to_string(), window, cx);
                return false;
            }
        };

        self.release_document(window, cx);

        let pages = loaded
            .pages
            .iter()
            .map(|_| PageView {
                image: None,
                thumb: None,
                rendered_scale: 0.0,
                bounds: Rc::new(Cell::new(Bounds::default())),
            })
            .collect();
        self.status = format!("Opened {} ({} pages)", file_name(&path), loaded.pages.len()).into();
        window.set_window_title(&format!("PDF Signer — {}", file_name(&path)));
        self.doc = Some(OpenDoc {
            path,
            loaded,
            pages,
            last_output: None,
        });
        self.items.clear();
        self.saved_items.clear();
        self.history.clear();
        self.future.clear();
        self.selected = None;
        self.editing = None;
        self.drag = None;
        self.dirty = false;
        self.render_task = None;
        self.pages_scroll.set_offset(point(px(0.), px(0.)));
        self.ensure_rendered(window, cx);
        self.render_thumbnails(window, cx);
        cx.notify();
        true
    }

    /// Drop the open document and free its page bitmaps.
    fn release_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(old) = self.doc.take() {
            for page in old.pages {
                for image in [page.image, page.thumb].into_iter().flatten() {
                    cx.drop_image(image, Some(window));
                }
            }
        }
        self.render_task = None;
        self.thumb_task = None;
    }

    /// Close the document and go back to the gallery (asking first if there are unsaved changes).
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        self.finish_editing(cx);
        let confirm = self.confirm_discard(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            if let Some(answer) = confirm {
                if answer.await.ok() != Some(0) {
                    return;
                }
            }
            this.update_in(cx, |this, window, cx| this.close_document(window, cx)).ok();
        })
        .detach();
    }

    fn close_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release_document(window, cx);
        self.items.clear();
        self.saved_items.clear();
        self.history.clear();
        self.future.clear();
        self.selected = None;
        self.editing = None;
        self.drag = None;
        self.dirty = false;
        self.tool = Tool::Select;
        self.status = "Ready".into();
        window.set_window_title("PDF Signer");
        cx.notify();
    }

    /// Revert to the fields as they were at the last open/save (undoable).
    fn request_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        let Some(confirm) = self.confirm_discard(window, cx) else { return };
        cx.spawn_in(window, async move |this, cx| {
            if confirm.await.ok() == Some(0) {
                this.update(cx, |this, cx| this.discard_changes(cx)).ok();
            }
        })
        .detach();
    }

    fn discard_changes(&mut self, cx: &mut Context<Self>) {
        if !self.dirty {
            return;
        }
        self.checkpoint();
        self.items = self.saved_items.clone();
        self.selected = None;
        self.editing = None;
        self.drag = None;
        self.dirty = false;
        self.status = "Changes discarded. Press ⌘Z to bring them back.".into();
        cx.notify();
    }

    /// Reopen a document from the gallery with its fields still editable.
    fn open_recent(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.recents.iter().find(|e| e.id == id).cloned() else { return };
        if entry.original.exists() {
            if !self.open_path(entry.original.clone(), window, cx) {
                return;
            }
            let page_count = self.doc.as_ref().map_or(0, |d| d.pages.len());
            let mut next_id = self.next_id;
            self.items = recents::restore_items(&entry, page_count, || {
                next_id += 1;
                next_id
            });
            self.next_id = next_id + 1;
            self.saved_items = self.items.clone();
            if let Some(doc) = &mut self.doc {
                doc.last_output = Some(entry.output.clone());
            }
            let n = self.items.len();
            self.status = format!(
                "Reopened {} with {n} editable field{}",
                file_name(&entry.original),
                if n == 1 { "" } else { "s" }
            )
            .into();
            cx.notify();
        } else if entry.output.exists() {
            if self.open_path(entry.output.clone(), window, cx) {
                self.status = format!(
                    "The original ({}) has moved, so the saved copy was opened instead. Its fields are part of the page now.",
                    entry.original.display()
                )
                .into();
            }
        } else {
            self.show_error(
                "Couldn't find this document",
                &format!(
                    "Neither {} nor the saved copy {} exists any more. You can remove it from Recents.",
                    entry.original.display(),
                    entry.output.display()
                ),
                window,
                cx,
            );
        }
    }

    fn remove_recent(&mut self, id: &str, cx: &mut Context<Self>) {
        match recents::remove(self.recents.clone(), id) {
            Ok(entries) => self.recents = entries,
            Err(e) => self.status = format!("Couldn't update recents: {e:#}").into(),
        }
        cx.notify();
    }

    /// Render the small page images for the thumbnail rail (once per document).
    fn render_thumbnails(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
        let pdf = doc.loaded.pdf.clone();
        let path = doc.path.clone();
        let scales: Vec<f32> = doc.loaded.pages.iter().map(|p| THUMB_RENDER_PX / p.width.max(1.)).collect();
        self.thumb_task = Some(cx.spawn_in(window, async move |this, cx| {
            for (index, scale) in scales.into_iter().enumerate() {
                let pdf = pdf.clone();
                let rendered = cx
                    .background_executor()
                    .spawn(async move { pdf::render_page(&pdf, index, scale) })
                    .await;
                let Some(image) = rendered.and_then(to_render_image) else { continue };
                let ok = this.update(cx, |this, cx| {
                    let Some(doc) = this.doc.as_mut().filter(|d| d.path == path) else { return false };
                    doc.pages[index].thumb = Some(image);
                    cx.notify();
                    true
                });
                if !matches!(ok, Ok(true)) {
                    return;
                }
            }
        }));
    }

    /// (Re-)render pages whose bitmap is too low-resolution for the current zoom.
    fn ensure_rendered(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
        let wanted = (self.zoom * window.scale_factor()).clamp(1.0, MAX_RENDER_SCALE);
        let stale: Vec<usize> = doc
            .pages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.rendered_scale < wanted * 0.95)
            .map(|(i, _)| i)
            .collect();
        if stale.is_empty() {
            return;
        }
        // Render a bit above what's needed so small zoom steps don't re-render.
        let scale = (wanted * 1.25).min(MAX_RENDER_SCALE);
        let pdf = doc.loaded.pdf.clone();
        let path = doc.path.clone();
        self.render_task = Some(cx.spawn_in(window, async move |this, cx| {
            for index in stale {
                let pdf = pdf.clone();
                let rendered = cx
                    .background_executor()
                    .spawn(async move { pdf::render_page(&pdf, index, scale) })
                    .await;
                let Some(image) = rendered.and_then(to_render_image) else { continue };
                let ok = this.update_in(cx, |this, window, cx| {
                    let Some(doc) = this.doc.as_mut().filter(|d| d.path == path) else { return false };
                    let page = &mut doc.pages[index];
                    if let Some(old) = page.image.replace(image) {
                        cx.drop_image(old, Some(window));
                    }
                    page.rendered_scale = scale;
                    cx.notify();
                    true
                });
                if !matches!(ok, Ok(true)) {
                    return;
                }
            }
        }));
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        let Some(doc) = &self.doc else {
            self.status = "Nothing to save — open a PDF first.".into();
            cx.notify();
            return;
        };
        // Suggest the previous output if there is one, otherwise "<name>-signed.pdf" next to the original.
        let (dir, suggested) = match &doc.last_output {
            Some(out) => (
                out.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")),
                file_name(out),
            ),
            None => (
                doc.path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")),
                format!("{}-signed.pdf", doc.path.file_stem().and_then(|s| s.to_str()).unwrap_or("document")),
            ),
        };
        let picker = cx.prompt_for_new_path(&dir, Some(&suggested));

        let original = doc.path.clone();
        let bytes = doc.loaded.bytes.clone();
        let pages: Vec<PageGeom> = doc.loaded.pages.clone();
        let page_count = pages.len();
        let items = self.items.clone();
        let metrics = self.metrics;
        let revision = self.revision;

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picker.await else { return };
            let target = path.clone();
            let saved_items = items.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let out = pdf::write(&bytes, &pages, &items, metrics)?;
                    std::fs::write(&target, &out)?;
                    anyhow::Ok(recents::make_thumbnail(out))
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(thumbnail) => {
                    // Only clear "Edited" if nothing changed while the save was running.
                    if this.revision == revision && this.doc.as_ref().is_some_and(|d| d.path == original) {
                        this.dirty = false;
                    }
                    this.saved_items = saved_items.clone();
                    if let Some(doc) = this.doc.as_mut().filter(|d| d.path == original) {
                        doc.last_output = Some(path.clone());
                    }
                    match recents::record(this.recents.clone(), &original, &path, page_count, &saved_items, thumbnail) {
                        Ok(entries) => this.recents = entries,
                        Err(e) => eprintln!("could not update recents: {e:#}"),
                    }
                    this.status = format!("Saved {}", path.display()).into();
                    cx.notify();
                }
                Err(e) => this.show_error("Couldn't save the PDF", &format!("{e:#}"), window, cx),
            })
            .ok();
        })
        .detach();
    }

    fn show_error(&mut self, title: &str, detail: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.status = format!("{title}: {detail}").into();
        let _ = window.prompt(PromptLevel::Critical, title, Some(detail), &["OK"], cx);
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Signatures
    // -----------------------------------------------------------------------

    fn add_signature_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add Signature".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picker.await else { return };
            this.update_in(cx, |this, window, cx| {
                for path in paths {
                    this.import_signature(&path, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn import_signature(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) -> Option<Arc<Signature>> {
        if !signatures::is_image(path) {
            self.show_error("Unsupported file", "Signatures must be PNG or JPEG images.", window, cx);
            return None;
        }
        match signatures::import(path) {
            Ok(sig) => {
                self.signatures.push(sig.clone());
                self.status = "Signature added — drag it onto the page.".into();
                cx.notify();
                Some(sig)
            }
            Err(e) => {
                self.show_error("Couldn't import signature", &format!("{e:#}"), window, cx);
                None
            }
        }
    }

    fn remove_signature(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.signatures.len() {
            let sig = self.signatures.remove(index);
            // Placed copies keep working until saved; the file is only needed at save time,
            // so only delete it if nothing on the page uses it.
            let in_use = self
                .items
                .iter()
                .any(|i| matches!(&i.kind, ItemKind::Image { sig: s } if Arc::ptr_eq(s, &sig)));
            if !in_use {
                signatures::delete(&sig);
            }
            cx.notify();
        }
    }

    fn default_sig_size(sig: &Signature) -> (f32, f32) {
        let aspect = sig.aspect().max(0.05);
        let (mut w, mut h) = (DEFAULT_SIG_WIDTH, DEFAULT_SIG_WIDTH / aspect);
        if h > DEFAULT_SIG_MAX_HEIGHT {
            h = DEFAULT_SIG_MAX_HEIGHT;
            w = h * aspect;
        }
        (w, h)
    }

    /// Which page (and where on it, in points) is under the given window position.
    fn page_at(&self, position: Point<Pixels>) -> Option<(usize, f32, f32)> {
        let doc = self.doc.as_ref()?;
        doc.pages.iter().enumerate().find_map(|(i, p)| {
            let b = p.bounds.get();
            b.contains(&position).then(|| {
                let local = position - b.origin;
                (i, local.x / px(self.zoom), local.y / px(self.zoom))
            })
        })
    }

    fn place_signature(&mut self, sig: Arc<Signature>, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some((page, x, y)) = self.page_at(position) else { return false };
        let (w, h) = Self::default_sig_size(&sig);
        self.add_item(page, x - w / 2., y - h / 2., w, h, ItemKind::Image { sig }, cx);
        true
    }

    fn handle_external_drop(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        let position = window.mouse_position();
        if let Some(pdf_path) = paths.paths().iter().find(|p| signatures::is_pdf(p)) {
            self.request_open(Some(pdf_path.clone()), window, cx);
            return;
        }
        for path in paths.paths() {
            if let Some(sig) = self.import_signature(path, window, cx) {
                self.place_signature(sig, position, cx);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Items
    // -----------------------------------------------------------------------

    /// Note an edit: shows "Edited" and invalidates any save already in flight.
    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.revision += 1;
    }

    fn checkpoint(&mut self) {
        let snapshot = self.items.clone();
        self.push_history(snapshot);
    }

    /// Record a state to undo back to. Any new edit invalidates the redo stack.
    fn push_history(&mut self, snapshot: Vec<Item>) {
        self.history.push(snapshot);
        if self.history.len() > 200 {
            self.history.remove(0);
        }
        self.future.clear();
        self.mark_dirty();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        if let Some(items) = self.history.pop() {
            let current = std::mem::replace(&mut self.items, items);
            self.future.push(current);
            self.after_history_jump(cx);
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        if let Some(items) = self.future.pop() {
            let current = std::mem::replace(&mut self.items, items);
            self.history.push(current);
            self.after_history_jump(cx);
        }
    }

    fn after_history_jump(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.editing = None;
        self.drag = None;
        // mark_dirty without clearing the redo stack.
        self.dirty = true;
        self.revision += 1;
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Copy / paste
    // -----------------------------------------------------------------------

    fn selected_item(&self) -> Option<&Item> {
        let id = self.selected?;
        self.items.iter().find(|i| i.id == id)
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.selected_item().cloned() else { return };
        // Text fields also go to the system clipboard, so they can be pasted elsewhere.
        if let ItemKind::Text { text, .. } = &item.kind {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
        }
        self.field_clipboard = Some(item);
        self.status = "Field copied. Press ⌘V to paste it on any page.".into();
        cx.notify();
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        let Some(item) = self.selected_item().cloned() else { return };
        self.field_clipboard = Some(item.clone());
        self.delete_item(item.id, cx);
        self.status = "Field cut. Press ⌘V to paste it.".into();
    }

    /// ⌘V: paste text while typing, otherwise paste the copied field.
    fn paste(&mut self, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                let text = text.replace("\r\n", "\n");
                self.edit_text(cx, |t| t.push_str(&text));
            }
            return;
        }
        let Some(source) = self.field_clipboard.clone() else { return };
        let page = self.current_page().unwrap_or(source.page);
        let id = self.insert_copy(&source, page);
        // Paste again → cascade further rather than stacking exactly on top.
        self.field_clipboard = self.items.iter().find(|i| i.id == id).cloned();
        cx.notify();
    }

    fn duplicate(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        let Some(source) = self.selected_item().cloned() else { return };
        self.insert_copy(&source, source.page);
        cx.notify();
    }

    /// Insert a copy of `source` on `page`: same spot on another page, or
    /// offset slightly on the same page so it doesn't hide the original.
    fn insert_copy(&mut self, source: &Item, page: usize) -> u64 {
        self.checkpoint();
        let mut item = source.clone();
        item.id = self.next_id;
        self.next_id += 1;
        if page == source.page && self.items.iter().any(|i| i.page == page && i.x == item.x && i.y == item.y) {
            item.x += 12.;
            item.y += 12.;
        }
        item.page = page;
        self.clamp_item(&mut item);
        let id = item.id;
        self.items.push(item);
        self.selected = Some(id);
        self.editing = None;
        id
    }

    /// Put a copy of the selected field at the same position on every other page.
    fn copy_to_all_pages(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        let Some(source) = self.selected_item().cloned() else { return };
        let page_count = self.doc.as_ref().map_or(0, |d| d.pages.len());
        if page_count < 2 {
            return;
        }
        self.checkpoint();
        for page in (0..page_count).filter(|&p| p != source.page) {
            let mut item = source.clone();
            item.id = self.next_id;
            self.next_id += 1;
            item.page = page;
            self.clamp_item(&mut item);
            self.items.push(item);
        }
        self.status = format!("Copied to {} other page{}", page_count - 1, if page_count == 2 { "" } else { "s" }).into();
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Pages & zoom
    // -----------------------------------------------------------------------

    /// The page occupying most of the viewport (from the bounds captured at the last paint).
    fn current_page(&self) -> Option<usize> {
        let doc = self.doc.as_ref()?;
        let viewport = self.pages_scroll.bounds();
        if viewport.size.height <= px(0.) {
            return Some(0);
        }
        doc.pages
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let b = p.bounds.get();
                let visible = (b.bottom().min(viewport.bottom()) - b.top().max(viewport.top())).max(px(0.));
                (i, visible)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
    }

    fn scroll_to_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
        let Some(view) = doc.pages.get(page) else { return };
        let viewport = self.pages_scroll.bounds();
        let offset = self.pages_scroll.offset();
        // Bring the page's top edge to just below the top of the viewport.
        let delta = view.bounds.get().top() - viewport.top() - px(24.);
        self.pages_scroll.set_offset(point(offset.x, offset.y - delta));
        cx.notify();
    }

    /// The zoom that makes pages fit the viewport, for the fit modes.
    fn fit_zoom(&self, mode: ZoomMode) -> Option<f32> {
        let doc = self.doc.as_ref()?;
        let viewport = self.pages_scroll.bounds();
        if viewport.size.width <= px(0.) {
            return None;
        }
        // Matches the page list's padding (px_8 / py_8) and the page label below each page.
        let avail_w = viewport.size.width / px(1.) - 2. * 32. - 8.;
        let avail_h = viewport.size.height / px(1.) - 2. * 32. - 26.;
        let widest = doc.loaded.pages.iter().map(|p| p.width).fold(1., f32::max);
        let zoom = match mode {
            ZoomMode::Fixed => return None,
            ZoomMode::FitWidth => avail_w / widest,
            ZoomMode::FitPage => {
                let page = &doc.loaded.pages[self.current_page().unwrap_or(0)];
                (avail_w / page.width).min(avail_h / page.height)
            }
        };
        Some(zoom.clamp(MIN_ZOOM, MAX_ZOOM))
    }

    fn set_zoom_mode(&mut self, mode: ZoomMode, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_mode = mode;
        self.zoom_menu_open = false;
        if let Some(zoom) = self.fit_zoom(mode) {
            self.zoom = zoom;
            self.ensure_rendered(window, cx);
        }
        cx.notify();
    }

    fn set_date_format(&mut self, format: &str, cx: &mut Context<Self>) {
        self.settings.date_format = format.to_string();
        self.date_menu_open = false;
        if let Err(e) = self.settings.save() {
            self.status = format!("Couldn't save settings: {e:#}").into();
        }
        cx.notify();
    }

    fn page_geom(&self, page: usize) -> Option<&PageGeom> {
        self.doc.as_ref()?.loaded.pages.get(page)
    }

    fn clamp_item(&self, item: &mut Item) {
        if let Some(g) = self.page_geom(item.page) {
            item.x = item.x.clamp(0., (g.width - item.w).max(0.));
            item.y = item.y.clamp(0., (g.height - item.h).max(0.));
        }
    }

    fn add_item(&mut self, page: usize, x: f32, y: f32, w: f32, h: f32, kind: ItemKind, cx: &mut Context<Self>) -> u64 {
        self.checkpoint();
        let id = self.next_id;
        self.next_id += 1;
        let is_text = matches!(kind, ItemKind::Text { .. });
        let mut item = Item { id, page, x, y, w, h, kind };
        self.clamp_item(&mut item);
        self.items.push(item);
        self.selected = Some(id);
        self.editing = is_text.then_some(id);
        // The checkpoint above already covers the first keystrokes.
        self.typing_checkpointed = true;
        cx.notify();
        id
    }

    fn item_mut(&mut self, id: u64) -> Option<&mut Item> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    fn delete_item(&mut self, id: u64, cx: &mut Context<Self>) {
        self.checkpoint();
        self.items.retain(|i| i.id != id);
        if self.selected == Some(id) {
            self.selected = None;
        }
        if self.editing == Some(id) {
            self.editing = None;
        }
        cx.notify();
    }

    /// Stop editing the current text box, dropping it if it's empty.
    fn finish_editing(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.editing.take() {
            let empty = self
                .items
                .iter()
                .any(|i| i.id == id && matches!(&i.kind, ItemKind::Text { text, .. } if text.trim().is_empty()));
            if empty {
                self.items.retain(|i| i.id != id);
                if self.selected == Some(id) {
                    self.selected = None;
                }
            }
            cx.notify();
        }
    }

    fn edit_text(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut String)) {
        let Some(id) = self.editing else { return };
        if !self.typing_checkpointed {
            self.checkpoint();
            self.typing_checkpointed = true;
        }
        self.mark_dirty();
        let Some(item) = self.item_mut(id) else { return };
        let ItemKind::Text { text, font_size } = &mut item.kind else { return };
        f(text);
        // Rough size estimate (used for clamping to the page only).
        let lines = text.split('\n');
        let longest = lines.clone().map(|l| l.chars().count()).max().unwrap_or(0).max(1);
        item.w = longest as f32 * *font_size * 0.55;
        item.h = lines.count() as f32 * *font_size * LINE_HEIGHT;
        cx.notify();
    }

    fn set_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let target = self.selected;
        let new_size = (self.font_size + delta).clamp(6., 72.);
        if let Some(item) = target.and_then(|id| self.items.iter().find(|i| i.id == id)) {
            if let ItemKind::Text { font_size, .. } = item.kind {
                let size = (font_size + delta).clamp(6., 72.);
                self.checkpoint();
                if let Some(ItemKind::Text { font_size, .. }) = target.and_then(|id| self.item_mut(id)).map(|i| &mut i.kind) {
                    *font_size = size;
                }
                self.font_size = size;
                cx.notify();
                return;
            }
        }
        self.font_size = new_size;
        cx.notify();
    }

    fn set_zoom(&mut self, zoom: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_mode = ZoomMode::Fixed;
        self.zoom_menu_open = false;
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.ensure_rendered(window, cx);
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Input handlers
    // -----------------------------------------------------------------------

    fn page_mouse_down(&mut self, _page: usize, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.finish_editing(cx);
        self.close_menus();
        let Some((page, x, y)) = self.page_at(event.position) else { return };
        if self.tool == Tool::Select {
            self.selected = None;
        } else {
            self.place_tool(self.tool, page, x, y, cx);
            self.tool = Tool::Select;
        }
        cx.notify();
    }

    /// Create the field for `tool` centred (vertically, for text) on page point (x, y).
    fn place_tool(&mut self, tool: Tool, page: usize, x: f32, y: f32, cx: &mut Context<Self>) {
        match tool {
            Tool::Select => {}
            Tool::Text | Tool::Date => {
                let text = if tool == Tool::Date { self.settings.today() } else { String::new() };
                let fs = self.font_size;
                let line_h = fs * LINE_HEIGHT;
                let w = text.chars().count().max(1) as f32 * fs * 0.55;
                let id = self.add_item(page, x, y - line_h / 2., w, line_h, ItemKind::Text { text, font_size: fs }, cx);
                // A date is complete as placed; don't drop the user into typing.
                if tool == Tool::Date {
                    self.editing = None;
                    self.selected = Some(id);
                }
            }
            Tool::Check => {
                let s = DEFAULT_CHECK_SIZE;
                self.add_item(page, x - s / 2., y - s / 2., s, s, ItemKind::Check, cx);
            }
        }
    }

    fn drop_tool(&mut self, tool: Tool, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        match self.page_at(window.mouse_position()) {
            Some((page, x, y)) => {
                self.place_tool(tool, page, x, y, cx);
                self.tool = Tool::Select;
            }
            None => self.status = "Drop it onto a page.".into(),
        }
        cx.notify();
    }

    fn close_menus(&mut self) {
        self.zoom_menu_open = false;
        self.date_menu_open = false;
    }

    fn item_mouse_down(&mut self, id: u64, mode: DragMode, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.focus(&self.focus);
        if self.editing != Some(id) {
            self.finish_editing(cx);
        }
        let Some(item) = self.items.iter().find(|i| i.id == id) else { return };
        let is_text = item.is_text();
        self.drag = Some(DragState {
            id,
            mode,
            start_mouse: event.position,
            start: (item.x, item.y, item.w, item.h),
            snapshot: self.items.clone(),
            moved: false,
        });
        self.selected = Some(id);
        if is_text && self.editing != Some(id) {
            self.editing = Some(id);
            self.typing_checkpointed = false;
        }
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let delta = event.position - drag.start_mouse;
        let (dx, dy) = (delta.x / px(self.zoom), delta.y / px(self.zoom));
        if !drag.moved && dx.abs() < 1.0 && dy.abs() < 1.0 {
            return;
        }
        drag.moved = true;
        let (id, mode, (x0, y0, w0, h0)) = (drag.id, drag.mode, drag.start);
        let Some(mut item) = self.items.iter().find(|i| i.id == id).cloned() else { return };
        match mode {
            DragMode::Move => {
                item.x = x0 + dx;
                item.y = y0 + dy;
            }
            DragMode::Resize => {
                let aspect = w0 / h0.max(0.01);
                let w = (w0 + dx).max(8.);
                item.w = w;
                item.h = w / aspect;
                if let Some(g) = self.page_geom(item.page) {
                    let max_w = (g.width - item.x).min((g.height - item.y) * aspect);
                    if item.w > max_w {
                        item.w = max_w;
                        item.h = max_w / aspect;
                    }
                }
            }
        }
        self.clamp_item(&mut item);
        if let Some(slot) = self.item_mut(id) {
            *slot = item;
        }
        cx.notify();
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.menu_dismissed = false;
        if let Some(drag) = self.drag.take() {
            if drag.moved {
                self.push_history(drag.snapshot);
            }
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let shortcut = ks.modifiers.platform || ks.modifiers.control;

        if self.editing.is_some() {
            match ks.key.as_str() {
                "escape" => self.finish_editing(cx),
                "backspace" => self.edit_text(cx, |t| {
                    t.pop();
                }),
                "enter" => self.edit_text(cx, |t| t.push('\n')),
                "tab" => {}
                _ if !shortcut => {
                    if let Some(ch) = ks.key_char.clone().filter(|c| !c.chars().any(char::is_control)) {
                        self.edit_text(cx, |t| t.push_str(&ch));
                    } else {
                        return;
                    }
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }

        if ks.key == "escape" && (self.zoom_menu_open || self.date_menu_open) {
            self.close_menus();
            cx.notify();
            return;
        }
        let Some(id) = self.selected else {
            if ks.key == "escape" {
                self.tool = Tool::Select;
                cx.notify();
            }
            return;
        };
        let step = if ks.modifiers.shift { 10. } else { 1. };
        let nudge = match ks.key.as_str() {
            "backspace" | "delete" => {
                self.delete_item(id, cx);
                return;
            }
            "escape" => {
                self.selected = None;
                cx.notify();
                return;
            }
            "left" => (-step, 0.),
            "right" => (step, 0.),
            "up" => (0., -step),
            "down" => (0., step),
            _ => return,
        };
        self.checkpoint();
        if let Some(mut item) = self.items.iter().find(|i| i.id == id).cloned() {
            item.x += nudge.0;
            item.y += nudge.1;
            self.clamp_item(&mut item);
            if let Some(slot) = self.item_mut(id) {
                *slot = item;
            }
        }
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let has_doc = self.doc.is_some();
        let (title, subtitle) = match &self.doc {
            Some(doc) => {
                let n = doc.pages.len();
                let pages = format!("{n} page{}", if n == 1 { "" } else { "s" });
                (file_name(&doc.path), if self.dirty { format!("{pages} · Edited") } else { pages })
            }
            None => ("PDF Signer".to_string(), "No document".to_string()),
        };

        div()
            .id("titlebar")
            .flex()
            .flex_none()
            .items_center()
            .gap_3()
            .h(px(52.))
            .pl(px(84.))
            .pr_3()
            .bg(ui::chrome_bg())
            .border_b_1()
            .border_color(ui::border())
            .on_click(|e: &ClickEvent, window, _| {
                if e.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            // Left: document identity.
            .child(
                div()
                    .flex()
                    .flex_1()
                    .flex_basis(px(0.))
                    .min_w_0()
                    .items_center()
                    .gap_2p5()
                    .when(has_doc, |d| {
                        d.child(
                            ui::icon_button("home", "grid")
                                .tooltip(ui::tooltip("All documents", Some("⌘W")))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.request_close(window, cx))),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .size(px(30.))
                            .rounded_md()
                            .bg(ui::accent_soft())
                            .child(ui::icon(if has_doc { "file" } else { "signature" }, ui::accent())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(ui::text_primary())
                                    .truncate()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .text_xs()
                                    .text_color(ui::text_tertiary())
                                    .when(self.dirty, |d| {
                                        d.child(div().size(px(6.)).rounded_full().bg(ui::warning()))
                                    })
                                    .child(subtitle),
                            ),
                    ),
            )
            // Right: view + file actions.
            .child(
                div()
                    .flex()
                    .flex_1()
                    .flex_basis(px(0.))
                    .items_center()
                    .justify_end()
                    .gap_1()
                    .when(has_doc, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .h(px(30.))
                                .rounded_md()
                                .border_1()
                                .border_color(ui::border_strong())
                                .bg(ui::surface())
                                .shadow(ui::card_shadow())
                                .child(
                                    ui::icon_button("zoom-out", "minus")
                                        .size(px(28.))
                                        .tooltip(ui::tooltip("Zoom out", Some("⌘−")))
                                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                            this.set_zoom(this.zoom / 1.2, window, cx)
                                        })),
                                )
                                .child(
                                    div()
                                        .id("zoom-menu-trigger")
                                        .relative()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .gap_0p5()
                                        .h_full()
                                        .min_w(px(62.))
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(ui::text_secondary())
                                        .cursor(CursorStyle::PointingHand)
                                        .hover(|s| s.text_color(ui::text_primary()))
                                        .child(format!("{:.0}%", self.zoom * 100.))
                                        .child(ui::icon("chevron-down", ui::text_tertiary()).size(px(12.)))
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            if std::mem::take(&mut this.menu_dismissed) {
                                                return;
                                            }
                                            this.date_menu_open = false;
                                            this.zoom_menu_open = !this.zoom_menu_open;
                                            cx.notify();
                                        }))
                                        .when(self.zoom_menu_open, |d| d.child(self.render_zoom_menu(cx))),
                                )
                                .child(
                                    ui::icon_button("zoom-in", "plus")
                                        .size(px(28.))
                                        .tooltip(ui::tooltip("Zoom in", Some("⌘+")))
                                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                            this.set_zoom(this.zoom * 1.2, window, cx)
                                        })),
                                ),
                        )
                        .child(ui::vertical_divider())
                        .child(
                            ui::icon_button("undo", "undo")
                                .tooltip(ui::tooltip("Undo", Some("⌘Z")))
                                .when(self.history.is_empty(), |b| b.opacity(0.4))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.undo(cx))),
                        )
                        .child(
                            ui::icon_button("redo", "redo")
                                .tooltip(ui::tooltip("Redo", Some("⇧⌘Z")))
                                .when(self.future.is_empty(), |b| b.opacity(0.4))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.redo(cx))),
                        )
                        .child(
                            ui::icon_button("open", "open")
                                .tooltip(ui::tooltip("Open another PDF", Some("⌘O")))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.request_open(None, window, cx)
                                })),
                        )
                        .child(div().w(px(4.)))
                        .when(self.dirty, |d| {
                            d.child(
                                ui::secondary_button("discard", "revert", "Discard")
                                    .tooltip(ui::tooltip("Discard changes since the last save", None))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.request_discard(window, cx)
                                    })),
                            )
                        })
                        .child(
                            ui::primary_button("save", "save", "Save PDF")
                                .tooltip(ui::tooltip("Save a filled copy", Some("⌘S")))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.save(window, cx))),
                        )
                    })
                    .when(!has_doc, |d| {
                        d.child(ui::secondary_button("open", "open", "Open PDF…").on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.request_open(None, window, cx),
                        )))
                    }),
            )
    }

    /// Wrap a menu panel so it floats above everything, anchored below its trigger
    /// (`align_right`: right edges line up), and closes on any click outside it.
    fn floating_menu(&self, panel: gpui::Div, align_right: bool, cx: &mut Context<Self>) -> AnyElement {
        let panel = panel.occlude().on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
            if this.zoom_menu_open || this.date_menu_open {
                this.close_menus();
                // The same click may land on the menu's trigger; don't let it reopen the menu.
                this.menu_dismissed = true;
                cx.notify();
            }
        }));
        div()
            .absolute()
            .top(px(32.))
            .when(align_right, |d| d.right_0())
            .when(!align_right, |d| d.left_0())
            .child(
                deferred(
                    anchored()
                        .anchor(if align_right { gpui::Corner::TopRight } else { gpui::Corner::TopLeft })
                        .snap_to_window_with_margin(px(8.))
                        .child(panel),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    fn render_zoom_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.zoom_mode;
        let fixed = |pct: f32| mode == ZoomMode::Fixed && (self.zoom * 100. - pct).abs() < 0.5;
        let mut panel = ui::menu_panel()
            .child(
                ui::menu_item("zoom-fit-width", "Fit width", mode == ZoomMode::FitWidth, Some("⌘1")).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| this.set_zoom_mode(ZoomMode::FitWidth, window, cx),
                )),
            )
            .child(
                ui::menu_item("zoom-fit-page", "Fit page", mode == ZoomMode::FitPage, Some("⌘2")).on_click(cx.listener(
                    |this, _: &ClickEvent, window, cx| this.set_zoom_mode(ZoomMode::FitPage, window, cx),
                )),
            )
            .child(ui::menu_separator());
        for pct in [50., 75., 100., 125., 150., 200.] {
            panel = panel.child(
                ui::menu_item(
                    SharedString::from(format!("zoom-{pct}")),
                    format!("{pct}%"),
                    fixed(pct),
                    (pct == 100.).then_some("⌘0"),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.set_zoom(pct / 100., window, cx))),
            );
        }
        self.floating_menu(panel, true, cx)
    }

    fn render_date_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = ui::menu_panel().child(
            div()
                .px_2()
                .pt_1()
                .pb_1p5()
                .text_xs()
                .text_color(ui::text_tertiary())
                .child("Date format"),
        );
        for (i, format) in settings::DATE_FORMATS.iter().enumerate() {
            panel = panel.child(
                ui::menu_item(("date-format", i), settings::format_today(format), self.settings.date_format == *format, None)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_date_format(format, cx))),
            );
        }
        self.floating_menu(panel, false, cx)
    }

    fn current_font_size(&self) -> f32 {
        self.selected
            .and_then(|id| self.items.iter().find(|i| i.id == id))
            .and_then(|i| match i.kind {
                ItemKind::Text { font_size, .. } => Some(font_size),
                _ => None,
            })
            .unwrap_or(self.font_size)
    }

    /// The "Fill in" tool list: click a tool then the page, or drag it onto the page.
    fn render_fill_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_doc = self.doc.is_some();
        let mut rows = Vec::new();
        for tool in [Tool::Text, Tool::Date, Tool::Check] {
            let (icon_name, label) = tool_label(tool);
            let active = self.tool == tool;
            let sublabel: SharedString = match tool {
                Tool::Date => self.settings.today().into(),
                Tool::Text => "Type anywhere on the page".into(),
                _ => "Tick a box".into(),
            };
            let row = div()
                .id(SharedString::from(format!("fill-{label}")))
                .flex()
                .items_center()
                .gap_2p5()
                .h(px(42.))
                .px_2()
                .rounded_md()
                .when(active, |d| d.bg(ui::accent_soft()))
                .when(!active && has_doc, |d| d.hover(|s| s.bg(ui::hover_bg())))
                .when(!has_doc, |d| d.opacity(0.45))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(28.))
                        .rounded_md()
                        .border_1()
                        .border_color(if active { ui::accent() } else { ui::border() })
                        .bg(if active { ui::accent() } else { ui::surface() })
                        .child(ui::icon(icon_name, if active { gpui::white() } else { ui::text_secondary() }).size(px(15.))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_sm()
                                .text_color(if active { ui::accent() } else { ui::text_primary() })
                                .when(active, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(label),
                        )
                        .child(div().text_xs().text_color(ui::text_tertiary()).truncate().child(sublabel)),
                )
                .when(tool == Tool::Date, |d| {
                    d.child(
                        div()
                            .id("date-format-trigger")
                            .relative()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_0p5()
                            .px_1p5()
                            .h(px(22.))
                            .rounded(px(5.))
                            .text_xs()
                            .text_color(ui::text_secondary())
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(ui::hover_bg()).text_color(ui::text_primary()))
                            .tooltip(ui::tooltip("Date format", None))
                            .child("Format")
                            .child(ui::icon("chevron-down", ui::text_tertiary()).size(px(11.)))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                if std::mem::take(&mut this.menu_dismissed) {
                                    return;
                                }
                                this.zoom_menu_open = false;
                                this.date_menu_open = !this.date_menu_open;
                                cx.notify();
                            }))
                            .when(self.date_menu_open, |d| d.child(self.render_date_menu(cx))),
                    )
                })
                .when(has_doc, |d| {
                    d.cursor(CursorStyle::PointingHand)
                        .tooltip(ui::tooltip(format!("Click, then click the page — or drag {} onto the page", label.to_lowercase()), None))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.finish_editing(cx);
                            this.tool = if this.tool == tool { Tool::Select } else { tool };
                            window.focus(&this.focus);
                            cx.notify();
                        }))
                        .on_drag(ToolDrag(tool), |drag: &ToolDrag, _, _, cx| cx.new(|_| *drag))
                });
            rows.push(row);
        }

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap_0p5()
            .px_3()
            .pt_4()
            .pb_3()
            .child(
                div()
                    .px_1()
                    .pb_1()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Fill in"),
            )
            .children(rows)
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let zoom = self.zoom;
        let cards: Vec<_> = self
            .signatures
            .iter()
            .enumerate()
            .map(|(index, sig)| {
                let (w, h) = Self::default_sig_size(sig);
                let drag = SignatureDrag {
                    sig: sig.clone(),
                    size: size(px(w * zoom), px(h * zoom)),
                };
                div()
                    .id(("sig", index))
                    .group("sig-card")
                    .relative()
                    .flex_none()
                    .h(px(SIG_THUMB_H + 26.))
                    .p_3()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(ui::paper())
                    .rounded_lg()
                    .border_1()
                    .border_color(ui::border())
                    .shadow(ui::card_shadow())
                    .cursor(CursorStyle::OpenHand)
                    .hover(|s| s.border_color(ui::accent().opacity(0.45)).shadow(ui::lifted_shadow()))
                    // Size the thumbnail ourselves (object-fit "contain") so width and height
                    // always agree with the image's aspect ratio; otherwise GPUI's layout lets
                    // the aspect ratio win and the image overflows the card.
                    .child({
                        let aspect = sig.aspect().max(0.05);
                        let w = SIG_THUMB_W.min(SIG_THUMB_H * aspect);
                        img(sig.path.clone()).w(px(w)).h(px(w / aspect)).object_fit(ObjectFit::Fill)
                    })
                    .on_drag(drag, |drag: &SignatureDrag, cursor_offset, _, cx| {
                        cx.new(|_| SignaturePreview {
                            sig: drag.sig.clone(),
                            size: drag.size,
                            cursor_offset,
                        })
                    })
                    .child(
                        ui::icon_button_tinted(("sig-remove", index), "trash", ui::danger())
                            .absolute()
                            .top(px(4.))
                            .right(px(4.))
                            .size(px(24.))
                            .bg(ui::paper())
                            .opacity(0.)
                            .group_hover("sig-card", |s| s.opacity(1.))
                            .tooltip(ui::tooltip("Remove from library", None))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.remove_signature(index, cx))),
                    )
            })
            .collect();
        let empty = cards.is_empty();

        let shortcut = |label: &'static str, keys: &'static str| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .text_xs()
                .text_color(ui::text_secondary())
                .child(label)
                .child(ui::kbd(keys))
        };

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDEBAR_W))
            .h_full()
            .bg(ui::sidebar_bg())
            .border_r_1()
            .border_color(ui::border())
            .child(self.render_fill_section(cx))
            .child(div().mx_4().h(px(1.)).bg(ui::border()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Signatures"))
                            .when(!empty, |d| {
                                d.child(
                                    div()
                                        .px_1p5()
                                        .rounded_full()
                                        .bg(ui::muted_bg())
                                        .text_xs()
                                        .text_color(ui::text_secondary())
                                        .child(self.signatures.len().to_string()),
                                )
                            }),
                    )
                    .child(
                        ui::icon_button("add-sig", "plus")
                            .tooltip(ui::tooltip("Add signature image", Some("⇧⌘S")))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.add_signature_dialog(window, cx))),
                    ),
            )
            .child(
                div()
                    .id("sig-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_2()
                    .px_3()
                    .pb_3()
                    .when(empty, |d| {
                        d.child(
                            div()
                                .id("add-sig-empty")
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .px_4()
                                .py_5()
                                .rounded_lg()
                                .border_1()
                                .border_dashed()
                                .border_color(ui::border_strong())
                                .cursor(CursorStyle::PointingHand)
                                .hover(|s| s.bg(ui::surface()).border_color(ui::accent()))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(40.))
                                        .rounded_full()
                                        .bg(ui::accent_soft())
                                        .child(ui::icon("signature", ui::accent()).size(px(20.))),
                                )
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Add your signature"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_center()
                                        .text_color(ui::text_tertiary())
                                        .child("PNG or JPG. The paper background is removed automatically."),
                                )
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.add_signature_dialog(window, cx))),
                        )
                    })
                    .children(cards)
                    .when(!empty, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap_1p5()
                                .pt_1()
                                .text_xs()
                                .text_color(ui::text_tertiary())
                                .child(ui::icon("upload", ui::text_tertiary()).size(px(13.)))
                                .child("Drag onto a page to sign"),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .gap_1p5()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(ui::border())
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(ui::text_tertiary())
                            .mb_0p5()
                            .child("Shortcuts"),
                    )
                    .child(shortcut("Undo / Redo", "⌘Z  ⇧⌘Z"))
                    .child(shortcut("Copy / Paste field", "⌘C  ⌘V"))
                    .child(shortcut("Duplicate field", "⌘D"))
                    .child(shortcut("Delete field", "⌫"))
                    .child(shortcut("Nudge (Shift = 10pt)", "←↑→↓")),
            )
    }

    fn render_item(&self, item: &Item, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let z = self.zoom;
        let id = item.id;
        let selected = self.selected == Some(id);
        let editing = self.editing == Some(id);
        let dragging = self.drag.as_ref().is_some_and(|d| d.id == id && d.moved);

        let content = match &item.kind {
            ItemKind::Text { text, font_size } => {
                let fs = px(font_size * z);
                let lh = px(font_size * LINE_HEIGHT * z);
                let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
                let last = lines.len() - 1;
                let placeholder = editing && text.is_empty();
                div()
                    .flex()
                    .flex_col()
                    .font_family("Helvetica")
                    .text_size(fs)
                    .line_height(lh)
                    .text_color(gpui::black())
                    .whitespace_nowrap()
                    .children(lines.into_iter().enumerate().map(move |(i, line)| {
                        div()
                            .flex()
                            .h(lh)
                            .when(placeholder, |d| d.child(div().text_color(ui::text_tertiary()).child("Type here")))
                            .when(!line.is_empty(), |d| d.child(line))
                            .when(editing && i == last, |d| d.child(div().w(px(1.5)).h(lh).bg(ui::accent())))
                    }))
                    .into_any_element()
            }
            ItemKind::Image { sig } => img(sig.path.clone())
                .w(px(item.w * z))
                .h(px(item.h * z))
                .object_fit(ObjectFit::Fill)
                .into_any_element(),
            ItemKind::Check => {
                let s = item.w * z;
                div()
                    .size(px(s))
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                let o = bounds.origin;
                                let p = |x: f32, y: f32| o + point(px(x * s), px(y * s));
                                let mut b = PathBuilder::stroke(px(s * 0.12));
                                b.move_to(p(0.15, 0.5));
                                b.line_to(p(0.4, 0.8));
                                b.line_to(p(0.85, 0.15));
                                if let Ok(path) = b.build() {
                                    window.paint_path(path, gpui::black());
                                }
                            },
                        )
                        .size_full(),
                    )
                    .into_any_element()
            }
        };

        let is_text = item.is_text();
        let multi_page = self.doc.as_ref().is_some_and(|d| d.pages.len() > 1);
        // Show the HUD above the field, or below it when there's no room on the page.
        let hud_above = item.y * z > 44.;
        let hud = div()
            .id(("hud", id as usize))
            .absolute()
            .left(px(-1.))
            .when(hud_above, |d| d.top(px(-40.)))
            .when(!hud_above, |d| d.top(px(item.h * z + 2. * ITEM_PAD + 6.)))
            .flex()
            .flex_none()
            .items_center()
            .gap_0p5()
            .p(px(3.))
            .rounded_lg()
            .bg(ui::hud_bg())
            .shadow(ui::lifted_shadow())
            .whitespace_nowrap()
            .cursor(CursorStyle::Arrow)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(is_text, |h| {
                h.child(
                    ui::hud_button(("smaller", id as usize), "text-smaller")
                        .tooltip(ui::tooltip("Smaller text", None))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.set_font_size(-1., cx))),
                )
                .child(
                    div()
                        .min_w(px(34.))
                        .text_center()
                        .text_xs()
                        .text_color(gpui::white())
                        .child(format!("{}pt", self.current_font_size())),
                )
                .child(
                    ui::hud_button(("larger", id as usize), "text-larger")
                        .tooltip(ui::tooltip("Larger text", None))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.set_font_size(1., cx))),
                )
                .child(div().w(px(1.)).h(px(16.)).mx_0p5().bg(rgba(0xffffff26)))
            })
            .child(
                ui::hud_button(("duplicate", id as usize), "copy")
                    .tooltip(ui::tooltip("Duplicate", Some("⌘D")))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.duplicate(cx))),
            )
            .when(multi_page, |h| {
                h.child(
                    ui::hud_button(("all-pages", id as usize), "layers")
                        .tooltip(ui::tooltip("Copy to every page", None))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_to_all_pages(cx))),
                )
            })
            .child(div().w(px(1.)).h(px(16.)).mx_0p5().bg(rgba(0xffffff26)))
            .child(
                ui::hud_button_tinted(("delete", id as usize), "trash", rgb(0xff8a8d).into())
                    .tooltip(ui::tooltip("Delete", Some("⌫")))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.delete_item(id, cx))),
            );

        div()
            .id(("item", id as usize))
            .absolute()
            .left(px(item.x * z - ITEM_PAD))
            .top(px(item.y * z - ITEM_PAD))
            .p(px(ITEM_PAD - 1.))
            .border_1()
            .rounded(px(3.))
            .border_color(if selected { ui::accent() } else { gpui::transparent_black() })
            .when(!selected, |d| d.hover(|s| s.border_color(ui::accent().opacity(0.35)).bg(ui::accent().opacity(0.03))))
            .when(editing, |d| d.bg(ui::accent().opacity(0.04)))
            .cursor(if editing { CursorStyle::IBeam } else { CursorStyle::OpenHand })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| this.item_mouse_down(id, DragMode::Move, e, window, cx)),
            )
            .child(content)
            .when(selected && !dragging, |d| d.child(hud))
            .when(selected && !is_text, |d| {
                d.child(
                    div()
                        .id(("resize", id as usize))
                        .absolute()
                        .bottom(px(-6.))
                        .right(px(-6.))
                        .size(px(11.))
                        .rounded_full()
                        .bg(gpui::white())
                        .border_2()
                        .border_color(ui::accent())
                        .shadow(ui::card_shadow())
                        .cursor(CursorStyle::ResizeUpLeftDownRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                this.item_mouse_down(id, DragMode::Resize, e, window, cx)
                            }),
                        ),
                )
            })
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("empty")
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .bg(ui::canvas_bg())
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(ui::accent_soft()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_5()
                    .w(px(420.))
                    .px_8()
                    .py_10()
                    .bg(ui::surface())
                    .rounded_xl()
                    .border_1()
                    .border_color(ui::border())
                    .shadow(ui::page_shadow())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(64.))
                            .rounded_full()
                            .bg(ui::accent_soft())
                            .child(ui::icon("file", ui::accent()).size(px(30.))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(ui::text_primary())
                                    .child("Open a PDF to get started"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_center()
                                    .text_color(ui::text_secondary())
                                    .child("Drop a file anywhere in this window, or choose one from your Mac."),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(ui::primary_button("open-empty", "open", "Open PDF…").on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| this.request_open(None, window, cx),
                            )))
                            .child(ui::kbd("⌘O")),
                    ),
            )
            .into_any_element()
    }

    fn render_gallery(&self, cx: &mut Context<Self>) -> AnyElement {
        const CARD_W: f32 = 200.;
        const THUMB_BOX_H: f32 = 236.;
        const THUMB_PAD: f32 = 16.;

        let cards: Vec<_> = self
            .recents
            .iter()
            .map(|entry| {
                let open_id = entry.id.clone();
                let remove_id = entry.id.clone();
                let output = entry.output.clone();
                let original_missing = !entry.original.exists();
                let output_exists = entry.output.exists();
                let fields = entry.items.len();

                let thumb = match &entry.thumbnail {
                    Some(path) if entry.thumb_height > 0 && path.exists() => {
                        let aspect = entry.thumb_width as f32 / entry.thumb_height as f32;
                        let (box_w, box_h) = (CARD_W - 2. * THUMB_PAD, THUMB_BOX_H - 2. * THUMB_PAD);
                        let w = box_w.min(box_h * aspect);
                        img(path.clone())
                            .w(px(w))
                            .h(px(w / aspect))
                            .object_fit(ObjectFit::Fill)
                            .shadow(ui::page_shadow())
                            .into_any_element()
                    }
                    _ => ui::icon("file", ui::text_tertiary()).size(px(36.)).into_any_element(),
                };

                div()
                    .id(SharedString::from(format!("recent-{}", entry.id)))
                    .group("recent-card")
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(CARD_W))
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(ui::surface())
                    .border_1()
                    .border_color(ui::border())
                    .shadow(ui::card_shadow())
                    .cursor(CursorStyle::PointingHand)
                    .hover(|s| s.border_color(ui::accent().opacity(0.5)).shadow(ui::lifted_shadow()))
                    .tooltip(ui::tooltip(entry.original.display().to_string(), None))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.open_recent(&open_id, window, cx)))
                    .child(
                        div()
                            .relative()
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(THUMB_BOX_H))
                            .bg(ui::muted_bg())
                            .border_b_1()
                            .border_color(ui::border())
                            .child(thumb)
                            .child(
                                div()
                                    .absolute()
                                    .top(px(8.))
                                    .right(px(8.))
                                    .flex()
                                    .gap_1()
                                    .opacity(0.)
                                    .group_hover("recent-card", |s| s.opacity(1.))
                                    .when(output_exists, |d| {
                                        d.child(
                                            ui::icon_button(SharedString::from(format!("reveal-{}", entry.id)), "reveal")
                                                .size(px(28.))
                                                .bg(ui::surface())
                                                .shadow(ui::card_shadow())
                                                .tooltip(ui::tooltip("Show saved PDF in Finder", None))
                                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                                .on_click(move |_, _, cx| {
                                                    cx.stop_propagation();
                                                    cx.reveal_path(&output);
                                                }),
                                        )
                                    })
                                    .child(
                                        ui::icon_button_tinted(
                                            SharedString::from(format!("forget-{}", entry.id)),
                                            "close",
                                            ui::danger(),
                                        )
                                        .size(px(28.))
                                        .bg(ui::surface())
                                        .shadow(ui::card_shadow())
                                        .tooltip(ui::tooltip("Remove from Recents (files are kept)", None))
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            cx.stop_propagation();
                                            this.remove_recent(&remove_id, cx);
                                        })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .px_3()
                            .py_2p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(ui::text_primary())
                                    .truncate()
                                    .child(file_name(&entry.original)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(ui::text_tertiary())
                                    .truncate()
                                    .child(format!(
                                        "Saved {} · {fields} field{}",
                                        relative_time(entry.saved_at),
                                        if fields == 1 { "" } else { "s" }
                                    )),
                            )
                            .when(original_missing, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(ui::warning())
                                        .truncate()
                                        .child(if output_exists { "Original moved · opens copy" } else { "Files not found" }),
                                )
                            }),
                    )
            })
            .collect();

        let open_tile = div()
            .id("gallery-open")
            .flex()
            .flex_col()
            .flex_none()
            .items_center()
            .justify_center()
            .gap_2()
            .w(px(CARD_W))
            // Stretches to the height of the cards in its row.
            .min_h(px(THUMB_BOX_H + 62.))
            .rounded_lg()
            .border_1()
            .border_dashed()
            .border_color(ui::border_strong())
            .cursor(CursorStyle::PointingHand)
            .hover(|s| s.border_color(ui::accent()).bg(ui::surface()))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.request_open(None, window, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(44.))
                    .rounded_full()
                    .bg(ui::accent_soft())
                    .child(ui::icon("plus", ui::accent()).size(px(22.))),
            )
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Open PDF…"))
            .child(div().text_xs().text_color(ui::text_tertiary()).child("or drop a file here"));

        div()
            .id("gallery")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .bg(ui::canvas_bg())
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(ui::accent_soft()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .px_10()
                    .py_8()
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .justify_between()
                            .gap_4()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xl()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(ui::text_primary())
                                            .child("Recent documents"),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(ui::text_secondary())
                                            .child("Reopen a document to keep editing. The fields you placed stay editable."),
                                    ),
                            )
                            .child(ui::primary_button("gallery-open-btn", "open", "Open PDF…").on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| this.request_open(None, window, cx),
                            ))),
                    )
                    .child(div().flex().flex_wrap().gap_5().child(open_tile).children(cards)),
            )
            .into_any_element()
    }

    fn render_thumbnail_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(doc) = &self.doc else { return div().into_any_element() };
        let current = self.current_page();
        let thumbs: Vec<_> = doc
            .loaded
            .pages
            .iter()
            .zip(&doc.pages)
            .enumerate()
            .map(|(index, (geom, view))| {
                let is_current = current == Some(index);
                let fields = self.items.iter().filter(|i| i.page == index).count();
                let aspect = geom.width / geom.height.max(1.);
                let (w, h) = if aspect >= 1. || THUMB_W / aspect <= 1.5 * THUMB_W {
                    (THUMB_W, THUMB_W / aspect)
                } else {
                    (1.5 * THUMB_W * aspect, 1.5 * THUMB_W)
                };
                div()
                    .id(("thumb", index))
                    .group("thumb")
                    .flex()
                    .flex_col()
                    .flex_none()
                    .items_center()
                    .gap_1p5()
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.scroll_to_page(index, cx)))
                    .child(
                        div()
                            .relative()
                            .w(px(w))
                            .h(px(h))
                            .bg(gpui::white())
                            .shadow(if is_current { ui::ring() } else { ui::page_shadow() })
                            .when_some(view.thumb.clone(), |d, image| {
                                d.child(img(image).size_full().object_fit(ObjectFit::Fill))
                            })
                            .when(fields > 0, |d| {
                                d.child(
                                    div()
                                        .absolute()
                                        .top(px(-7.))
                                        .right(px(-7.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .min_w(px(18.))
                                        .h(px(18.))
                                        .px_1()
                                        .rounded_full()
                                        .bg(ui::accent())
                                        .text_color(gpui::white())
                                        .text_size(px(10.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(fields.to_string()),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if is_current { ui::accent() } else { ui::text_tertiary() })
                            .when(is_current, |d| d.font_weight(FontWeight::MEDIUM))
                            .group_hover("thumb", |s| s.text_color(ui::text_primary()))
                            .child((index + 1).to_string()),
                    )
            })
            .collect();

        div()
            .id("thumb-rail")
            .flex()
            .flex_col()
            .flex_none()
            .items_center()
            .w(px(THUMB_RAIL_W))
            .h_full()
            .overflow_y_scroll()
            .gap_5()
            .pt_4()
            .pb_6()
            .bg(ui::sidebar_bg())
            .border_l_1()
            .border_color(ui::border())
            .child(
                div()
                    .w_full()
                    .px_4()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Pages"),
            )
            .children(thumbs)
            .into_any_element()
    }

    fn render_pages(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(doc) = &self.doc else {
            return if self.recents.is_empty() {
                self.render_empty_state(cx)
            } else {
                self.render_gallery(cx)
            };
        };

        let z = self.zoom;
        let page_count = doc.pages.len();
        let cursor = if self.tool == Tool::Select { CursorStyle::Arrow } else { CursorStyle::Crosshair };
        let mut page_elements = Vec::with_capacity(page_count);
        for (index, (geom, view)) in doc.loaded.pages.iter().zip(&doc.pages).enumerate() {
            let items: Vec<_> = self
                .items
                .iter()
                .filter(|i| i.page == index)
                .map(|item| self.render_item(item, cx))
                .collect();

            let bounds = view.bounds.clone();
            let page = div()
                .id(("page", index))
                .relative()
                .flex_none()
                .w(px(geom.width * z))
                .h(px(geom.height * z))
                .bg(gpui::white())
                .shadow(ui::page_shadow())
                .drag_over::<SignatureDrag>(|s, _, _, _| s.shadow(ui::ring()))
                .drag_over::<ExternalPaths>(|s, _, _, _| s.shadow(ui::ring()))
                .cursor(cursor)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| this.page_mouse_down(index, e, window, cx)),
                )
                .child(canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().size_full())
                .when_some(view.image.clone(), |d, image| {
                    d.child(img(image).absolute().size_full().object_fit(ObjectFit::Fill))
                })
                .when(view.image.is_none(), |d| {
                    d.child(
                        div()
                            .absolute()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(ui::text_tertiary())
                            .child("Rendering…"),
                    )
                })
                .children(items);

            page_elements.push(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(page)
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui::text_tertiary())
                            .child(format!("{} of {page_count}", index + 1)),
                    ),
            );
        }

        let hint = match self.tool {
            Tool::Select => None,
            Tool::Text => Some(("text", "Click on the page to add a text box")),
            Tool::Date => Some(("calendar", "Click on the page to add today's date")),
            Tool::Check => Some(("check", "Click on the page to add a checkmark")),
        };

        div()
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(
                div()
                    .id("pages")
                    .size_full()
                    .overflow_scroll()
                    .track_scroll(&self.pages_scroll)
                    // Keeps the thumbnail rail's "current page" highlight in sync.
                    .on_scroll_wheel(cx.listener(|_, _: &ScrollWheelEvent, _, cx| cx.notify()))
                    .bg(ui::canvas_bg())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_6()
                            .px_8()
                            .py_8()
                            .min_w_full()
                            .children(page_elements),
                    ),
            )
            .when_some(hint, |d, (icon_name, text)| {
                d.child(
                    div().absolute().bottom(px(20.)).left_0().right_0().flex().justify_center().child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .pl_3()
                            .pr_1p5()
                            .py_1p5()
                            .rounded_full()
                            .bg(ui::hud_bg())
                            .text_color(gpui::white())
                            .text_xs()
                            .shadow(ui::lifted_shadow())
                            .child(ui::icon(icon_name, gpui::white()).size(px(14.)))
                            .child(text)
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded_full()
                                    .bg(rgba(0xffffff1f))
                                    .text_color(ui::hud_muted())
                                    .child("Esc to cancel"),
                            ),
                    ),
                )
            })
            .into_any_element()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        ui::set_appearance(window.appearance());
        // Fit modes follow the viewport (e.g. when the window is resized). The viewport
        // size comes from the previous frame; on the very first frame, try again next frame.
        if self.zoom_mode != ZoomMode::Fixed && self.doc.is_some() {
            match self.fit_zoom(self.zoom_mode) {
                Some(zoom) if (zoom - self.zoom).abs() > 0.002 => {
                    self.zoom = zoom;
                    self.ensure_rendered(window, cx);
                }
                None => cx.on_next_frame(window, |_, _, cx| cx.notify()),
                _ => {}
            }
        }
        let field_count = self.items.len();

        div()
            .key_context("Editor")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::chrome_bg())
            .text_color(ui::text_primary())
            .font_family(".SystemUIFont")
            .on_action(cx.listener(|this, _: &Open, window, cx| this.request_open(None, window, cx)))
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.redo(cx)))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &Duplicate, _, cx| this.duplicate(cx)))
            .on_action(cx.listener(|this, _: &ZoomFitWidth, window, cx| this.set_zoom_mode(ZoomMode::FitWidth, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomFitPage, window, cx| this.set_zoom_mode(ZoomMode::FitPage, window, cx)))
            .on_action(cx.listener(|this, _: &AddSignature, window, cx| this.add_signature_dialog(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| this.set_zoom(this.zoom * 1.2, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| this.set_zoom(this.zoom / 1.2, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, window, cx| this.set_zoom(1.0, window, cx)))
            .on_action(cx.listener(|this, _: &CloseDocument, window, cx| this.request_close(window, cx)))
            .on_action(cx.listener(|this, _: &DiscardChanges, window, cx| this.request_discard(window, cx)))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.handle_external_drop(paths, window, cx)))
            .on_drop(cx.listener(|this, drag: &ToolDrag, window, cx| this.drop_tool(drag.0, window, cx)))
            .on_drop(cx.listener(|this, drag: &SignatureDrag, window, cx| {
                if !this.place_signature(drag.sig.clone(), window.mouse_position(), cx) {
                    this.status = "Drop the signature onto a page.".into();
                    cx.notify();
                }
            }))
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_pages(cx))
                    .when(self.doc.is_some(), |d| d.child(self.render_thumbnail_rail(cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .h(px(26.))
                    .px_3()
                    .text_xs()
                    .text_color(ui::text_secondary())
                    .bg(ui::chrome_bg())
                    .border_t_1()
                    .border_color(ui::border())
                    .child(div().truncate().child(self.status.clone()))
                    .when(self.doc.is_some(), |d| {
                        d.child(
                            div()
                                .flex_none()
                                .child(format!("{field_count} field{}", if field_count == 1 { "" } else { "s" })),
                        )
                    }),
            )
    }
}

/// "just now", "5 min ago", "3 h ago", "yesterday", or a date.
fn relative_time(timestamp: i64) -> String {
    let elapsed = chrono::Utc::now().timestamp() - timestamp;
    match elapsed {
        ..60 => "just now".into(),
        60..3600 => format!("{} min ago", elapsed / 60),
        3600..86400 => format!("{} h ago", elapsed / 3600),
        86400..172800 => "yesterday".into(),
        _ => chrono::DateTime::from_timestamp(timestamp, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%b %-d, %Y").to_string())
            .unwrap_or_default(),
    }
}

/// Wrap a rendered page (already BGRA) as a GPUI image.
fn to_render_image(page: pdf::RenderedPage) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(page.width, page.height, page.bgra)?;
    Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("cmd-o", Open, None),
        KeyBinding::new("cmd-s", Save, None),
        KeyBinding::new("cmd-z", Undo, None),
        KeyBinding::new("cmd-shift-z", Redo, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-x", Cut, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-d", Duplicate, None),
        KeyBinding::new("cmd-1", ZoomFitWidth, None),
        KeyBinding::new("cmd-2", ZoomFitPage, None),
        KeyBinding::new("cmd-=", ZoomIn, None),
        KeyBinding::new("cmd-+", ZoomIn, None),
        KeyBinding::new("cmd--", ZoomOut, None),
        KeyBinding::new("cmd-0", ZoomReset, None),
        KeyBinding::new("cmd-shift-s", AddSignature, None),
        KeyBinding::new("cmd-w", CloseDocument, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
}

pub fn open_editor(cx: &mut App, initial: Option<PathBuf>) -> anyhow::Result<Entity<Editor>> {
    let bounds = Bounds::centered(None, size(px(1240.), px(900.)), cx);
    let mut editor = None;
    cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("PDF Signer".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(18.), px(19.))),
            }),
            ..Default::default()
        },
        |window, cx| {
            let entity = cx.new(|cx| {
                let mut e = Editor::new(window, cx);
                if let Some(path) = initial {
                    e.open_path(path, window, cx);
                }
                e
            });
            editor = Some(entity.clone());
            entity
        },
    )?;
    editor.ok_or_else(|| anyhow::anyhow!("window was not created"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    fn setup(cx: &mut TestAppContext) -> (Entity<Editor>, &mut VisualTestContext) {
        let dir = std::env::temp_dir().join("pdf_signer_ui_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.pdf");
        std::fs::write(&path, crate::pdf::tests::sample_pdf()).unwrap();

        cx.update(bind_keys);
        let (editor, cx) = cx.add_window_view(|window, cx| Editor::new(window, cx));
        editor.update_in(cx, |e, window, cx| e.open_path(path, window, cx));
        cx.run_until_parked();
        // Tests reason in page points, so pin 100% instead of the default "fit width".
        editor.update_in(cx, |e, window, cx| e.set_zoom(1.0, window, cx));
        cx.run_until_parked();
        (editor, cx)
    }

    fn page_origin(editor: &Entity<Editor>, cx: &mut VisualTestContext, page: usize) -> Point<Pixels> {
        editor.read_with(cx, |e, _| e.doc.as_ref().unwrap().pages[page].bounds.get().origin)
    }

    #[gpui::test]
    fn text_tool_typing_and_dragging(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        assert!(origin.x > px(0.), "page bounds were not captured");
        editor.read_with(cx, |e, _| assert!(e.doc.as_ref().unwrap().pages.iter().all(|p| p.image.is_some())));

        // Text tool → click on the page → type.
        editor.update(cx, |e, _| e.tool = Tool::Text);
        cx.simulate_click(origin + point(px(100.), px(150.)), Modifiers::none());
        cx.simulate_keystrokes("H i space shift-t enter backspace backspace");
        cx.simulate_keystrokes("! escape");
        let (id, x, y) = editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 1);
            let item = &e.items[0];
            match &item.kind {
                ItemKind::Text { text, .. } => assert_eq!(text, "Hi !"),
                _ => panic!("expected text"),
            }
            assert_eq!(e.editing, None);
            assert_eq!(e.tool, Tool::Select);
            assert!((item.x - 100.).abs() < 0.01);
            (item.id, item.x, item.y)
        });

        // Drag it by 50x30 px at 100% zoom.
        let start = origin + point(px(x + 2.), px(y + 4.));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(start + point(px(25.), px(10.)), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(start + point(px(50.), px(30.)), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(start + point(px(50.), px(30.)), MouseButton::Left, Modifiers::none());
        editor.read_with(cx, |e, _| {
            let item = e.items.iter().find(|i| i.id == id).unwrap();
            assert!((item.x - (x + 50.)).abs() < 0.01, "x = {}", item.x);
            assert!((item.y - (y + 30.)).abs() < 0.01, "y = {}", item.y);
        });

        // Undo the move, then undo the creation.
        cx.simulate_keystrokes("cmd-z");
        editor.read_with(cx, |e, _| assert!((e.items[0].x - x).abs() < 0.01));
    }

    #[gpui::test]
    fn check_and_signature_placement_then_save(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 1);

        editor.update(cx, |e, _| e.tool = Tool::Check);
        cx.simulate_click(origin + point(px(60.), px(60.)), Modifiers::none());

        let sig_path = std::env::temp_dir().join("pdf_signer_ui_test/sig.png");
        let mut img = image::RgbaImage::from_pixel(300, 100, image::Rgba([255, 255, 255, 255]));
        for x in 20..280 {
            img.put_pixel(x, 50, image::Rgba([0, 0, 80, 255]));
        }
        img.save(&sig_path).unwrap();
        let sig = Arc::new(Signature { path: sig_path, width_px: 300, height_px: 100 });
        editor.update(cx, |e, cx| assert!(e.place_signature(sig, origin + point(px(300.), px(200.)), cx)));

        // Select the signature and delete it with the keyboard, then undo.
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 2);
            assert!(e.items.iter().all(|i| i.page == 1));
        });
        cx.simulate_keystrokes("backspace");
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 1));
        cx.simulate_keystrokes("cmd-z");

        let (bytes, pages, items, metrics) = editor.read_with(cx, |e, _| {
            let d = e.doc.as_ref().unwrap();
            (d.loaded.bytes.clone(), d.loaded.pages.clone(), e.items.clone(), e.metrics)
        });
        assert_eq!(items.len(), 2);
        let out = crate::pdf::write(&bytes, &pages, &items, metrics).unwrap();
        assert_eq!(crate::pdf::load(out).unwrap().pages.len(), 2);
    }

    #[gpui::test]
    fn redo_copy_paste_duplicate_and_copy_to_all_pages(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        let count = |editor: &Entity<Editor>, cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.items.len());

        editor.update(cx, |e, _| e.tool = Tool::Check);
        cx.simulate_click(origin + point(px(50.), px(50.)), Modifiers::none());
        assert_eq!(count(&editor, cx), 1);

        // Undo, then redo brings the field back.
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(count(&editor, cx), 0);
        cx.simulate_keystrokes("cmd-shift-z");
        assert_eq!(count(&editor, cx), 1);
        cx.simulate_keystrokes("cmd-shift-z");
        assert_eq!(count(&editor, cx), 1, "nothing left to redo");

        // Copy + paste: a copy offset from the original on the visible page.
        let original = editor.update(cx, |e, _| {
            e.selected = Some(e.items[0].id);
            e.items[0].clone()
        });
        cx.simulate_keystrokes("cmd-c cmd-v");
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 2);
            let pasted = &e.items[1];
            assert_ne!(pasted.id, original.id);
            assert_eq!((pasted.page, pasted.x, pasted.y), (0, original.x + 12., original.y + 12.));
            assert_eq!(e.selected, Some(pasted.id));
        });
        // Pasting again cascades instead of stacking on the previous copy.
        cx.simulate_keystrokes("cmd-v");
        editor.read_with(cx, |e, _| assert_eq!(e.items[2].x, original.x + 24.));

        // Duplicate, then undo it; a new edit clears the redo stack.
        cx.simulate_keystrokes("cmd-d");
        assert_eq!(count(&editor, cx), 4);
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(count(&editor, cx), 3);
        editor.update(cx, |e, cx| {
            e.selected = Some(original.id);
            e.copy_to_all_pages(cx);
            assert!(e.future.is_empty());
        });
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 4);
            let copy = &e.items[3];
            assert_eq!(copy.page, 1);
            assert_eq!((copy.x, copy.y), (original.x, original.y));
        });
        // One undo removes every copy made by "copy to every page".
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(count(&editor, cx), 3);
    }

    #[gpui::test]
    fn dropped_date_uses_the_chosen_format(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        editor.update(cx, |e, cx| e.set_date_format("%Y-%m-%d", cx));
        assert_eq!(Settings::load().date_format, "%Y-%m-%d", "the choice is remembered");

        cx.simulate_mouse_move(origin + point(px(80.), px(80.)), None, Modifiers::none());
        editor.update_in(cx, |e, window, cx| e.drop_tool(Tool::Date, window, cx));
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 1);
            let item = &e.items[0];
            match &item.kind {
                ItemKind::Text { text, .. } => assert_eq!(*text, chrono::Local::now().format("%Y-%m-%d").to_string()),
                _ => panic!("expected a date"),
            }
            assert!((item.x - 80.).abs() < 0.01);
            // A placed date is finished, so it's selected rather than being edited.
            assert_eq!((e.selected, e.editing), (Some(item.id), None));
            assert_eq!(e.tool, Tool::Select);
        });

        // Dropping outside any page adds nothing.
        cx.simulate_mouse_move(point(px(5.), px(5.)), None, Modifiers::none());
        editor.update_in(cx, |e, window, cx| e.drop_tool(Tool::Check, window, cx));
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 1));
    }

    #[gpui::test]
    fn fit_zoom_thumbnails_and_page_navigation(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);

        // Fit width: the widest page spans the viewport (minus padding).
        cx.simulate_keystrokes("cmd-1");
        cx.run_until_parked();
        let (zoom, widest, viewport) = editor.read_with(cx, |e, _| {
            let widest = e.doc.as_ref().unwrap().loaded.pages.iter().map(|p| p.width).fold(0., f32::max);
            (e.zoom, widest, e.pages_scroll.bounds())
        });
        let expected = (viewport.size.width / px(1.) - 72.) / widest;
        assert!((zoom - expected.clamp(MIN_ZOOM, MAX_ZOOM)).abs() < 0.01, "zoom {zoom} vs {expected}");

        // Fit page: a whole page fits vertically too.
        cx.simulate_keystrokes("cmd-2");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert_eq!(e.zoom_mode, ZoomMode::FitPage);
            let page = e.doc.as_ref().unwrap().pages[0].bounds.get();
            assert!(page.size.height <= viewport.size.height, "page should fit: {page:?} in {viewport:?}");
        });

        // A fixed percentage leaves the fit modes.
        cx.simulate_keystrokes("cmd-0");
        editor.read_with(cx, |e, _| assert_eq!((e.zoom_mode, e.zoom), (ZoomMode::Fixed, 1.0)));

        // Every page gets a thumbnail; clicking one scrolls that page into view.
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert!(e.doc.as_ref().unwrap().pages.iter().all(|p| p.thumb.is_some()));
            assert_eq!(e.current_page(), Some(0));
        });
        editor.update(cx, |e, cx| e.scroll_to_page(1, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert_eq!(e.current_page(), Some(1)));
    }

    #[gpui::test]
    fn discard_reverts_to_last_save_and_is_undoable(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        editor.update(cx, |e, _| e.tool = Tool::Check);
        cx.simulate_click(origin + point(px(50.), px(50.)), Modifiers::none());
        editor.read_with(cx, |e, _| assert!(e.dirty && e.items.len() == 1));

        // Cancelling the confirmation keeps the field.
        editor.update_in(cx, |e, window, cx| e.request_discard(window, cx));
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 1));

        // Confirming removes it and clears "Edited".
        editor.update_in(cx, |e, window, cx| e.request_discard(window, cx));
        cx.simulate_prompt_answer("Discard");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert!(e.items.is_empty());
            assert!(!e.dirty);
        });

        // ...and ⌘Z brings it back.
        cx.simulate_keystrokes("cmd-z");
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 1));
    }

    #[gpui::test]
    fn reopening_a_recent_restores_editable_fields(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let original = editor.read_with(cx, |e, _| e.doc.as_ref().unwrap().path.clone());
        // Built in memory: the recents store on disk has its own test, and sharing it
        // between parallel tests would make them flaky.
        use crate::recents::{SavedItem, SavedKind};
        let id = "test-entry".to_string();
        let entries = vec![RecentEntry {
            id: id.clone(),
            original: original.clone(),
            output: original.with_file_name("out.pdf"),
            saved_at: 0,
            page_count: 2,
            thumbnail: None,
            thumb_width: 0,
            thumb_height: 0,
            items: vec![
                SavedItem { page: 0, x: 40., y: 60., w: 50., h: 14., kind: SavedKind::Text { text: "Restored".into(), font_size: 14. } },
                SavedItem { page: 1, x: 70., y: 80., w: 16., h: 16., kind: SavedKind::Check },
            ],
        }];

        editor.update_in(cx, |e, window, cx| {
            e.recents = entries;
            e.close_document(window, cx);
        });
        editor.read_with(cx, |e, _| assert!(e.doc.is_none() && e.items.is_empty()));

        editor.update_in(cx, |e, window, cx| e.open_recent(&id, window, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert!(e.doc.is_some());
            assert_eq!(e.items.len(), 2);
            assert!(!e.dirty, "a freshly reopened document has no unsaved changes");
            assert!(matches!(&e.items[0].kind, ItemKind::Text { text, .. } if text == "Restored"));
            assert_eq!(e.doc.as_ref().unwrap().last_output.as_deref(), Some(original.with_file_name("out.pdf").as_path()));
            // Restored ids must not collide with ids handed out later.
            assert!(e.items.iter().all(|i| i.id < e.next_id));
        });

        // Restored fields are real fields: they can be deleted and the deletion discarded.
        let first = editor.read_with(cx, |e, _| e.items[0].id);
        editor.update(cx, |e, cx| e.delete_item(first, cx));
        editor.update(cx, |e, cx| e.discard_changes(cx));
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 2));
    }
}
