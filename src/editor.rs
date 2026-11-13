//! The main editor window: toolbar, signature library sidebar and page view.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, ClickEvent, Context, CursorStyle, Entity, ExternalPaths, FocusHandle, Focusable, Hsla,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, PathBuilder,
    PathPromptOptions, Pixels, Point, PromptLevel, RenderImage, SharedString, Task, Window, actions, canvas, div,
    font, img, point, prelude::*, px, rgb, size,
};

use crate::model::{Item, ItemKind, LINE_HEIGHT, PageGeom, Signature, TextMetrics};
use crate::pdf::{self, LoadedPdf};
use crate::signatures;

actions!(
    pdf_signer,
    [Open, Save, Undo, ZoomIn, ZoomOut, ZoomReset, Paste, AddSignature, Quit]
);

const MIN_ZOOM: f32 = 0.3;
const MAX_ZOOM: f32 = 4.0;
const MAX_RENDER_SCALE: f32 = 6.0;
const DEFAULT_FONT_SIZE: f32 = 12.0;
const DEFAULT_SIG_WIDTH: f32 = 150.0;
const DEFAULT_SIG_MAX_HEIGHT: f32 = 55.0;
const DEFAULT_CHECK_SIZE: f32 = 14.0;
/// Padding (in screen px) between an item's content and its selection border.
const ITEM_PAD: f32 = 3.0;

fn accent() -> Hsla {
    rgb(0x2f6fed).into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Select,
    Text,
    Date,
    Check,
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
    rendered_scale: f32,
    /// Screen bounds of the page, captured during the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

struct OpenDoc {
    path: PathBuf,
    loaded: LoadedPdf,
    pages: Vec<PageView>,
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
                    .border_1()
                    .border_color(accent())
                    .bg(gpui::white().opacity(0.4))
                    .child(img(self.sig.path.clone()).size_full().object_fit(ObjectFit::Fill)),
            )
    }
}

pub struct Editor {
    focus: FocusHandle,
    doc: Option<OpenDoc>,
    signatures: Vec<Arc<Signature>>,
    items: Vec<Item>,
    next_id: u64,
    selected: Option<u64>,
    editing: Option<u64>,
    typing_checkpointed: bool,
    tool: Tool,
    zoom: f32,
    drag: Option<DragState>,
    history: Vec<Vec<Item>>,
    status: SharedString,
    metrics: TextMetrics,
    dirty: bool,
    font_size: f32,
    render_task: Option<Task<()>>,
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
            next_id: 1,
            selected: None,
            editing: None,
            typing_checkpointed: false,
            tool: Tool::Select,
            zoom: 1.0,
            drag: None,
            history: Vec::new(),
            status: "Open a PDF to get started (⌘O), or drop one onto this window.".into(),
            metrics,
            dirty: false,
            font_size: DEFAULT_FONT_SIZE,
            render_task: None,
        }
    }

    // -----------------------------------------------------------------------
    // Document lifecycle
    // -----------------------------------------------------------------------

    fn request_open(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let confirm = self.dirty.then(|| {
            window.prompt(
                PromptLevel::Warning,
                "Discard unsaved changes?",
                Some("The current document has placed fields that haven't been saved."),
                &["Discard", "Cancel"],
                cx,
            )
        });
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

    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(pdf::load);
        let loaded = match loaded {
            Ok(l) => l,
            Err(e) => {
                self.show_error(&format!("Couldn't open {}", file_name(&path)), &e.to_string(), window, cx);
                return;
            }
        };

        if let Some(old) = self.doc.take() {
            for page in old.pages {
                if let Some(image) = page.image {
                    cx.drop_image(image, Some(window));
                }
            }
        }

        let pages = loaded
            .pages
            .iter()
            .map(|_| PageView {
                image: None,
                rendered_scale: 0.0,
                bounds: Rc::new(Cell::new(Bounds::default())),
            })
            .collect();
        self.status = format!("Opened {} ({} pages)", file_name(&path), loaded.pages.len()).into();
        window.set_window_title(&format!("PDF Signer — {}", file_name(&path)));
        self.doc = Some(OpenDoc { path, loaded, pages });
        self.items.clear();
        self.history.clear();
        self.selected = None;
        self.editing = None;
        self.drag = None;
        self.dirty = false;
        self.render_task = None;
        self.ensure_rendered(window, cx);
        cx.notify();
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
                let Some(rendered) = rendered else { continue };
                let Some(buffer) = image::RgbaImage::from_raw(rendered.width, rendered.height, rendered.bgra) else {
                    continue;
                };
                let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
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
        let dir = doc.path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let stem = doc.path.file_stem().and_then(|s| s.to_str()).unwrap_or("document");
        let suggested = format!("{stem}-signed.pdf");
        let picker = cx.prompt_for_new_path(&dir, Some(&suggested));

        let bytes = doc.loaded.bytes.clone();
        let pages: Vec<PageGeom> = doc.loaded.pages.clone();
        let items = self.items.clone();
        let metrics = self.metrics;

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picker.await else { return };
            let target = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let out = pdf::write(&bytes, &pages, &items, metrics)?;
                    std::fs::write(&target, out)?;
                    anyhow::Ok(())
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    this.dirty = false;
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

    fn checkpoint(&mut self) {
        self.history.push(self.items.clone());
        if self.history.len() > 200 {
            self.history.remove(0);
        }
        self.dirty = true;
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(items) = self.history.pop() {
            self.items = items;
            self.selected = None;
            self.editing = None;
            self.drag = None;
            self.dirty = true;
            cx.notify();
        }
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
        self.dirty = true;
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
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.ensure_rendered(window, cx);
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Input handlers
    // -----------------------------------------------------------------------

    fn page_mouse_down(&mut self, page: usize, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.finish_editing(cx);
        let Some((_, x, y)) = self.page_at(event.position) else { return };
        match self.tool {
            Tool::Select => {
                self.selected = None;
            }
            Tool::Text | Tool::Date => {
                let text = if self.tool == Tool::Date {
                    chrono::Local::now().format("%m/%d/%Y").to_string()
                } else {
                    String::new()
                };
                let fs = self.font_size;
                let line_h = fs * LINE_HEIGHT;
                let (w, h) = (text.chars().count().max(1) as f32 * fs * 0.55, line_h);
                // Put the click point at the vertical middle of the first line.
                self.add_item(page, x, y - line_h / 2., w, h, ItemKind::Text { text, font_size: fs }, cx);
                self.tool = Tool::Select;
            }
            Tool::Check => {
                let s = DEFAULT_CHECK_SIZE;
                self.add_item(page, x - s / 2., y - s / 2., s, s, ItemKind::Check, cx);
                self.tool = Tool::Select;
            }
        }
        cx.notify();
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
        if let Some(drag) = self.drag.take() {
            if drag.moved {
                self.history.push(drag.snapshot);
                self.dirty = true;
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

    fn paste(&mut self, cx: &mut Context<Self>) {
        if self.editing.is_none() {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
            let text = text.replace("\r\n", "\n");
            self.edit_text(cx, |t| t.push_str(&text));
        }
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tool_button = |id: &'static str, label: &'static str, tool: Tool, cx: &mut Context<Self>| {
            button(id, label, self.tool == tool).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.finish_editing(cx);
                this.tool = if this.tool == tool { Tool::Select } else { tool };
                window.focus(&this.focus);
                cx.notify();
            }))
        };
        let has_doc = self.doc.is_some();

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .bg(rgb(0xf7f7f8))
            .border_b_1()
            .border_color(rgb(0xdddde0))
            .child(button("open", "Open…", false).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.request_open(None, window, cx)
            })))
            .child(button("save", "Save…", false).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.save(window, cx)
            })))
            .child(separator())
            .when(has_doc, |bar| {
                bar.child(tool_button("tool-select", "Select", Tool::Select, cx))
                    .child(tool_button("tool-text", "Text", Tool::Text, cx))
                    .child(tool_button("tool-date", "Date", Tool::Date, cx))
                    .child(tool_button("tool-check", "✓ Check", Tool::Check, cx))
                    .child(separator())
                    .child(button("font-smaller", "A−", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.set_font_size(-1., cx)
                    })))
                    .child(div().text_sm().text_color(rgb(0x555555)).child(format!("{}pt", self.current_font_size())))
                    .child(button("font-larger", "A+", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.set_font_size(1., cx)
                    })))
                    .child(separator())
                    .child(button("zoom-out", "−", false).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.set_zoom(this.zoom / 1.2, window, cx)
                    })))
                    .child(
                        div()
                            .id("zoom-reset")
                            .min_w(px(48.))
                            .text_center()
                            .text_sm()
                            .cursor(CursorStyle::PointingHand)
                            .child(format!("{:.0}%", self.zoom * 100.))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.set_zoom(1.0, window, cx))),
                    )
                    .child(button("zoom-in", "+", false).on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.set_zoom(this.zoom * 1.2, window, cx)
                    })))
                    .child(separator())
                    .child(button("undo", "Undo", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.undo(cx))))
                    .when_some(self.selected, |bar, id| {
                        bar.child(button("delete", "Delete", false).on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| this.delete_item(id, cx),
                        )))
                    })
            })
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

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let zoom = self.zoom;
        div()
            .id("sidebar")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(210.))
            .h_full()
            .overflow_y_scroll()
            .gap_2()
            .p_3()
            .bg(rgb(0xf2f2f4))
            .border_r_1()
            .border_color(rgb(0xdddde0))
            .child(div().text_xs().font_weight(gpui::FontWeight::BOLD).text_color(rgb(0x666666)).child("SIGNATURES"))
            .child(button("add-sig", "+ Add signature image…", false).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| this.add_signature_dialog(window, cx),
            )))
            .children(self.signatures.iter().enumerate().map(|(index, sig)| {
                let (w, h) = Self::default_sig_size(sig);
                let drag = SignatureDrag {
                    sig: sig.clone(),
                    size: size(px(w * zoom), px(h * zoom)),
                };
                div()
                    .id(("sig", index))
                    .relative()
                    .h(px(72.))
                    .p_2()
                    .bg(gpui::white())
                    .border_1()
                    .border_color(rgb(0xd5d5da))
                    .rounded_md()
                    .cursor(CursorStyle::OpenHand)
                    .hover(|s| s.border_color(accent()))
                    .child(img(sig.path.clone()).size_full().object_fit(ObjectFit::Contain))
                    .on_drag(drag, |drag: &SignatureDrag, cursor_offset, _, cx| {
                        cx.new(|_| SignaturePreview {
                            sig: drag.sig.clone(),
                            size: drag.size,
                            cursor_offset,
                        })
                    })
                    .child(
                        div()
                            .id(("sig-remove", index))
                            .absolute()
                            .top(px(2.))
                            .right(px(4.))
                            .text_xs()
                            .text_color(rgb(0x999999))
                            .hover(|s| s.text_color(rgb(0xd03030)))
                            .cursor(CursorStyle::PointingHand)
                            .child("✕")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.remove_signature(index, cx))),
                    )
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x888888))
                    .child(if self.signatures.is_empty() {
                        "Add a scan or photo of your signature. White paper is removed automatically. You can also drop image files straight onto a page."
                    } else {
                        "Drag a signature onto a page. Select it to move, resize (corner handle) or delete."
                    }),
            )
    }

    fn render_item(&self, item: &Item, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let z = self.zoom;
        let id = item.id;
        let selected = self.selected == Some(id);
        let editing = self.editing == Some(id);

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
                            .when(placeholder, |d| d.child(div().text_color(rgb(0xaaaaaa)).child("Type here")))
                            .when(!line.is_empty(), |d| d.child(line))
                            .when(editing && i == last, |d| {
                                d.child(div().w(px(1.5)).h(lh).bg(accent()))
                            })
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

        let resizable = !item.is_text();
        div()
            .id(("item", id as usize))
            .absolute()
            .left(px(item.x * z - ITEM_PAD))
            .top(px(item.y * z - ITEM_PAD))
            .p(px(ITEM_PAD - 1.))
            .border_1()
            .rounded_sm()
            .border_color(if selected { accent() } else { gpui::transparent_black() })
            .when(!selected, |d| d.hover(|s| s.border_color(accent().opacity(0.4))))
            .when(selected, |d| d.bg(accent().opacity(0.05)))
            .cursor(if editing { CursorStyle::IBeam } else { CursorStyle::OpenHand })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, window, cx| this.item_mouse_down(id, DragMode::Move, e, window, cx)),
            )
            .child(content)
            .when(selected, |d| {
                d.child(
                    div()
                        .id(("delete", id as usize))
                        .absolute()
                        .top(px(-9.))
                        .right(px(-9.))
                        .size(px(16.))
                        .rounded_full()
                        .bg(rgb(0xd03030))
                        .text_color(gpui::white())
                        .text_size(px(10.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor(CursorStyle::PointingHand)
                        .child("✕")
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.delete_item(id, cx))),
                )
                .when(resizable, |d| {
                    d.child(
                        div()
                            .id(("resize", id as usize))
                            .absolute()
                            .bottom(px(-5.))
                            .right(px(-5.))
                            .size(px(10.))
                            .rounded_full()
                            .bg(accent())
                            .border_1()
                            .border_color(gpui::white())
                            .cursor(CursorStyle::ResizeUpLeftDownRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                    this.item_mouse_down(id, DragMode::Resize, e, window, cx)
                                }),
                            ),
                    )
                })
            })
    }

    fn render_pages(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(doc) = &self.doc else {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .bg(rgb(0xe4e4e8))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_3()
                        .p_8()
                        .border_2()
                        .border_dashed()
                        .border_color(rgb(0xb8b8c0))
                        .rounded_lg()
                        .text_color(rgb(0x666670))
                        .child(div().text_xl().child("Drop a PDF here"))
                        .child(div().text_sm().child("or"))
                        .child(button("open-empty", "Open PDF…", true).on_click(cx.listener(
                            |this, _: &ClickEvent, window, cx| this.request_open(None, window, cx),
                        ))),
                )
                .into_any_element();
        };

        let z = self.zoom;
        let cursor = if self.tool == Tool::Select { CursorStyle::Arrow } else { CursorStyle::Crosshair };
        let mut page_elements = Vec::with_capacity(doc.pages.len());
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
                            .shadow_md()
                            .cursor(cursor)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                    this.page_mouse_down(index, e, window, cx)
                                }),
                            )
                            .child(
                                canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
                                    .absolute()
                                    .size_full(),
                            )
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
                                        .text_color(rgb(0xaaaaaa))
                                        .child("Rendering…"),
                                )
                            })
                            .children(items);
            page_elements.push(page);
        }
        div()
            .id("pages")
            .flex_1()
            .h_full()
            .overflow_scroll()
            .bg(rgb(0xe4e4e8))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_6()
                    .p_6()
                    .min_w_full()
                    .children(page_elements),
            )
            .into_any_element()
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page_count = self.doc.as_ref().map(|d| d.pages.len()).unwrap_or(0);
        let hint = match self.tool {
            Tool::Select => None,
            Tool::Text => Some("Click on the page to add a text box"),
            Tool::Date => Some("Click on the page to add today's date"),
            Tool::Check => Some("Click on the page to add a checkmark"),
        };

        div()
            .key_context("Editor")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .text_color(rgb(0x222222))
            .font_family(".SystemUIFont")
            .on_action(cx.listener(|this, _: &Open, window, cx| this.request_open(None, window, cx)))
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &AddSignature, window, cx| this.add_signature_dialog(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| this.set_zoom(this.zoom * 1.2, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| this.set_zoom(this.zoom / 1.2, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, window, cx| this.set_zoom(1.0, window, cx)))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.handle_external_drop(paths, window, cx)))
            .on_drop(cx.listener(|this, drag: &SignatureDrag, window, cx| {
                if !this.place_signature(drag.sig.clone(), window.mouse_position(), cx) {
                    this.status = "Drop the signature onto a page.".into();
                    cx.notify();
                }
            }))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_pages(cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(0x666666))
                    .bg(rgb(0xf7f7f8))
                    .border_t_1()
                    .border_color(rgb(0xdddde0))
                    .child(hint.map(SharedString::from).unwrap_or_else(|| self.status.clone()))
                    .child(if page_count > 0 {
                        format!(
                            "{page_count} page{} · {} field{}{}",
                            if page_count == 1 { "" } else { "s" },
                            self.items.len(),
                            if self.items.len() == 1 { "" } else { "s" },
                            if self.dirty { " · unsaved" } else { "" }
                        )
                    } else {
                        String::new()
                    }),
            )
    }
}

fn button(id: &'static str, label: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .text_sm()
        .border_1()
        .cursor(CursorStyle::PointingHand)
        .when(active, |d| d.bg(accent()).text_color(gpui::white()).border_color(accent()))
        .when(!active, |d| {
            d.bg(gpui::white())
                .border_color(rgb(0xd5d5da))
                .hover(|s| s.bg(rgb(0xececf0)))
        })
        .child(label)
}

fn separator() -> impl IntoElement {
    div().w(px(1.)).h(px(20.)).mx_1().bg(rgb(0xdddde0))
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
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-=", ZoomIn, None),
        KeyBinding::new("cmd-+", ZoomIn, None),
        KeyBinding::new("cmd--", ZoomOut, None),
        KeyBinding::new("cmd-0", ZoomReset, None),
        KeyBinding::new("cmd-shift-s", AddSignature, None),
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
                ..Default::default()
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
}
