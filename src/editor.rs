//! The main editor window: toolbar, signature library sidebar and page view.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, CursorStyle, Entity, ExternalPaths, FocusHandle, Focusable,
    FontWeight, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    PathPromptOptions, Pixels, Point, PromptLevel, RenderImage, ScrollHandle, ScrollWheelEvent, SharedString,
    Subscription, Task, Window, DispatchPhase, actions, anchored, canvas, deferred, div, font, img, point, prelude::*, px, rgb, rgba,
    size,
};

use crate::model::{Watermark, Item, ItemKind, LINE_HEIGHT, PageGeom, RedactFill, Signature, TEXT_FONT, TextMetrics};

mod fields;
mod input;
use input::InputTarget;
use crate::fonts::FontBook;
use crate::forms::{self, FieldKind, FieldValue};
use crate::pdf::{self, LoadedPdf};
use crate::recents::{self, RecentEntry};
use crate::gestures;
use crate::settings::{self, Settings};
use crate::signatures;
use crate::ui;
use crate::updater::{self, Updater};

actions!(
    pdf_signer,
    [
        Open,
        Save,
        SaveWithPassword,
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
        About,
        CheckForUpdates,
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
/// Padding around the page column (`px_8` / `py_8`).
const PAGES_PAD: f32 = 32.;
/// The height of the number under a page or a thumbnail, and the space above
/// it. These and the gaps are fixed, so that where a page or thumbnail is
/// follows from the sizes of those before it (see `pages_content_bounds` and
/// `thumb_tops`) without having drawn them.
const LABEL_H: f32 = 16.;
const PAGE_LABEL_GAP: f32 = 8.;
const THUMB_LABEL_GAP: f32 = 6.;
/// The space between one page's number and the next page; between thumbnails.
const PAGE_GAP: f32 = 24.;
const THUMB_GAP: f32 = 20.;
/// From the bottom of one page to the top of the next.
const PAGE_SPACING: f32 = PAGE_LABEL_GAP + LABEL_H + PAGE_GAP;
/// Pages and thumbnails are drawn when they are within this many viewport
/// heights of the viewport; the rest only take up their space.
const DRAWN_MARGIN: f32 = 1.;
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
    Redact,
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
        Tool::Redact => ("redact", "Redact"),
    }
}

/// Watermark choices offered in the menu.
const WATERMARK_PRESETS: &[&str] = &["CONFIDENTIAL", "DRAFT", "COPY", "SAMPLE", "DO NOT COPY"];
const WATERMARK_COLORS: &[(&str, u32)] = &[("Gray", 0x8a8a8a), ("Red", 0xd93025), ("Blue", 0x1a5fd6)];
const WATERMARK_STRENGTHS: &[(&str, f32)] = &[("Light", 0.12), ("Medium", 0.22), ("Strong", 0.38)];
const WATERMARK_MAX_CHARS: usize = 40;
/// A redaction box placed with a click (rather than drawn), in points.
const DEFAULT_REDACTION: (f32, f32) = (140., 18.);

/// Everything undo/redo and "discard" restore.
#[derive(Clone, Default)]
struct Snapshot {
    items: Vec<Item>,
    watermark: Option<Watermark>,
    order: Vec<usize>,
    /// Values of the document's form fields.
    fields: Vec<FieldValue>,
}

/// The rendered watermark preview, one transparent image per page size.
#[derive(Default)]
struct WatermarkPreview {
    images: HashMap<(u32, u32), Arc<RenderImage>>,
    /// The watermark and scale last requested (rendering or rendered).
    requested: Option<(Watermark, f32)>,
    task: Option<Task<()>>,
}

/// The colour a redaction box is shown in.
fn redact_color(fill: RedactFill) -> gpui::Hsla {
    match fill {
        RedactFill::Black => gpui::black(),
        RedactFill::White => gpui::white(),
    }
}

/// Width of `text` at `size`, for laying it out on screen: as the writer will
/// draw it, or a rough estimate while the installed fonts that it needs are
/// still being found (see `Editor::load_fonts`).
fn text_width(text: &str, size: f32) -> f32 {
    pdf::measure_text_now(text, size)
        .unwrap_or_else(|| text.chars().map(|c| if c >= '\u{2e80}' { 1.0 } else { 0.55 }).sum::<f32>() * size)
}

/// The size of a text item: the box around its lines.
fn text_item_size(text: &str, font_size: f32) -> (f32, f32) {
    let lines = text.split('\n');
    let longest = lines.clone().map(|line| text_width(line, font_size)).fold(0.55 * font_size, f32::max);
    (longest, lines.count() as f32 * font_size * LINE_HEIGHT)
}

/// How long what `Editor::file_exists` found is trusted.
const FILE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The size of a page's picture in the thumbnail rail.
fn thumb_size(geom: &PageGeom) -> (f32, f32) {
    let aspect = geom.width / geom.height.max(1.);
    if aspect >= 1. || THUMB_W / aspect <= 1.5 * THUMB_W {
        (THUMB_W, THUMB_W / aspect)
    } else {
        (1.5 * THUMB_W * aspect, 1.5 * THUMB_W)
    }
}

fn page_size_key(geom: &PageGeom) -> (u32, u32) {
    ((geom.width * 100.).round() as u32, (geom.height * 100.).round() as u32)
}

/// Dragging the page canvas to scroll it.
struct PanState {
    start_mouse: Point<Pixels>,
    start_offset: Point<Pixels>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Move,
    Resize,
    /// Drawing a new redaction box from the point where the drag started.
    Draw,
}

struct DragState {
    id: u64,
    mode: DragMode,
    start_mouse: Point<Pixels>,
    start: (f32, f32, f32, f32),
    snapshot: Snapshot,
    moved: bool,
}

/// The first thumbnail of the rail as it was painted.
#[derive(Clone, Copy)]
struct PaintedThumb {
    /// Its top edge (window coordinates), and the rail's scroll offset then.
    top: Pixels,
    scroll: Pixels,
}

struct PageView {
    image: Option<Arc<RenderImage>>,
    /// Small render for the page thumbnail rail.
    thumb: Option<Arc<RenderImage>>,
    rendered_scale: f32,
}

struct OpenDoc {
    path: PathBuf,
    loaded: LoadedPdf,
    /// One per page of the original file (rendered images etc.), by original index.
    pages: Vec<PageView>,
    /// The form widgets on each page of the original file (see `forms::widgets_by_page`).
    widgets: Vec<Vec<(usize, usize)>>,
    /// The text of form fields as laid out in their widgets (see `Editor::field_layout`).
    layouts: RefCell<HashMap<(usize, usize), fields::CachedLayout>>,
    /// The document's pages as shown and saved: `order[i]` is the original index of
    /// page `i`. Deleting or moving pages only changes this list. Item page numbers
    /// and everything else in the UI use positions in this list.
    order: Vec<usize>,
    /// Where this document was last saved to (suggested on the next save).
    last_output: Option<PathBuf>,
}

impl OpenDoc {
    fn page_count(&self) -> usize {
        self.order.len()
    }

    fn geom(&self, page: usize) -> Option<&PageGeom> {
        self.loaded.pages.get(*self.order.get(page)?)
    }

    /// The displayed pages, in order.
    fn geoms(&self) -> impl Iterator<Item = &PageGeom> + '_ {
        self.order.iter().map(|&i| &self.loaded.pages[i])
    }
}

/// Asking for the password of a protected PDF, or for a new password for a
/// copy of one.
struct PasswordPrompt {
    path: PathBuf,
    text: String,
    /// The last password tried was wrong (or, for a new password, the repeat
    /// didn't match).
    wrong: bool,
    purpose: PasswordPurpose,
}

/// What to do once a document reopened from Recents is open.
#[derive(Clone, PartialEq)]
enum AfterOpen {
    /// Bring back the edits remembered in this Recents entry.
    Restore(String),
    /// Say that the saved copy was opened, as the original (this) has moved.
    NoteMoved(PathBuf),
}

#[derive(Clone, PartialEq)]
enum PasswordPurpose {
    /// Opening the document at `path`, and what to do once it's open.
    Open { then: Option<AfterOpen> },
    /// Choosing the password for a saved copy; `first` is set once it's been
    /// typed, while it's typed again to confirm.
    NewPassword { first: Option<String> },
}

/// Payload when dragging a page thumbnail to reorder pages.
#[derive(Clone)]
struct PageDrag {
    from: usize,
    image: Option<Arc<RenderImage>>,
    size: (f32, f32),
}

impl Render for PageDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(self.size.0))
            .h(px(self.size.1))
            .bg(gpui::white())
            .opacity(0.85)
            .shadow(ui::lifted_shadow())
            .when_some(self.image.clone(), |d, image| d.child(img(image).size_full().object_fit(ObjectFit::Fill)))
    }
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
    watermark: Option<Watermark>,
    /// Values of the document's own form fields (same order as `LoadedPdf::form`).
    fields: Vec<FieldValue>,
    /// Everything as of the last open/save; "Discard changes" reverts to this.
    saved: Snapshot,
    /// The form field being typed into.
    field_editing: Option<usize>,
    /// The form field (a combo box) whose list of options is open.
    choice_open: Option<usize>,
    /// The colour of new redaction boxes: the one chosen last.
    redact_fill: RedactFill,
    /// The search in the Recents gallery, and whether it's being typed into.
    recents_query: String,
    recents_search_focused: bool,
    /// While dragging a page thumbnail: the gap it would be dropped into (0 = before page 1).
    page_drop_target: Option<usize>,
    /// The mouse went down on the (self-drawn) title bar; a drag now moves the window.
    titlebar_pressed: bool,
    watermark_menu_open: bool,
    /// Typing into the watermark text box.
    watermark_editing: bool,
    /// The next keystroke replaces the whole watermark text (like a selected field).
    watermark_replace: bool,
    watermark_checkpointed: bool,
    watermark_preview: WatermarkPreview,
    /// Recently saved documents, shown in the gallery when no document is open.
    recents: Vec<RecentEntry>,
    password_prompt: Option<PasswordPrompt>,
    next_id: u64,
    selected: Option<u64>,
    editing: Option<u64>,
    typing_checkpointed: bool,
    /// Text an input method is composing (a byte range of the text being typed into).
    ime_marked: Option<(InputTarget, std::ops::Range<usize>)>,
    tool: Tool,
    zoom: f32,
    drag: Option<DragState>,
    history: Vec<Snapshot>,
    /// Undone states, for redo. Cleared by any new edit.
    future: Vec<Snapshot>,
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
    /// Where each page *position* was last painted (window coordinates). Kept per
    /// position rather than per page, so the layout measurements stay valid when
    /// pages are reordered: position 1 is always where the first page goes.
    page_slots: Vec<Rc<Cell<Bounds<Pixels>>>>,
    /// The page list's scroll offset when page bounds were last captured. Lets us
    /// correct those (stale) bounds when the list has scrolled since the last paint.
    painted_scroll: Rc<Cell<Point<Pixels>>>,
    /// Scroll state of the thumbnail rail.
    thumbs_scroll: ScrollHandle,
    /// The first thumbnail as last painted, which the others are placed from
    /// (see `thumb_tops`).
    painted_thumb: Rc<Cell<Option<PaintedThumb>>>,
    /// The zoom the pages were last painted at.
    painted_zoom: Rc<Cell<f32>>,
    pan: Option<PanState>,
    _pinch_task: Task<()>,
    status: SharedString,
    metrics: TextMetrics,
    dirty: bool,
    /// Bumped on every edit, so a finished save knows whether edits happened meanwhile.
    revision: u64,
    font_size: f32,
    /// Which of the files shown in Recents exist, and when that was looked up
    /// (see `file_exists`).
    seen_files: RefCell<(Instant, HashMap<PathBuf, bool>)>,
    render_task: Option<Task<()>>,
    /// The document being read and parsed, to be shown when that's done.
    open_task: Option<Task<()>>,
    /// The installed fonts being found (see `load_fonts`).
    fonts_task: Option<Task<()>>,
    thumb_task: Option<Task<()>>,
    _appearance_observer: Subscription,
    /// The app's updater (absent in tests), and our subscriptions to it.
    updater: Option<Entity<Updater>>,
    _updater_subscriptions: Vec<Subscription>,
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
        let updater = updater::global(cx);
        let appearance_observer = cx.observe_window_appearance(window, |_, window, cx| {
            ui::set_appearance(window.appearance());
            cx.notify();
        });

        // Measure the UI font so text lands on the same baseline in the PDF.
        let ts = window.text_system();
        let font_id = ts.resolve_font(&font(TEXT_FONT));
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
            watermark: None,
            saved: Snapshot::default(),
            page_drop_target: None,
            titlebar_pressed: false,
            watermark_menu_open: false,
            watermark_editing: false,
            watermark_replace: false,
            watermark_checkpointed: false,
            watermark_preview: WatermarkPreview::default(),
            recents: recents::load(),
            password_prompt: None,
            next_id: 1,
            selected: None,
            editing: None,
            typing_checkpointed: false,
            fields: Vec::new(),
            field_editing: None,
            choice_open: None,
            redact_fill: RedactFill::Black,
            recents_query: String::new(),
            recents_search_focused: false,
            ime_marked: None,
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
            page_slots: Vec::new(),
            painted_scroll: Rc::new(Cell::new(Point::default())),
            thumbs_scroll: ScrollHandle::new(),
            painted_thumb: Rc::default(),
            painted_zoom: Rc::new(Cell::new(0.)),
            pan: None,
            _pinch_task: {
                let mut pinches = gestures::subscribe();
                cx.spawn_in(window, async move |this, cx| {
                    while let Some(pinch) = pinches.next().await {
                        let anchor = point(px(pinch.position.0), px(pinch.position.1));
                        let alive = this.update_in(cx, |this, window, cx| this.zoom_by(pinch.factor, Some(anchor), window, cx));
                        if alive.is_err() {
                            break;
                        }
                    }
                })
            },
            status: "Ready".into(),
            metrics,
            dirty: false,
            revision: 0,
            font_size: DEFAULT_FONT_SIZE,
            seen_files: RefCell::new((Instant::now(), HashMap::new())),
            render_task: None,
            open_task: None,
            fonts_task: None,
            thumb_task: None,
            _appearance_observer: appearance_observer,
            updater: updater.clone(),
            _updater_subscriptions: updater
                .iter()
                .flat_map(|u| [cx.observe(u, |_, _, cx| cx.notify()), cx.subscribe_in(u, window, Self::on_update_event)])
                .collect(),
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

    pub fn request_open(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
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

    /// Open a PDF: it is read in the background, and shown when that's done
    /// (or its password is asked for first).
    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_with_password(path, None, None, window, cx)
    }

    /// Open a PDF, with the password typed for it if it has been asked for.
    fn open_with_password(
        &mut self,
        path: PathBuf,
        password: Option<String>,
        then: Option<AfterOpen>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = format!("Opening {}…", file_name(&path)).into();
        cx.notify();
        let tried_password = password.is_some();
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move { pdf::open_for_editing(std::fs::read(&path)?, password.as_deref()) }
        });
        // Replaces, and so abandons, a document that was still being opened.
        self.open_task = Some(cx.spawn_in(window, async move |this, cx| {
            let loaded = read.await;
            this.update_in(cx, |this, window, cx| this.finish_opening(path, loaded, tried_password, then, window, cx)).ok();
        }));
    }

    /// Show a document that has been read, or say why it couldn't be.
    fn finish_opening(
        &mut self,
        path: PathBuf,
        loaded: anyhow::Result<LoadedPdf>,
        tried_password: bool,
        then: Option<AfterOpen>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let loaded = match loaded {
            Ok(l) => l,
            Err(e) if e.downcast_ref::<pdf::PasswordError>().is_some() => {
                self.status = format!("{} is password protected", file_name(&path)).into();
                self.password_prompt = Some(PasswordPrompt {
                    path,
                    text: String::new(),
                    wrong: tried_password,
                    purpose: PasswordPurpose::Open { then },
                });
                window.focus(&self.focus);
                cx.notify();
                return;
            }
            Err(e) => {
                self.password_prompt = None;
                self.show_error(&format!("Couldn't open {}", file_name(&path)), &e.to_string(), window, cx);
                return;
            }
        };
        self.password_prompt = None;

        self.release_document(window, cx);

        let pages = loaded
            .pages
            .iter()
            .map(|_| PageView {
                image: None,
                thumb: None,
                rendered_scale: 0.0,
            })
            .collect();
        let field_count = loaded.form.len();
        // Fields may hold text in any script, which is measured with the installed fonts.
        if field_count > 0 {
            self.load_fonts(cx);
        }
        self.status = format!(
            "Opened {} ({} pages){}{}",
            file_name(&path),
            loaded.pages.len(),
            match field_count {
                0 => String::new(),
                1 => " · 1 form field to fill in".into(),
                n => format!(" · {n} form fields to fill in"),
            },
            if loaded.password.is_some() { " · password protected" } else { "" }
        )
        .into();
        self.fields = loaded.form.iter().map(|f| f.value.clone()).collect();
        self.recents_query.clear();
        self.recents_search_focused = false;
        window.set_window_title(&format!("PDF Signer — {}", file_name(&path)));
        let order = (0..loaded.pages.len()).collect::<Vec<_>>();
        self.doc = Some(OpenDoc {
            path,
            widgets: forms::widgets_by_page(&loaded.form, loaded.pages.len()),
            layouts: RefCell::default(),
            loaded,
            pages,
            order,
            last_output: None,
        });
        self.items.clear();
        self.watermark = None;
        self.saved = self.snapshot();
        self.history.clear();
        self.future.clear();
        self.close_menus();
        self.selected = None;
        self.editing = None;
        self.drag = None;
        self.dirty = false;
        self.render_task = None;
        self.pages_scroll.set_offset(point(px(0.), px(0.)));
        self.ensure_rendered(window, cx);
        self.render_thumbnails(window, cx);
        cx.notify();
        match then {
            Some(AfterOpen::Restore(id)) => {
                if let Some(entry) = self.recents.iter().find(|e| e.id == id).cloned() {
                    self.restore_recent(&entry, cx);
                }
            }
            Some(AfterOpen::NoteMoved(original)) => {
                self.status = format!(
                    "The original ({}) has moved, so the saved copy was opened instead. Its fields are part of the page now.",
                    original.display()
                )
                .into();
            }
            None => {}
        }
    }

    /// Whether a file shown in Recents is there. Every card asks on every
    /// frame, so the answers are kept for a moment instead of being read
    /// from the disk each time.
    fn file_exists(&self, path: &Path) -> bool {
        let mut seen = self.seen_files.borrow_mut();
        if seen.0.elapsed() > FILE_CHECK_INTERVAL {
            *seen = (Instant::now(), HashMap::new());
        }
        if let Some(&exists) = seen.1.get(path) {
            return exists;
        }
        let exists = path.exists();
        seen.1.insert(path.to_path_buf(), exists);
        exists
    }

    /// Find the installed fonts in the background, then draw again. Until
    /// then, text that needs them is laid out by estimate (see `text_width`).
    fn load_fonts(&mut self, cx: &mut Context<Self>) {
        if self.fonts_task.is_some() || FontBook::loaded().is_some() {
            return;
        }
        let fonts = cx.background_executor().spawn(async {
            FontBook::system();
        });
        self.fonts_task = Some(cx.spawn(async move |this, cx| {
            fonts.await;
            this.update(cx, |this, cx| {
                for item in &mut this.items {
                    if let ItemKind::Text { text, font_size } = &item.kind {
                        (item.w, item.h) = text_item_size(text, *font_size);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
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
        let preview = std::mem::take(&mut self.watermark_preview);
        for image in preview.images.into_values() {
            cx.drop_image(image, Some(window));
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
        // Recents is shown again: look at its files afresh.
        self.seen_files.borrow_mut().1.clear();
        self.fields.clear();
        self.items.clear();
        self.watermark = None;
        self.saved = Snapshot::default();
        self.history.clear();
        self.future.clear();
        self.close_menus();
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
        self.restore(self.saved.clone());
        self.close_menus();
        self.selected = None;
        self.editing = None;
        self.drag = None;
        self.dirty = false;
        self.status = format!("Changes discarded. Press {} to bring them back.", ui::keys("⌘Z")).into();
        cx.notify();
    }

    fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = &mut self.password_prompt else { return };
        if prompt.text.is_empty() {
            return;
        }
        match &mut prompt.purpose {
            PasswordPurpose::Open { then } => {
                let (path, password, then) = (prompt.path.clone(), prompt.text.clone(), then.clone());
                self.open_with_password(path, Some(password), then, window, cx);
            }
            PasswordPurpose::NewPassword { first } => {
                let typed = std::mem::take(&mut prompt.text);
                match first.take() {
                    // Now type it again.
                    None => {
                        *first = Some(typed);
                        prompt.wrong = false;
                    }
                    Some(first) if first == typed => {
                        self.password_prompt = None;
                        self.save_with_password(Some(first), window, cx);
                    }
                    // Didn't match: start over.
                    Some(_) => prompt.wrong = true,
                }
                cx.notify();
            }
        }
    }

    fn cancel_password(&mut self, cx: &mut Context<Self>) {
        self.password_prompt = None;
        cx.notify();
    }

    /// Reopen a document from the gallery with its fields still editable.
    fn open_recent(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.recents.iter().find(|e| e.id == id).cloned() else { return };
        if entry.original.exists() {
            self.open_with_password(entry.original, None, Some(AfterOpen::Restore(entry.id)), window, cx);
        } else if entry.output.exists() {
            self.open_with_password(entry.output, None, Some(AfterOpen::NoteMoved(entry.original)), window, cx);
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

    /// Bring back the fields, watermark and page order saved with a recent entry
    /// (its original has just been opened).
    fn restore_recent(&mut self, entry: &RecentEntry, cx: &mut Context<Self>) {
        if let (Some(doc), Some(order)) = (self.doc.as_mut(), entry.order.as_ref()) {
            let n = doc.pages.len();
            let mut seen = vec![false; n];
            let valid = !order.is_empty() && order.iter().all(|&i| i < n && !std::mem::replace(&mut seen[i], true));
            if valid {
                doc.order = order.clone();
            }
        }
        let page_count = self.doc.as_ref().map_or(0, |d| d.page_count());
        let mut next_id = self.next_id;
        self.items = recents::restore_items(entry, page_count, || {
            next_id += 1;
            next_id
        });
        self.next_id = next_id + 1;
        self.watermark = entry.watermark.clone();
        if let Some(doc) = &mut self.doc {
            doc.last_output = Some(entry.output.clone());
        }
        // Form values, matched by field name (the original may have changed since).
        let form = self.form();
        for (name, value) in &entry.fields {
            if let Some(i) = form.iter().position(|f| f.name == *name).filter(|&i| form[i].can_hold(value)) {
                self.fields[i] = value.clone();
            }
        }
        self.saved = self.snapshot();
        let n = self.items.len() + forms::changed(&form, &self.fields).count();
        self.status = match n {
            0 => format!("Reopened {}", file_name(&entry.original)),
            n => format!("Reopened {} with {n} editable field{}", file_name(&entry.original), if n == 1 { "" } else { "s" }),
        }
        .into();
        cx.notify();
    }

    /// Typing into the Recents search box (only while the gallery shows).
    fn searching_recents(&self) -> bool {
        self.recents_search_focused && self.doc.is_none() && !self.recents.is_empty()
    }

    fn start_recents_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.recents_search_focused = true;
        self.ime_marked = None;
        cx.notify();
    }

    /// The recent documents that match the search, most recent first.
    fn shown_recents(&self) -> Vec<&RecentEntry> {
        self.recents.iter().filter(|e| recents::matches(e, &self.recents_query)).collect()
    }

    fn clear_recents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Clear all recent documents?",
            Some("They're only removed from this list. The PDFs themselves are kept."),
            &["Clear Recents", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update(cx, |this, cx| {
                if let Err(e) = recents::clear(&this.recents) {
                    this.status = format!("Couldn't update recents: {e:#}").into();
                }
                this.recents.clear();
                this.recents_query.clear();
                this.recents_search_focused = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        let path = doc.path.clone();
        let pages: Vec<(usize, f32)> =
            doc.loaded.pages.iter().map(|p| THUMB_RENDER_PX / p.width.max(1.)).enumerate().collect();
        let pdf = doc.loaded.pdf.clone();
        self.thumb_task = Some(cx.spawn_in(window, async move |this, cx| {
            let (mut rendered, _rendering) = render_in_background(pdf, pages, cx.background_executor());
            while let Some((index, page)) = rendered.next().await {
                let Some(image) = page.and_then(to_render_image) else { continue };
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
        // Original page indices, for the displayed pages only.
        let mut stale: Vec<usize> = doc
            .order
            .iter()
            .copied()
            .filter(|&i| doc.pages[i].rendered_scale < wanted * 0.95)
            .collect();
        if stale.is_empty() {
            return;
        }
        // The page in view first, then outwards.
        let current = self.current_page().unwrap_or(0);
        let position = |source: usize| doc.order.iter().position(|&i| i == source).unwrap_or(0);
        stale.sort_by_key(|&i| position(i).abs_diff(current));
        // While zooming continuously (pinch, wheel), the stretched bitmaps are fine
        // for a moment; wait for the zoom to settle rather than re-render every step.
        // (Each call replaces `render_task`, cancelling the previous wait.) A
        // page with nothing on screen yet renders at once.
        let settle = stale.iter().all(|&i| doc.pages[i].image.is_some());
        // Render a bit above what's needed so small zoom steps don't re-render.
        let scale = (wanted * 1.25).min(MAX_RENDER_SCALE);
        let pdf = doc.loaded.pdf.clone();
        let path = doc.path.clone();
        self.render_task = Some(cx.spawn_in(window, async move |this, cx| {
            if settle {
                cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
            }
            let pages = stale.into_iter().map(|index| (index, scale)).collect();
            let (mut rendered, _rendering) = render_in_background(pdf, pages, cx.background_executor());
            while let Some((index, page)) = rendered.next().await {
                let Some(image) = page.and_then(to_render_image) else { continue };
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
        let Some(password) = doc.loaded.password.clone() else {
            self.save_with_password(None, window, cx);
            return;
        };
        // A protected document: ask how to protect the copy.
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("“{}” is password protected", file_name(&doc.path)),
            Some("Should the copy you save need the same password to open, a new one, or none?"),
            &["Keep Password", "Change Password…", "Remove Password", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else { return };
            this.update_in(cx, |this, window, cx| match answer {
                0 => this.save_with_password(Some(password), window, cx),
                1 => this.save_with_new_password(window, cx),
                2 => this.save_with_password(None, window, cx),
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// Ask for a password (typed twice), then save a copy that needs it to open.
    fn save_with_new_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        self.close_menus();
        let Some(doc) = &self.doc else {
            self.status = "Nothing to save — open a PDF first.".into();
            cx.notify();
            return;
        };
        self.password_prompt = Some(PasswordPrompt {
            path: doc.path.clone(),
            text: String::new(),
            wrong: false,
            purpose: PasswordPurpose::NewPassword { first: None },
        });
        window.focus(&self.focus);
        cx.notify();
    }

    /// Ask where to save, then save a copy, encrypted with `password` if set.
    fn save_with_password(&mut self, password: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
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
        let loaded = doc.loaded.clone();
        let order = doc.order.clone();
        let page_count = order.len();
        let items = self.items.clone();
        let form = doc.loaded.form.clone();
        let values = self.fields.clone();
        let watermark = self.watermark.clone().filter(|w| !w.text.trim().is_empty());
        let redacted_pages = items
            .iter()
            .filter(|i| i.is_redaction())
            .map(|i| i.page)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let metrics = self.metrics;
        let revision = self.revision;
        // How the copy's protection differs from the original's, for the status line.
        let protection_note = match (&doc.loaded.password, &password) {
            (Some(_), None) => " without a password",
            (Some(old), Some(new)) if old != new => " with the new password",
            (None, Some(_)) => " with a password",
            _ => "",
        };
        let protected = password.is_some();

        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picker.await else { return };
            let target = path.clone();
            let saved = Snapshot {
                items: items.clone(),
                watermark: watermark.clone(),
                order: order.clone(),
                fields: values.clone(),
            };
            // Remembered by name: the values that differ from the file's own.
            let filled: Vec<(String, FieldValue)> =
                forms::changed(&form, &values).map(|(f, v)| (f.name.clone(), v.clone())).collect();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let edits = pdf::Edits {
                        order: &order,
                        items: &items,
                        watermark: watermark.as_ref(),
                        metrics,
                        password: password.as_deref(),
                        form: &form,
                        values: &values,
                    };
                    let written = loaded.save(&edits)?;
                    std::fs::write(&target, &written.bytes)?;
                    // No thumbnail of a protected document: it would be an unprotected
                    // picture of its first page.
                    let thumbnail = if protected { None } else { recents::make_thumbnail(written.bytes) };
                    anyhow::Ok((thumbnail, written.missing))
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok((thumbnail, missing)) => {
                    // Recents are stored unencrypted, so for a protected document they
                    // remember only where it is, not what was typed into it.
                    let contents = (!protected).then(|| recents::Contents {
                        items: &saved.items,
                        watermark: saved.watermark.as_ref(),
                        fields: filled,
                        thumbnail,
                    });
                    let reordered = Some(saved.order.as_slice()).filter(|o| !o.iter().enumerate().all(|(i, &p)| i == p));
                    match recents::record(this.recents.clone(), &original, &path, page_count, reordered, contents) {
                        Ok(entries) => this.recents = entries,
                        Err(e) => eprintln!("could not update recents: {e:#}"),
                    }
                    // Unless another document was opened while the save was running.
                    if let Some(doc) = this.doc.as_mut().filter(|d| d.path == original) {
                        doc.last_output = Some(path.clone());
                        this.saved = saved;
                        // Only clear "Edited" if nothing changed meanwhile.
                        if this.revision == revision {
                            this.dirty = false;
                        }
                    }
                    this.status = match redacted_pages {
                        0 => format!("Saved {}{protection_note}", path.display()),
                        n => format!(
                            "Saved {}{protection_note} · {n} redacted page{} saved as image{} (their text is no longer selectable)",
                            path.display(),
                            if n == 1 { "" } else { "s" },
                            if n == 1 { "" } else { "s" },
                        ),
                    }
                    .into();
                    if !missing.is_empty() {
                        let chars: String = missing.iter().flat_map(|c| [*c, ' ']).collect();
                        let _ = window.prompt(
                            PromptLevel::Warning,
                            "Some characters were left out",
                            Some(&format!(
                                "No font on this computer can draw these characters, so they aren't in the saved PDF: {}\n\nEverything else was saved.",
                                chars.trim_end()
                            )),
                            &["OK"],
                            cx,
                        );
                    }
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
        (0..doc.page_count()).find_map(|i| {
            let b = self.slot_bounds(i)?;
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

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            items: self.items.clone(),
            watermark: self.watermark.clone(),
            order: self.doc.as_ref().map(|d| d.order.clone()).unwrap_or_default(),
            fields: self.fields.clone(),
        }
    }

    /// Switch to `snapshot`, returning the state it replaced.
    fn restore(&mut self, snapshot: Snapshot) -> Snapshot {
        let current = self.snapshot();
        self.items = snapshot.items;
        self.watermark = snapshot.watermark;
        if let Some(doc) = self.doc.as_mut().filter(|_| !snapshot.order.is_empty()) {
            doc.order = snapshot.order;
        }
        if snapshot.fields.len() == self.fields.len() {
            self.fields = snapshot.fields;
        }
        current
    }

    fn checkpoint(&mut self) {
        let snapshot = self.snapshot();
        self.push_history(snapshot);
    }

    /// Record a state to undo back to. Any new edit invalidates the redo stack.
    fn push_history(&mut self, snapshot: Snapshot) {
        self.history.push(snapshot);
        if self.history.len() > 200 {
            self.history.remove(0);
        }
        self.future.clear();
        self.mark_dirty();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        if let Some(snapshot) = self.history.pop() {
            let current = self.restore(snapshot);
            self.future.push(current);
            self.after_history_jump(cx);
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        if let Some(snapshot) = self.future.pop() {
            let current = self.restore(snapshot);
            self.history.push(current);
            self.after_history_jump(cx);
        }
    }

    fn after_history_jump(&mut self, cx: &mut Context<Self>) {
        self.watermark_editing = false;
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
        self.status = format!("Field copied. Press {} to paste it on any page.", ui::keys("⌘V")).into();
        cx.notify();
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        let Some(item) = self.selected_item().cloned() else { return };
        self.field_clipboard = Some(item.clone());
        self.delete_item(item.id, cx);
        self.status = format!("Field cut. Press {} to paste it.", ui::keys("⌘V")).into();
    }

    /// ⌘V: paste text while typing, otherwise paste the copied field.
    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(prompt) = &mut self.password_prompt {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                prompt.text.push_str(text.lines().next().unwrap_or_default());
                prompt.wrong = false;
                cx.notify();
            }
            return;
        }
        if let Some(target) = self.input_target() {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                let text = match target {
                    // The watermark is one line: the first of what was copied.
                    InputTarget::Watermark => text.lines().next().unwrap_or_default().to_string(),
                    _ => text,
                };
                self.replace_input(None, &text, false, cx);
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
        let page_count = self.doc.as_ref().map_or(0, |d| d.page_count());
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
    // About & updates
    // -----------------------------------------------------------------------

    /// The About box: version, author, link, and the update controls.
    fn show_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::about;
        let support = match &self.updater {
            Some(u) => u.read(cx).support(),
            None => Err(updater::NoUpdates::Development),
        };
        let auto = self.settings.check_for_updates;
        let updates = match support {
            Ok(()) if auto => "Automatic updates are on.",
            Ok(()) => "Automatic updates are off.",
            Err(reason) => reason.message(),
        };
        let detail = format!(
            "Version {}\n{}\n\nBy {}\n{}\n\n{updates}",
            about::VERSION,
            about::DESCRIPTION,
            about::author(),
            about::REPOSITORY
        );
        let mut buttons = vec!["OK"];
        if support.is_ok() {
            buttons.push("Check for Updates");
            buttons.push(if auto { "Turn Off Automatic Updates" } else { "Turn On Automatic Updates" });
        }
        buttons.push("Visit Website");
        let answer = window.prompt(PromptLevel::Info, "PDF Signer", Some(&detail), &buttons, cx);
        cx.spawn(async move |this, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = this.update(cx, |this, cx| match buttons.get(choice).copied() {
                Some("Check for Updates") => this.check_for_updates(cx),
                Some("Turn Off Automatic Updates") => this.set_automatic_updates(false, cx),
                Some("Turn On Automatic Updates") => this.set_automatic_updates(true, cx),
                Some("Visit Website") => cx.open_url(about::REPOSITORY),
                _ => {}
            });
        })
        .detach();
    }

    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if let Some(updater) = &self.updater {
            updater.update(cx, |u, cx| u.check(true, cx));
        }
    }

    fn set_automatic_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.check_for_updates = on;
        if let Err(e) = self.settings.save() {
            self.status = format!("Couldn't save settings: {e}").into();
        }
        if let Some(updater) = &self.updater {
            updater.update(cx, |u, cx| u.set_enabled(on, cx));
        }
        cx.notify();
    }

    fn on_update_event(
        &mut self,
        _: &Entity<Updater>,
        event: &updater::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            updater::Event::UpToDate => {
                let detail = format!("PDF Signer {} is the newest version.", crate::about::VERSION);
                let _ = window.prompt(PromptLevel::Info, "You're up to date", Some(&detail), &["OK"], cx);
            }
            updater::Event::Failed(message) => {
                self.show_update_error("Couldn't check for updates", message, window, cx);
            }
            updater::Event::Ready { manual: true } => self.prompt_install_update(window, cx),
            // Found by the daily check: the status bar offers it, without interrupting.
            updater::Event::Ready { manual: false } => {}
        }
    }

    fn show_update_error(&mut self, title: &str, message: &str, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(PromptLevel::Warning, title, Some(message), &["OK", "Open Downloads Page"], cx);
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(1) {
                let _ = cx.update(|cx| cx.open_url(updater::RELEASES_PAGE));
            }
        })
        .detach();
    }

    /// Offer to install the downloaded update: now (restart), later (on quit), or never.
    fn prompt_install_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(updater) = self.updater.clone() else { return };
        let Some(release) = updater.read(cx).release().cloned() else { return };
        let mut detail = release.notes.clone();
        if !detail.is_empty() {
            detail.push_str("\n\n");
        }
        detail.push_str(if self.dirty {
            "Restarting now discards your unsaved changes. Choose Later to save them first; the update is installed when you quit."
        } else {
            "PDF Signer restarts to finish. Choose Later to install it when you quit."
        });
        let restart = if self.dirty { "Discard Changes and Restart" } else { "Restart Now" };
        let title = format!("PDF Signer {} is ready to install", release.version);
        let answer = window.prompt(PromptLevel::Info, &title, Some(&detail), &[restart, "Later", "Skip This Version"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = this.update_in(cx, |this, window, cx| match choice {
                0 => this.install_update_and_restart(window, cx),
                2 => updater.update(cx, |u, cx| u.skip(cx)),
                _ => {}
            });
        })
        .detach();
    }

    fn install_update_and_restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(updater) = self.updater.clone() else { return };
        match updater.update(cx, |u, cx| u.install_and_restart(cx)) {
            Ok(()) => cx.quit(),
            Err(e) => {
                let message = format!("{e:#}\n\nYou can download the new version from GitHub instead.");
                self.show_update_error("The update couldn't be installed", &message, window, cx);
            }
        }
    }

    /// The status-bar notice for updates (empty when there's nothing to say).
    fn render_update_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let updater = self.updater.as_ref()?.read(cx);
        let manual = updater.is_manual();
        let text = |t: String| div().flex_none().text_color(ui::text_tertiary()).child(t).into_any_element();
        match updater.status() {
            updater::Status::Checking if manual => Some(text("Checking for updates…".into())),
            updater::Status::Downloading { version, fraction } if manual => {
                Some(text(format!("Downloading {version}… {:.0}%", fraction * 100.)))
            }
            updater::Status::Ready { version } => Some(
                div()
                    .id("update-ready")
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h(px(18.))
                    .rounded_full()
                    .bg(ui::accent())
                    .text_color(gpui::white())
                    .font_weight(FontWeight::MEDIUM)
                    .cursor(CursorStyle::PointingHand)
                    .hover(|s| s.bg(ui::accent_hover()))
                    .child(ui::icon("save", gpui::white()).size(px(11.)))
                    .child(format!("Update to {version}"))
                    .tooltip(ui::tooltip("A new version is downloaded and ready to install", None))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.prompt_install_update(window, cx)))
                    .into_any_element(),
            ),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Deleting & reordering pages
    // -----------------------------------------------------------------------

    /// Delete page `page` (and the fields on it). Undoable.
    fn delete_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let Some(count) = self.doc.as_ref().map(|d| d.page_count()) else { return };
        if page >= count {
            return;
        }
        if count == 1 {
            self.status = "A PDF needs at least one page.".into();
            cx.notify();
            return;
        }
        self.finish_editing(cx);
        self.checkpoint();
        self.items.retain(|i| i.page != page);
        for item in &mut self.items {
            if item.page > page {
                item.page -= 1;
            }
        }
        if let Some(doc) = &mut self.doc {
            doc.order.remove(page);
        }
        self.selected = None;
        self.status = format!("Deleted page {}. Press {} to bring it back.", page + 1, ui::keys("⌘Z")).into();
        cx.notify();
    }

    /// Move page `from` into gap `gap` (0 = before the first page, n = after the
    /// last). Fields move with their page. Undoable.
    fn move_page(&mut self, from: usize, gap: usize, cx: &mut Context<Self>) {
        let Some(count) = self.doc.as_ref().map(|d| d.page_count()) else { return };
        if from >= count || gap > count || gap == from || gap == from + 1 {
            return;
        }
        let to = if gap > from { gap - 1 } else { gap };
        self.finish_editing(cx);
        self.checkpoint();
        // `positions[new] = old`.
        let mut positions: Vec<usize> = (0..count).collect();
        let moved = positions.remove(from);
        positions.insert(to, moved);
        let mut new_of_old = vec![0; count];
        for (new, &old) in positions.iter().enumerate() {
            new_of_old[old] = new;
        }
        for item in &mut self.items {
            item.page = new_of_old[item.page];
        }
        if let Some(doc) = &mut self.doc {
            let old = doc.order.clone();
            doc.order = positions.iter().map(|&p| old[p]).collect();
        }
        self.status = format!("Moved page {} to position {}", from + 1, to + 1).into();
        cx.notify();
    }

    /// The gap between thumbnails closest to window y (0 = above the first).
    fn page_gap_at(&self, y: Pixels) -> usize {
        let Some(tops) = self.thumb_tops() else { return 0 };
        tops.filter(|&(top, height)| top + height / 2. < y).count()
    }

    /// Where each thumbnail is in the rail right now: its top edge (window
    /// coordinates) and height, number included. Worked out from the layout
    /// rules and the first thumbnail as last painted, as only the thumbnails
    /// near the viewport are drawn. `None` before anything has been painted.
    fn thumb_tops(&self) -> Option<impl Iterator<Item = (Pixels, Pixels)> + '_> {
        let doc = self.doc.as_ref()?;
        let painted = self.painted_thumb.get()?;
        let mut top = painted.top - painted.scroll + self.thumbs_scroll.offset().y;
        Some(doc.geoms().map(move |geom| {
            let height = px(thumb_size(geom).1 + THUMB_LABEL_GAP + LABEL_H);
            let block = (top, height);
            top += height + px(THUMB_GAP);
            block
        }))
    }

    /// Which thumbnails to draw (see `DRAWN_MARGIN`). The first always is: the
    /// others are placed from it.
    fn drawn_thumbs(&self) -> Vec<bool> {
        let count = self.doc.as_ref().map_or(0, |d| d.page_count());
        let viewport = self.thumbs_scroll.bounds();
        let Some(tops) = self.thumb_tops().filter(|_| viewport.size.height > px(0.)) else { return vec![true; count] };
        let margin = viewport.size.height * DRAWN_MARGIN;
        tops.enumerate()
            .map(|(index, (top, height))| index == 0 || (top + height >= viewport.top() - margin && top <= viewport.bottom() + margin))
            .collect()
    }

    /// Which pages to draw (see `DRAWN_MARGIN`). The first always is (the
    /// others are placed from it, see `layout_metrics`), and so is the page
    /// with what is being typed into, which takes the typing. Before the
    /// pages have been painted, all of them.
    fn drawn_pages(&self) -> Vec<bool> {
        let Some(doc) = &self.doc else { return Vec::new() };
        let count = doc.page_count();
        let viewport = self.pages_scroll.bounds();
        let measured = self.painted_zoom.get() > 0. && self.slot_bounds(0).is_some();
        if !measured || viewport.size.height <= px(0.) {
            return vec![true; count];
        }
        let margin = viewport.size.height * DRAWN_MARGIN;
        let typing = self.editing.and_then(|id| self.items.iter().find(|i| i.id == id)).map(|i| i.page);
        let field = self.field_editing.or(self.choice_open).and_then(|i| doc.loaded.form.get(i));
        let holds_field = |index: usize| field.is_some_and(|f| f.widgets.iter().any(|w| w.page == doc.order[index]));
        self.live_pages_bounds()
            .enumerate()
            .map(|(index, b)| {
                index == 0
                    || (b.bottom() >= viewport.top() - margin && b.top() <= viewport.bottom() + margin)
                    || typing == Some(index)
                    || holds_field(index)
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Pages & zoom
    // -----------------------------------------------------------------------

    /// Two layout measurements that don't depend on zoom: the content-space
    /// top of the first page (from the last paint), and the space between
    /// consecutive pages (page label + gaps).
    fn layout_metrics(&self) -> (Pixels, Pixels) {
        let gap = px(PAGE_SPACING);
        let first = self.slot_bounds(0).filter(|_| self.doc.is_some() && self.painted_zoom.get() > 0.);
        let lead = first.map(|first| first.top() - self.pages_scroll.bounds().top() - self.painted_scroll.get().y);
        (lead.unwrap_or(px(PAGES_PAD)), gap)
    }

    /// Where page `index` sits within the scrolled content at zoom `z`. Computed
    /// from the layout rules rather than the last paint, so it is right even
    /// before the pages have been painted at a new zoom or scroll position.
    fn page_content_bounds(&self, index: usize, z: f32) -> Option<Bounds<Pixels>> {
        self.pages_content_bounds(z).nth(index)
    }

    /// `page_content_bounds` of every page, from the first down.
    fn pages_content_bounds(&self, z: f32) -> impl Iterator<Item = Bounds<Pixels>> + '_ {
        let (lead, gap) = self.layout_metrics();
        let widest = self.doc.iter().flat_map(|d| d.geoms()).map(|p| p.width).fold(0., f32::max);
        // The page column is at least as wide as the viewport, and pages are centred in it.
        let content_w = self.pages_scroll.bounds().size.width.max(px(widest * z + 2. * PAGES_PAD));
        let mut top = lead;
        self.doc.iter().flat_map(|d| d.geoms()).map(move |geom| {
            let (w, h) = (px(geom.width * z), px(geom.height * z));
            let bounds = Bounds::new(point((content_w - w) / 2., top), size(w, h));
            top += h + gap;
            bounds
        })
    }

    /// Where position `index` was last painted.
    fn slot_bounds(&self, index: usize) -> Option<Bounds<Pixels>> {
        self.page_slots.get(index).map(|b| b.get()).filter(|b| b.size.width > px(0.))
    }

    /// Where page `index` is on screen right now (window coordinates).
    fn live_page_bounds(&self, index: usize) -> Option<Bounds<Pixels>> {
        self.live_pages_bounds().nth(index)
    }

    /// `live_page_bounds` of every page, from the first down.
    fn live_pages_bounds(&self) -> impl Iterator<Item = Bounds<Pixels>> + '_ {
        let origin = self.pages_scroll.bounds().origin + self.pages_scroll.offset();
        self.pages_content_bounds(self.zoom).map(move |content| Bounds::new(content.origin + origin, content.size))
    }

    /// The page occupying most of the viewport.
    fn current_page(&self) -> Option<usize> {
        self.doc.as_ref()?;
        let viewport = self.pages_scroll.bounds();
        if viewport.size.height <= px(0.) {
            return Some(0);
        }
        self.live_pages_bounds()
            .enumerate()
            .map(|(i, b)| {
                let visible = (b.bottom().min(viewport.bottom()) - b.top().max(viewport.top())).max(px(0.));
                (i, visible)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
    }

    fn scroll_to_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let Some(bounds) = self.live_page_bounds(page) else { return };
        let viewport = self.pages_scroll.bounds();
        let offset = self.pages_scroll.offset();
        // Bring the page's top edge to just below the top of the viewport.
        let delta = bounds.top() - viewport.top() - px(24.);
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
        let widest = doc.geoms().map(|p| p.width).fold(1., f32::max);
        let zoom = match mode {
            ZoomMode::Fixed => return None,
            ZoomMode::FitWidth => avail_w / widest,
            ZoomMode::FitPage => {
                let page = doc.geom(self.current_page().unwrap_or(0))?;
                (avail_w / page.width).min(avail_h / page.height)
            }
        };
        Some(zoom.clamp(MIN_ZOOM, MAX_ZOOM))
    }

    fn set_zoom_mode(&mut self, mode: ZoomMode, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_mode = mode;
        self.zoom_menu_open = false;
        if let Some(zoom) = self.fit_zoom(mode) {
            let page = self.current_page();
            self.apply_zoom(zoom, None, window, cx);
            // "Fit page" is about seeing one whole page, so line it up.
            if let (ZoomMode::FitPage, Some(page)) = (mode, page) {
                self.scroll_to_page(page, cx);
            }
        }
        cx.notify();
    }

    /// Multiply the zoom (wheel / pinch), keeping `anchor` fixed on screen.
    fn zoom_by(&mut self, factor: f32, anchor: Option<Point<Pixels>>, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        self.zoom_mode = ZoomMode::Fixed;
        self.zoom_menu_open = false;
        self.apply_zoom(self.zoom * factor, anchor, window, cx);
    }

    /// Change the zoom so that the document point under `anchor` (default: the
    /// viewport centre) stays where it is on screen.
    fn apply_zoom(&mut self, zoom: f32, anchor: Option<Point<Pixels>>, window: &mut Window, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let viewport = self.pages_scroll.bounds();
        let anchor = anchor.filter(|a| viewport.contains(a)).unwrap_or_else(|| viewport.center());
        if let Some(offset) = self.offset_keeping(anchor, zoom) {
            self.pages_scroll.set_offset(offset);
        }
        self.zoom = zoom;
        self.ensure_rendered(window, cx);
        cx.notify();
    }

    /// The scroll offset that keeps the document point under `anchor` in place
    /// after zooming to `new_zoom`. (GPUI clamps it to the scrollable range.)
    fn offset_keeping(&self, anchor: Point<Pixels>, new_zoom: f32) -> Option<Point<Pixels>> {
        let viewport = self.pages_scroll.bounds();
        if viewport.size.width <= px(0.) {
            return None;
        }
        // The page nearest the anchor, and the anchor's position on it in page points
        // (it may lie a little outside the page, e.g. in the gap between pages).
        let distance = |b: &Bounds<Pixels>| (b.top() - anchor.y).max(anchor.y - b.bottom()).max(px(0.));
        let (index, bounds) = self
            .live_pages_bounds()
            .enumerate()
            .min_by(|a, b| distance(&a.1).partial_cmp(&distance(&b.1)).unwrap_or(std::cmp::Ordering::Equal))?;
        let on_page = (anchor - bounds.origin).map(|v| v / px(self.zoom));
        let after = self.page_content_bounds(index, new_zoom)?;
        let anchor_in_content = after.origin + on_page.map(|v| px(v * new_zoom));
        Some((anchor - viewport.origin) - anchor_in_content)
    }

    // -----------------------------------------------------------------------
    // Panning
    // -----------------------------------------------------------------------

    fn can_pan(&self) -> bool {
        let max = self.pages_scroll.max_offset();
        max.width > px(0.) || max.height > px(0.)
    }

    fn start_pan(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.finish_editing(cx);
        self.close_menus();
        self.selected = None;
        if self.drag.is_none() {
            self.pan = Some(PanState {
                start_mouse: event.position,
                start_offset: self.pages_scroll.offset(),
            });
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
        self.doc.as_ref()?.geom(page)
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
        self.ime_marked = None;
        if self.field_editing.take().is_some() {
            cx.notify();
        }
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
        (item.w, item.h) = text_item_size(text, *font_size);
        if pdf::needs_installed_fonts(text) {
            self.load_fonts(cx);
        }
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

    /// Colour the selected redaction box, and the ones drawn next.
    fn set_redact_fill(&mut self, fill: RedactFill, cx: &mut Context<Self>) {
        self.redact_fill = fill;
        let Some(id) = self.selected else { return };
        if self.items.iter().any(|i| i.id == id && i.redaction().is_some_and(|f| f != fill)) {
            self.checkpoint();
            if let Some(item) = self.item_mut(id) {
                item.kind = ItemKind::Redact { fill };
            }
        }
        cx.notify();
    }

    fn set_zoom(&mut self, zoom: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_mode = ZoomMode::Fixed;
        self.zoom_menu_open = false;
        self.apply_zoom(zoom, None, window, cx);
    }

    // -----------------------------------------------------------------------
    // Input handlers
    // -----------------------------------------------------------------------

    fn page_mouse_down(&mut self, _page: usize, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        self.finish_editing(cx);
        self.close_menus();
        let Some((page, x, y)) = self.page_at(event.position) else { return };
        match self.tool {
            // Let the press reach the canvas, which starts panning.
            Tool::Select => self.selected = None,
            // Draw a box. Like the other tools, it's one box per use (a sticky tool
            // turns the next "click to deselect" into an unwanted box).
            Tool::Redact => {
                cx.stop_propagation();
                self.tool = Tool::Select;
                let id = self.add_item(page, x, y, 0., 0., ItemKind::Redact { fill: self.redact_fill }, cx);
                self.drag = Some(DragState {
                    id,
                    mode: DragMode::Draw,
                    start_mouse: event.position,
                    start: (x, y, 0., 0.),
                    snapshot: self.snapshot(),
                    moved: false,
                });
            }
            tool => {
                cx.stop_propagation();
                self.place_tool(tool, page, x, y, cx);
                self.tool = Tool::Select;
            }
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
                let (w, line_h) = text_item_size(&text, fs);
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
            Tool::Redact => {
                let (w, h) = DEFAULT_REDACTION;
                self.add_item(page, x - w / 2., y - h / 2., w, h, ItemKind::Redact { fill: self.redact_fill }, cx);
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
        self.watermark_menu_open = false;
        self.watermark_editing = false;
        self.choice_open = None;
    }

    fn any_menu_open(&self) -> bool {
        self.zoom_menu_open || self.date_menu_open || self.watermark_menu_open || self.choice_open.is_some()
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
            snapshot: self.snapshot(),
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
        if let Some(pan) = &self.pan {
            if event.pressed_button == Some(MouseButton::Left) {
                let offset = pan.start_offset + (event.position - pan.start_mouse);
                self.pages_scroll.set_offset(offset);
            } else {
                self.pan = None;
            }
            cx.notify();
            return;
        }
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
            DragMode::Draw => {
                item.x = x0.min(x0 + dx);
                item.y = y0.min(y0 + dy);
                item.w = dx.abs();
                item.h = dy.abs();
                if let Some(g) = self.page_geom(item.page) {
                    item.x = item.x.max(0.);
                    item.y = item.y.max(0.);
                    item.w = item.w.min(g.width - item.x);
                    item.h = item.h.min(g.height - item.y);
                }
            }
            DragMode::Resize if item.is_redaction() => {
                item.w = (w0 + dx).max(4.);
                item.h = (h0 + dy).max(4.);
                if let Some(g) = self.page_geom(item.page) {
                    item.w = item.w.min(g.width - item.x);
                    item.h = item.h.min(g.height - item.y);
                }
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
        if self.page_drop_target.take().is_some() {
            cx.notify();
        }
        if self.pan.take().is_some() {
            cx.notify();
        }
        if let Some(drag) = self.drag.take() {
            if drag.mode == DragMode::Draw {
                // `add_item` already recorded the undo step. A click (or a tiny drag)
                // places a default-sized box instead.
                let (x0, y0, _, _) = drag.start;
                if let Some(mut item) = self.items.iter().find(|i| i.id == drag.id).cloned() {
                    if item.w < 4. || item.h < 4. {
                        let (w, h) = DEFAULT_REDACTION;
                        (item.x, item.y, item.w, item.h) = (x0 - w / 2., y0 - h / 2., w, h);
                        self.clamp_item(&mut item);
                        if let Some(slot) = self.item_mut(drag.id) {
                            *slot = item;
                        }
                    }
                }
            } else if drag.moved {
                self.push_history(drag.snapshot);
            }
            cx.notify();
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let shortcut = ks.modifiers.platform || ks.modifiers.control;

        // The password prompt takes all typing while it's open. Its text goes
        // straight in (no input method), as in system password fields.
        if let Some(prompt) = &mut self.password_prompt {
            match ks.key.as_str() {
                "escape" => self.password_prompt = None,
                "enter" => self.submit_password(window, cx),
                "backspace" => {
                    prompt.text.pop();
                }
                _ if !shortcut => {
                    let Some(ch) = ks.key_char.clone().filter(|c| !c.chars().any(char::is_control)) else { return };
                    prompt.text.push_str(&ch);
                    prompt.wrong = false;
                }
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if ks.key == "backspace" && self.input_target().is_some() {
            self.delete_backward(cx);
            cx.stop_propagation();
            return;
        }

        // The Recents gallery: typing anywhere searches it.
        if self.doc.is_none() && !self.recents.is_empty() {
            if self.searching_recents() {
                match ks.key.as_str() {
                    "escape" if self.recents_query.is_empty() => self.recents_search_focused = false,
                    "escape" => self.recents_query.clear(),
                    "enter" => {
                        if let Some(id) = self.shown_recents().first().map(|e| e.id.clone()) {
                            self.open_recent(&id, window, cx);
                        }
                    }
                    "tab" => {}
                    // Characters arrive through the input handler.
                    _ => return,
                }
                self.ime_marked = None;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if shortcut && ks.key == "f" {
                self.start_recents_search(window, cx);
                cx.stop_propagation();
                return;
            }
            let typed = ks.key_char.clone().filter(|c| !c.trim().is_empty() && !c.chars().any(char::is_control));
            if let (false, Some(typed)) = (shortcut, typed) {
                self.recents_query.push_str(&typed);
                self.start_recents_search(window, cx);
                cx.stop_propagation();
                return;
            }
        }

        if let Some(index) = self.field_editing {
            let multiline = self.form().get(index).is_some_and(|f| matches!(f.kind, FieldKind::Text { multiline: true, .. }));
            match ks.key.as_str() {
                "enter" if multiline && !shortcut => {
                    if let Some(text) = self.field_input_text(index) {
                        self.type_into_field(index, text + "\n", cx);
                    }
                }
                "escape" | "enter" => {
                    self.finish_editing(cx);
                    self.close_menus();
                }
                "tab" => self.next_field(ks.modifiers.shift, window, cx),
                // Typed characters arrive through the input handler (see `input.rs`).
                _ => return,
            }
            cx.stop_propagation();
            return;
        }

        if self.editing.is_some() {
            match ks.key.as_str() {
                "escape" => self.finish_editing(cx),
                "enter" => self.edit_text(cx, |t| t.push('\n')),
                "tab" => {}
                // Typed characters arrive through the input handler (see `input.rs`).
                _ => return,
            }
            cx.stop_propagation();
            return;
        }

        if self.watermark_editing {
            match ks.key.as_str() {
                "escape" | "enter" => {
                    self.watermark_editing = false;
                    cx.notify();
                }
                "tab" => {}
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        if ks.key == "escape" && self.any_menu_open() {
            self.close_menus();
            cx.notify();
            return;
        }
        if ks.key == "tab" && self.selected.is_none() && !shortcut {
            self.next_field(ks.modifiers.shift, window, cx);
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

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    fn render_titlebar(&self, client_decorated: bool, maximized: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let has_doc = self.doc.is_some();
        let (title, subtitle) = match &self.doc {
            Some(doc) => {
                let n = doc.page_count();
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
            // Room for the traffic lights on macOS.
            .pl(px(if cfg!(target_os = "macos") { 84. } else { 14. }))
            .pr_3()
            .bg(ui::chrome_bg())
            .border_b_1()
            .border_color(ui::border())
            .on_click(move |e: &ClickEvent, window, _| {
                if e.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        window.titlebar_double_click();
                    } else if client_decorated {
                        window.zoom_window();
                    }
                }
            })
            // Where the app draws its own title bar (Linux compositors without
            // server-side decorations), dragging it moves the window. The move
            // starts on the first drag step so double-clicks still arrive.
            .when(client_decorated, |d| {
                d.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, _| this.titlebar_pressed = true),
                )
                .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, _| this.titlebar_pressed = false))
                .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, _| {
                    if std::mem::take(&mut this.titlebar_pressed) && e.dragging() {
                        window.start_window_move();
                    }
                }))
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
                                // Rapid clicks here must not register as a title-bar double-click.
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
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
                                            let open = !this.zoom_menu_open;
                                            this.close_menus();
                                            this.zoom_menu_open = open;
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
                    }),
                // No document: the start screen (gallery or empty state) has its own open action.
            )
            .when(client_decorated, |d| {
                let control = |id: &'static str, icon: &'static str| {
                    ui::button_base(id)
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(28.))
                        .rounded_full()
                        .cursor(CursorStyle::PointingHand)
                        .hover(|s| s.bg(ui::hover_bg()))
                        .child(ui::icon(icon, ui::text_secondary()).size(px(14.)))
                };
                d.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .ml_2()
                        .child(control("win-minimize", "window-minimize").on_click(|_, window, _| window.minimize_window()))
                        .child(
                            control("win-maximize", if maximized { "window-restore" } else { "window-maximize" })
                                .on_click(|_, window, _| window.zoom_window()),
                        )
                        .child(control("win-close", "close").on_click(|_, window, _| window.remove_window())),
                )
            })
    }

    /// Wrap a menu panel so it floats above everything, anchored below its trigger
    /// (`align_right`: right edges line up), and closes on any click outside it.
    fn floating_menu(&self, panel: gpui::Div, align_right: bool, cx: &mut Context<Self>) -> AnyElement {
        let panel = panel.occlude().on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
            if this.any_menu_open() {
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

    // -----------------------------------------------------------------------
    // Watermark
    // -----------------------------------------------------------------------

    /// Change a watermark setting (adding the default watermark if there is none).
    fn update_watermark(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Watermark)) {
        self.checkpoint();
        self.watermark_editing = false;
        f(self.watermark.get_or_insert_with(Watermark::default));
        cx.notify();
    }

    fn remove_watermark(&mut self, cx: &mut Context<Self>) {
        if self.watermark.is_some() {
            self.checkpoint();
            self.watermark = None;
        }
        self.close_menus();
        cx.notify();
    }

    fn start_watermark_typing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        self.selected = None;
        self.watermark_editing = true;
        self.watermark_replace = true;
        self.watermark_checkpointed = false;
        window.focus(&self.focus);
        cx.notify();
    }

    fn edit_watermark_text(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut String)) {
        if !self.watermark_checkpointed {
            self.checkpoint();
            self.watermark_checkpointed = true;
        } else {
            self.mark_dirty();
        }
        let watermark = self.watermark.get_or_insert_with(Watermark::default);
        if std::mem::take(&mut self.watermark_replace) {
            watermark.text.clear();
        }
        f(&mut watermark.text);
        if watermark.text.chars().count() > WATERMARK_MAX_CHARS {
            watermark.text = watermark.text.chars().take(WATERMARK_MAX_CHARS).collect();
        }
        cx.notify();
    }

    /// Keep the watermark preview images in step with the watermark and zoom.
    /// Called from `render`, so undo/redo, typing and zooming all just work.
    fn sync_watermark_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
        let Some(watermark) = self.watermark.clone().filter(|w| !w.text.trim().is_empty()) else {
            let preview = std::mem::take(&mut self.watermark_preview);
            for image in preview.images.into_values() {
                cx.drop_image(image, Some(window));
            }
            return;
        };
        // Big soft lettering: up to 3× is plenty, even zoomed far in.
        let scale = (self.zoom * window.scale_factor() * 1.25).clamp(1.0, 3.0);
        if let Some((requested, requested_scale)) = &self.watermark_preview.requested {
            if *requested == watermark && *requested_scale >= scale * 0.8 && *requested_scale <= scale * 2.5 {
                return;
            }
        }
        let mut sizes: Vec<((u32, u32), f32, f32)> = Vec::new();
        for geom in &doc.loaded.pages {
            let key = page_size_key(geom);
            if !sizes.iter().any(|(k, _, _)| *k == key) {
                sizes.push((key, geom.width, geom.height));
            }
        }
        self.watermark_preview.requested = Some((watermark.clone(), scale));
        self.watermark_preview.task = Some(cx.spawn_in(window, async move |this, cx| {
            let wm = watermark.clone();
            let rendered = cx
                .background_executor()
                .spawn(async move {
                    sizes
                        .into_iter()
                        .filter_map(|(key, w, h)| Some((key, pdf::render_watermark(w, h, &wm, scale)?)))
                        .collect::<Vec<_>>()
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                // Superseded by a newer request (or the watermark was removed)?
                if this.watermark_preview.requested.as_ref() != Some(&(watermark, scale)) {
                    return;
                }
                for image in std::mem::take(&mut this.watermark_preview.images).into_values() {
                    cx.drop_image(image, Some(window));
                }
                for (key, page) in rendered {
                    if let Some(image) = to_render_image(page) {
                        this.watermark_preview.images.insert(key, image);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_watermark_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.watermark.clone();
        let editing = self.watermark_editing;
        let text = current.as_ref().map(|w| w.text.clone()).unwrap_or_default();
        let selected_text = editing && self.watermark_replace && !text.is_empty();

        let input = ui::text_field(if editing { ui::accent() } else { ui::border_strong() })
            .id("watermark-text")
            .mx_1()
            .mb_1()
            .cursor(CursorStyle::IBeam)
            .overflow_hidden()
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.start_watermark_typing(window, cx)))
            .when(text.is_empty(), |d| {
                d.child(div().text_color(ui::text_tertiary()).child(if editing { "" } else { "Type your own…" }))
            })
            .when(!text.is_empty(), |d| {
                d.child(
                    div()
                        .whitespace_nowrap()
                        .when(selected_text, |d| d.bg(ui::accent().opacity(0.25)).rounded(px(2.)))
                        .child(text.clone()),
                )
            })
            .when(editing && !selected_text, |d| d.child(ui::caret(px(16.))))
            .when(editing, |d| d.relative().child(self.input_anchor(cx)));

        let mut panel = ui::menu_panel()
            .w(px(250.))
            .child(ui::menu_caption("Watermark text"))
            .child(input);
        for (i, preset) in WATERMARK_PRESETS.iter().enumerate() {
            let checked = current.as_ref().is_some_and(|w| w.text == *preset);
            panel = panel.child(ui::menu_item(("wm-preset", i), *preset, checked, None).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| this.update_watermark(cx, |w| w.text = preset.to_string()),
            )));
        }

        let color = current.as_ref().map(|w| w.color);
        let swatches = WATERMARK_COLORS.iter().enumerate().map(|(i, &(name, rgb_value))| {
            let active = color == Some(rgb_value);
            ui::button_base(("wm-color", i))
                .size(px(22.))
                .rounded_full()
                .bg(rgb(rgb_value))
                .cursor(CursorStyle::PointingHand)
                .border_2()
                .border_color(if active { ui::accent() } else { ui::surface() })
                .tooltip(ui::tooltip(name, None))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.update_watermark(cx, |w| w.color = rgb_value)))
        });
        let opacity = current.as_ref().map(|w| w.opacity);
        let strengths = WATERMARK_STRENGTHS.iter().enumerate().map(|(i, &(name, value))| {
            let active = opacity.is_some_and(|o| (o - value).abs() < 0.01);
            ui::chip(("wm-strength", i), name, active)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.update_watermark(cx, |w| w.opacity = value)))
        });
        let diagonal = current.as_ref().map(|w| w.diagonal);
        let directions = [("Diagonal", true), ("Across", false)].into_iter().enumerate().map(|(i, (name, value))| {
            ui::chip(("wm-direction", i), name, diagonal == Some(value))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.update_watermark(cx, |w| w.diagonal = value)))
        });

        panel = panel
            .child(ui::menu_separator())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(div().text_xs().text_color(ui::text_secondary()).child("Colour"))
                    .child(div().flex().gap_1p5().children(swatches)),
            )
            .child(ui::menu_caption("Strength"))
            .child(div().flex().gap_1().px_1().children(strengths))
            .child(ui::menu_caption("Direction"))
            .child(div().flex().gap_1().px_1().pb_1().children(directions));
        if current.is_some() {
            panel = panel.child(ui::menu_separator()).child(
                ui::menu_item("wm-remove", "Remove watermark", false, None)
                    .text_color(ui::danger())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.remove_watermark(cx))),
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
        let mut rows = Vec::new();
        for tool in [Tool::Text, Tool::Date, Tool::Check] {
            rows.push(self.render_tool_row(tool, cx));
        }
        Self::sidebar_section("Fill in", rows)
    }

    fn sidebar_section(title: &'static str, rows: Vec<AnyElement>) -> AnyElement {
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
                    .child(title),
            )
            .children(rows)
            .into_any_element()
    }

    /// "Protect": redaction and the watermark.
    fn render_protect_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_doc = self.doc.is_some();
        let watermark_on = self.watermark.as_ref().is_some_and(|w| !w.text.trim().is_empty());
        let sublabel: SharedString = match &self.watermark {
            Some(w) if watermark_on => w.text.clone().into(),
            _ => "None".into(),
        };
        let watermark_row = Self::sidebar_row("protect-watermark", "watermark", "Watermark", sublabel, watermark_on, has_doc)
            .relative()
            .child(ui::icon("chevron-down", ui::text_tertiary()).size(px(12.)).mr_1())
            .when(has_doc, |d| {
                d.cursor(CursorStyle::PointingHand)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        if std::mem::take(&mut this.menu_dismissed) {
                            return;
                        }
                        let open = !this.watermark_menu_open;
                        this.close_menus();
                        this.watermark_menu_open = open;
                        window.focus(&this.focus);
                        cx.notify();
                    }))
            })
            .when(self.watermark_menu_open, |d| d.child(self.render_watermark_menu(cx)))
            .into_any_element();
        let protected = self.doc.as_ref().is_some_and(|d| d.loaded.password.is_some());
        let password_row = Self::sidebar_row(
            "protect-password",
            "lock",
            "Password",
            if protected { "Saving asks to keep, change or remove it" } else { "Save a copy that needs one" }.into(),
            false,
            has_doc,
        )
        .when(has_doc, |d| {
            d.cursor(CursorStyle::PointingHand)
                .tooltip(ui::tooltip("Save with a password", Some("⌥⌘S")))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.save_with_new_password(window, cx)))
        })
        .into_any_element();
        Self::sidebar_section("Protect", vec![self.render_tool_row(Tool::Redact, cx), watermark_row, password_row])
    }

    /// The shared look of a sidebar row: icon tile, label and a one-line description.
    fn sidebar_row(
        id: &'static str,
        icon_name: &'static str,
        label: &'static str,
        sublabel: SharedString,
        active: bool,
        enabled: bool,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_2p5()
            .h(px(42.))
            .px_2()
            .rounded_md()
            .when(active, |d| d.bg(ui::accent_soft()))
            .when(!active && enabled, |d| d.hover(|s| s.bg(ui::hover_bg())))
            .when(!enabled, |d| d.opacity(0.45))
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
    }

    /// A tool row: click it then the page, or drag it onto the page.
    fn render_tool_row(&self, tool: Tool, cx: &mut Context<Self>) -> AnyElement {
        let has_doc = self.doc.is_some();
        {
            let (icon_name, label) = tool_label(tool);
            let active = self.tool == tool;
            let sublabel: SharedString = match tool {
                Tool::Date => self.settings.today().into(),
                Tool::Text => "Type anywhere on the page".into(),
                Tool::Redact => "Cover & remove for good".into(),
                _ => "Tick a box".into(),
            };
            let id = match tool {
                Tool::Text => "fill-Text",
                Tool::Date => "fill-Date",
                Tool::Check => "fill-Checkmark",
                Tool::Redact => "protect-Redact",
                Tool::Select => "fill-Select",
            };
            let row = Self::sidebar_row(id, icon_name, label, sublabel, active, has_doc)
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
                                let open = !this.date_menu_open;
                                this.close_menus();
                                this.date_menu_open = open;
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
            row.into_any_element()
        }
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
                        ui::floating_icon_button(("sig-remove", index), "trash", ui::danger())
                            .absolute()
                            .top(px(4.))
                            .right(px(4.))
                            .size(px(24.))
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
            .child(self.render_protect_section(cx))
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
                let placeholder = editing && text.is_empty();
                // Text an input method is still composing is shown underlined.
                let marked = if editing { self.marked_range(text) } else { None };
                let mut start = 0;
                let lines: Vec<(String, String, String)> = text
                    .split('\n')
                    .map(|line| {
                        let range = start..start + line.len();
                        start = range.end + 1;
                        match &marked {
                            Some(m) if m.start >= range.start && m.end <= range.end && !m.is_empty() => {
                                let (a, b) = (m.start - range.start, m.end - range.start);
                                (line[..a].to_owned(), line[a..b].to_owned(), line[b..].to_owned())
                            }
                            _ => (line.to_owned(), String::new(), String::new()),
                        }
                    })
                    .collect();
                let last = lines.len() - 1;
                let mut caret = editing.then(|| ui::caret(lh).relative().child(self.input_anchor(cx)));
                div()
                    .flex()
                    .flex_col()
                    .font_family(TEXT_FONT)
                    .text_size(fs)
                    .line_height(lh)
                    .text_color(gpui::black())
                    .whitespace_nowrap()
                    .children(lines.into_iter().enumerate().map(|(i, (before, composing, after))| {
                        div()
                            .flex()
                            .h(lh)
                            .when(placeholder, |d| d.child(div().text_color(ui::text_tertiary()).child("Type here")))
                            .when(!before.is_empty(), |d| d.child(before))
                            .when(!composing.is_empty(), |d| {
                                d.child(div().underline().text_decoration_color(ui::accent()).child(composing))
                            })
                            .when(!after.is_empty(), |d| d.child(after))
                            .when(i == last, |d| d.children(caret.take()))
                    }))
                    .into_any_element()
            }
            ItemKind::Image { sig } => img(sig.path.clone())
                .w(px(item.w * z))
                .h(px(item.h * z))
                .object_fit(ObjectFit::Fill)
                .into_any_element(),
            // Semi-transparent while selected, so you can see what you're covering.
            ItemKind::Redact { fill } => div()
                .w(px(item.w * z))
                .h(px(item.h * z))
                .bg(redact_color(*fill))
                // A white box on a white page would be invisible; outline it here
                // (only on screen, not in the saved file).
                .when(*fill == RedactFill::White, |d| d.border_1().border_dashed().border_color(rgba(0x0000004d)))
                .when(selected, |d| d.opacity(0.6))
                .into_any_element(),
            ItemKind::Check => {
                let s = item.w * z;
                div()
                    .size(px(s))
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                if let Some(path) = fields::mark_path(&forms::Symbol::Check.mark(), bounds.origin, s) {
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
        let redact_fill = item.redaction();
        let multi_page = self.doc.as_ref().is_some_and(|d| d.page_count() > 1);
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
            .when_some(redact_fill, |h, current| {
                let swatch = |fill: RedactFill, name: &'static str, label: &'static str, cx: &mut Context<Self>| {
                    let active = current == fill;
                    div()
                        .id((name, id as usize))
                        .size(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .cursor(CursorStyle::PointingHand)
                        .hover(|s| s.bg(rgba(0xffffff1f)))
                        .child(
                            div()
                                .size(px(14.))
                                .rounded(px(3.))
                                .bg(redact_color(fill))
                                .border_1()
                                .border_color(rgba(0xffffff8c))
                                .when(active, |d| d.border_2().border_color(ui::accent())),
                        )
                        .tooltip(ui::tooltip(label, None))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_redact_fill(fill, cx)))
                };
                h.child(swatch(RedactFill::Black, "redact-black", "Black", cx))
                    .child(swatch(RedactFill::White, "redact-white", "White", cx))
                    .child(div().w(px(1.)).h(px(16.)).mx_0p5().bg(rgba(0xffffff26)))
            })
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
                                    .child(if cfg!(target_os = "macos") {
                                        "Drop a file anywhere in this window, or choose one from your Mac."
                                    } else {
                                        "Drop a file anywhere in this window, or choose one from your computer."
                                    }),
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

        let shown = self.shown_recents();
        let searching = !self.recents_query.trim().is_empty();
        let cards: Vec<_> = shown
            .iter()
            .map(|entry| {
                let open_id = entry.id.clone();
                let remove_id = entry.id.clone();
                let output = entry.output.clone();
                let original_missing = !self.file_exists(&entry.original);
                let output_exists = self.file_exists(&entry.output);
                let fields = entry.items.len();

                let thumb = match &entry.thumbnail {
                    Some(path) if entry.thumb_height > 0 && self.file_exists(path) => {
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
                                            ui::floating_icon_button(
                                                SharedString::from(format!("reveal-{}", entry.id)),
                                                "reveal",
                                                ui::text_primary(),
                                            )
                                            .size(px(28.))
                                                .tooltip(ui::tooltip("Show saved PDF in Finder", None))
                                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                                .on_click(move |_, _, cx| {
                                                    cx.stop_propagation();
                                                    cx.reveal_path(&output);
                                                }),
                                        )
                                    })
                                    .child(
                                        ui::floating_icon_button(
                                            SharedString::from(format!("forget-{}", entry.id)),
                                            "close",
                                            ui::danger(),
                                        )
                                        .size(px(28.))
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
            // Clicking anywhere else leaves the search box (keeping the search).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    if this.recents_search_focused {
                        this.recents_search_focused = false;
                        cx.notify();
                    }
                }),
            )
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
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .gap_2()
                                    .child(self.render_recents_search(cx))
                                    .child(
                                        ui::icon_button("recents-clear", "trash")
                                            .size(px(32.))
                                            .tooltip(ui::tooltip("Clear Recents…", None))
                                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.clear_recents(window, cx))),
                                    ),
                            ),
                    )
                    .when(searching && cards.is_empty(), |d| {
                        d.child(
                            div()
                                .py_10()
                                .text_center()
                                .text_sm()
                                .text_color(ui::text_secondary())
                                .child(format!("No recent documents match “{}”", self.recents_query.trim())),
                        )
                    })
                    .child(div().flex().flex_wrap().gap_5().when(!searching, |d| d.child(open_tile)).children(cards)),
            )
            .into_any_element()
    }

    /// The search box above the Recents gallery.
    fn render_recents_search(&self, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.searching_recents();
        let query = self.recents_query.clone();
        let caret = focused.then(|| ui::caret(px(15.)).relative().flex_none().child(self.input_anchor(cx)));
        ui::text_field(if focused { ui::accent() } else { ui::border() })
            .id("recents-search")
            .gap_1p5()
            .w(px(260.))
            .h(px(32.))
            .when(focused, |d| d.shadow(ui::ring()))
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.start_recents_search(window, cx);
                }),
            )
            .child(ui::icon("search", ui::text_tertiary()).size(px(15.)).flex_none())
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_sm()
                    .text_color(ui::text_primary())
                    .when(!query.is_empty(), |d| d.child(query.clone()))
                    .children(caret)
                    .when(query.is_empty(), |d| {
                        d.child(div().text_color(ui::text_tertiary()).child(SharedString::from(format!("Search · {}", ui::keys("⌘F")))))
                    }),
            )
            .when(!query.is_empty(), |d| {
                d.child(
                    ui::icon_button("recents-search-clear", "close")
                        .size(px(22.))
                        .flex_none()
                        .tooltip(ui::tooltip("Clear search", None))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.recents_query.clear();
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    fn render_thumbnail_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(doc) = &self.doc else { return div().into_any_element() };
        let current = self.current_page();
        let page_count = doc.page_count();
        let can_delete = page_count > 1;
        let drop_target = self.page_drop_target;
        // The line showing where a dragged page would land.
        let drop_line = |top: bool| {
            div()
                .absolute()
                .left(px(-8.))
                .right(px(-8.))
                .h(px(3.))
                .rounded_full()
                .bg(ui::accent())
                .when(top, |d| d.top(px(-12.)))
                .when(!top, |d| d.bottom(px(-12.)))
        };
        let drawn = self.drawn_thumbs();
        let mut thumbs = Vec::new();
        // The height of the thumbnails left out since the last one drawn.
        let mut skipped: Option<f32> = None;
        for (index, &source) in doc.order.iter().enumerate() {
            let (geom, view) = (&doc.loaded.pages[source], &doc.pages[source]);
            let (w, h) = thumb_size(geom);
            if !drawn[index] {
                let above = skipped.map_or(0., |above| above + THUMB_GAP);
                skipped = Some(above + h + THUMB_LABEL_GAP + LABEL_H);
                continue;
            }
            if let Some(height) = skipped.take() {
                thumbs.push(div().flex_none().h(px(height)).into_any_element());
            }
            let is_current = current == Some(index);
            let fields = self.items.iter().filter(|i| i.page == index).count();
            let (scroll, painted) = (self.thumbs_scroll.clone(), self.painted_thumb.clone());
            let drag = PageDrag { from: index, image: view.thumb.clone(), size: (w, h) };
            thumbs.push(
                div()
                    .id(("thumb", index))
                    .group("thumb")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .items_center()
                    .gap(px(THUMB_LABEL_GAP))
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.scroll_to_page(index, cx)))
                    .on_drag(drag, |drag: &PageDrag, _, _, cx| cx.new(|_| drag.clone()))
                    .when(index == 0, |d| {
                        let measure = move |b: Bounds<Pixels>, _: &mut Window, _: &mut App| {
                            painted.set(Some(PaintedThumb { top: b.top(), scroll: scroll.offset().y }));
                        };
                        d.child(canvas(measure, |_, _, _, _| {}).absolute().size_full())
                    })
                    .when(drop_target == Some(index), |d| d.child(drop_line(true)))
                    .when(index + 1 == page_count && drop_target == Some(page_count), |d| d.child(drop_line(false)))
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
                            })
                            // Delete, shown on hover (top-left; the field count sits top-right).
                            .when(can_delete, |d| {
                                d.child(
                                    ui::button_base(("delete-page", index))
                                        .absolute()
                                        .top(px(-8.))
                                        .left(px(-8.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(22.))
                                        .rounded_full()
                                        .bg(ui::hud_bg())
                                        .border_1()
                                        .border_color(rgba(0xffffff33))
                                        .shadow(ui::card_shadow())
                                        .cursor(CursorStyle::PointingHand)
                                        .opacity(0.)
                                        .group_hover("thumb", |s| s.opacity(1.))
                                        .hover(|s| s.bg(ui::danger()))
                                        .tooltip(ui::tooltip(format!("Delete page {}", index + 1), None))
                                        .child(ui::icon("close", gpui::white()).size(px(12.)))
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            cx.stop_propagation();
                                            this.delete_page(index, cx);
                                        })),
                                )
                            }),
                    )
                    .child(
                        div()
                            .h(px(LABEL_H))
                            .line_height(px(LABEL_H))
                            .text_xs()
                            .text_color(if is_current { ui::accent() } else { ui::text_tertiary() })
                            .when(is_current, |d| d.font_weight(FontWeight::MEDIUM))
                            .group_hover("thumb", |s| s.text_color(ui::text_primary()))
                            .child((index + 1).to_string()),
                    )
                    .into_any_element(),
            );
        }
        if let Some(height) = skipped {
            thumbs.push(div().flex_none().h(px(height)).into_any_element());
        }

        div()
            .id("thumb-rail")
            .flex()
            .flex_col()
            .flex_none()
            .items_center()
            .w(px(THUMB_RAIL_W))
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.thumbs_scroll)
            .gap(px(THUMB_GAP))
            .pt_4()
            .pb_6()
            .bg(ui::sidebar_bg())
            .border_l_1()
            .border_color(ui::border())
            .on_drag_move(cx.listener(|this, e: &gpui::DragMoveEvent<PageDrag>, _, cx| {
                let target = e.bounds.contains(&e.event.position).then(|| this.page_gap_at(e.event.position.y));
                if target != this.page_drop_target {
                    this.page_drop_target = target;
                    cx.notify();
                }
            }))
            .on_drop(cx.listener(|this, drag: &PageDrag, _, cx| {
                if let Some(gap) = this.page_drop_target.take() {
                    this.move_page(drag.from, gap, cx);
                }
                cx.notify();
            }))
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

    /// The modal password dialog for a protected PDF.
    fn render_password_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.password_prompt.as_ref()?;
        let dots: String = "•".repeat(prompt.text.chars().count());
        let name = file_name(&prompt.path);
        let had_password = self.doc.as_ref().is_some_and(|d| d.loaded.password.is_some());
        let copy_title = format!("{} for the copy of “{name}”", if had_password { "New password" } else { "Password" });
        let (title, detail, placeholder, error, action, icon) = match &prompt.purpose {
            PasswordPurpose::Open { .. } => (
                format!("“{name}” is password protected"),
                "Enter its password to open it. When you save a copy, you can keep, change or remove the password.",
                "Password",
                "That password isn't right. Try again.",
                "Open",
                "open",
            ),
            PasswordPurpose::NewPassword { first: None } => (
                copy_title,
                if had_password {
                    "The copy you save will need this password to open. The original keeps its own."
                } else {
                    "The copy you save will need this password to open. The original stays as it is."
                },
                "Password",
                "The passwords didn't match. Enter the password again.",
                "Next",
                "lock",
            ),
            PasswordPurpose::NewPassword { first: Some(_) } => (
                copy_title,
                "Type the password again to make sure.",
                "Repeat the password",
                "",
                "Save",
                "lock",
            ),
        };
        let field = ui::text_field(if prompt.wrong { ui::danger() } else { ui::accent() })
            .shadow(ui::ring())
            .when(prompt.text.is_empty(), |d| d.child(div().text_color(ui::text_tertiary()).child(placeholder)))
            .when(!prompt.text.is_empty(), |d| d.child(dots))
            .child(ui::caret(px(16.)));
        let card = div()
            .w(px(360.))
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .rounded_xl()
            .bg(ui::surface())
            .border_1()
            .border_color(ui::border())
            .shadow(ui::lifted_shadow())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(ui::icon("lock", ui::accent()).size(px(18.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(ui::text_secondary())
                    .child(detail),
            )
            .child(field)
            .when(prompt.wrong, |d| {
                d.child(div().text_xs().text_color(ui::danger()).child(error))
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        ui::secondary_button("password-cancel", "close", "Cancel")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_password(cx))),
                    )
                    .child(
                        ui::primary_button("password-open", icon, action)
                            .when(prompt.text.is_empty(), |d| d.opacity(0.5))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.submit_password(window, cx))),
                    ),
            );
        Some(
            div()
                .id("password-prompt")
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x0000004d))
                .font_family(".SystemUIFont")
                .text_color(ui::text_primary())
                .child(card)
                .into_any_element(),
        )
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
        let page_count = doc.page_count();
        let hand = if self.pan.is_some() {
            CursorStyle::ClosedHand
        } else if self.can_pan() {
            CursorStyle::OpenHand
        } else {
            CursorStyle::Arrow
        };
        let cursor = if self.tool == Tool::Select { hand } else { CursorStyle::Crosshair };
        let drawn = self.drawn_pages();
        let (_, gap) = self.layout_metrics();
        let mut page_elements = Vec::new();
        // The width and height of the pages left out since the last one drawn.
        let mut skipped: Option<(f32, Pixels)> = None;
        let space = |(width, height): (f32, Pixels)| div().flex_none().w(px(width)).h(height);
        for (index, &source) in doc.order.iter().enumerate() {
            let (geom, view) = (&doc.loaded.pages[source], &doc.pages[source]);
            if !drawn[index] {
                // It isn't anywhere on screen (see `page_at`).
                if let Some(slot) = self.page_slots.get(index) {
                    slot.set(Bounds::default());
                }
                // Each page takes its height and `gap`; the last one's gap is the list's own.
                let (width, height) = skipped.unwrap_or((0., px(-PAGE_GAP)));
                skipped = Some((width.max(geom.width * z), height + px(geom.height * z) + gap));
                continue;
            }
            page_elements.extend(skipped.take().map(space));
            // Redactions always cover the other fields (they do in the saved file, too).
            let mut on_page: Vec<&Item> = self.items.iter().filter(|i| i.page == index).collect();
            on_page.sort_by_key(|i| i.is_redaction());
            let items: Vec<_> = on_page.into_iter().map(|item| self.render_item(item, cx)).collect();
            let widgets = self.render_widgets(source, cx);
            let watermark_image = self
                .watermark
                .as_ref()
                .and_then(|_| self.watermark_preview.images.get(&page_size_key(geom)).cloned());

            let bounds = self.page_slots.get(index).cloned().unwrap_or_default();
            let (scroll, painted_scroll, painted_zoom) =
                (self.pages_scroll.clone(), self.painted_scroll.clone(), self.painted_zoom.clone());
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
                .child(
                    canvas(
                        move |b, _, _| {
                            bounds.set(b);
                            painted_scroll.set(scroll.offset());
                            painted_zoom.set(z);
                        },
                        |_, _, _, _| {},
                    )
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
                            .text_sm()
                            .text_color(ui::text_tertiary())
                            .child("Rendering…"),
                    )
                })
                // Over the page, under the fields — where the writer puts it.
                .when_some(watermark_image, |d, image| {
                    d.child(img(image).absolute().size_full().object_fit(ObjectFit::Fill))
                })
                // The document's own fields, then the items placed on top.
                .children(widgets)
                .children(items);

            page_elements.push(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(PAGE_LABEL_GAP))
                    .child(page)
                    .child(
                        div()
                            .h(px(LABEL_H))
                            .line_height(px(LABEL_H))
                            .text_xs()
                            .text_color(ui::text_tertiary())
                            .child(format!("{} of {page_count}", index + 1)),
                    ),
            );
        }
        page_elements.extend(skipped.map(space));

        let hint = match self.tool {
            Tool::Select => None,
            Tool::Text => Some(("text", "Click on the page to add a text box")),
            Tool::Date => Some(("calendar", "Click on the page to add today's date")),
            Tool::Check => Some(("check", "Click on the page to add a checkmark")),
            Tool::Redact => Some(("redact", "Drag over anything to cover it in black or white. It's removed from the file when you save")),
        };

        // ⌘/Ctrl + wheel zooms around the pointer. Listening in the capture phase
        // lets us take the event before the page list would scroll with it.
        let editor = cx.entity().downgrade();
        let wheel_zoom = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let editor = editor.clone();
                window.on_mouse_event(move |e: &ScrollWheelEvent, phase, window, cx| {
                    let zoom_modifier = e.modifiers.platform || e.modifiers.control;
                    if phase != DispatchPhase::Capture || !zoom_modifier || !bounds.contains(&e.position) {
                        return;
                    }
                    cx.stop_propagation();
                    let dy = e.delta.pixel_delta(px(20.)).y / px(1.);
                    let factor = (dy * 0.005).clamp(-0.5, 0.5).exp();
                    let _ = editor.update(cx, |this, cx| this.zoom_by(factor, Some(e.position), window, cx));
                });
            },
        )
        .absolute()
        .size_full();

        div()
            .relative()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(wheel_zoom)
            .child(
                div()
                    .id("pages")
                    .size_full()
                    // A row, so the page column can grow wider than the viewport
                    // (and scroll horizontally) when zoomed in. `items_start` stops the
                    // column being stretched to the viewport height, which would hide
                    // its overflow from the scroll container.
                    .flex()
                    .items_start()
                    .overflow_scroll()
                    .cursor(hand)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::start_pan))
                    .track_scroll(&self.pages_scroll)
                    // Keeps the thumbnail rail's "current page" highlight in sync.
                    .on_scroll_wheel(cx.listener(|_, _: &ScrollWheelEvent, _, cx| cx.notify()))
                    .bg(ui::canvas_bg())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(PAGE_GAP))
                            .flex_none()
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
        self.sync_watermark_preview(window, cx);
        let page_count = self.doc.as_ref().map_or(0, |d| d.page_count());
        self.page_slots.resize_with(page_count, Default::default);
        let field_count = self.items.len();
        let client_decorated = matches!(window.window_decorations(), gpui::Decorations::Client { .. });

        div()
            .key_context("Editor")
            .relative()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::chrome_bg())
            .text_color(ui::text_primary())
            .font_family(".SystemUIFont")
            .on_action(cx.listener(|this, _: &Open, window, cx| this.request_open(None, window, cx)))
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &SaveWithPassword, window, cx| this.save_with_new_password(window, cx)))
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
            .on_action(cx.listener(|this, _: &About, window, cx| this.show_about(window, cx)))
            .on_action(cx.listener(|this, _: &CheckForUpdates, _, cx| this.check_for_updates(cx)))
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
            .child(self.render_titlebar(client_decorated, window.is_maximized(), cx))
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
                    .child(div().flex_1().min_w_0().truncate().child(self.status.clone()))
                    .when_some(self.render_update_notice(cx), |d, notice| d.child(div().flex_none().mx_3().child(notice)))
                    .when(self.doc.is_none(), |d| {
                        d.child(
                            div()
                                .id("about")
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap_1()
                                .cursor(CursorStyle::PointingHand)
                                .text_color(ui::text_tertiary())
                                .hover(|s| s.text_color(ui::text_primary()))
                                .child(ui::icon("info", ui::text_tertiary()).size(px(12.)))
                                .child(format!("PDF Signer {}", crate::about::VERSION))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.show_about(window, cx))),
                        )
                    })
                    .when(self.doc.is_some(), |d| {
                        d.child(
                            div()
                                .flex_none()
                                .child(format!("{field_count} field{}", if field_count == 1 { "" } else { "s" })),
                        )
                    }),
            )
            .when(client_decorated && !window.is_maximized(), |d| d.children(resize_handles()))
            .when_some(self.render_password_prompt(cx), |d, prompt| d.child(prompt))
    }
}

/// Invisible strips along the window's edges and corners that resize it, for
/// windows without a system frame (client-side decorations on Linux).
fn resize_handles() -> Vec<AnyElement> {
    use gpui::ResizeEdge as E;
    const EDGE: f32 = 5.;
    const CORNER: f32 = 12.;
    [
        (E::Top, CursorStyle::ResizeUpDown),
        (E::Bottom, CursorStyle::ResizeUpDown),
        (E::Left, CursorStyle::ResizeLeftRight),
        (E::Right, CursorStyle::ResizeLeftRight),
        (E::TopLeft, CursorStyle::ResizeUpLeftDownRight),
        (E::BottomRight, CursorStyle::ResizeUpLeftDownRight),
        (E::TopRight, CursorStyle::ResizeUpRightDownLeft),
        (E::BottomLeft, CursorStyle::ResizeUpRightDownLeft),
    ]
    .into_iter()
    .map(|(edge, cursor)| {
        let d = div().absolute().cursor(cursor);
        let d = match edge {
            E::Top => d.top_0().left(px(CORNER)).right(px(CORNER)).h(px(EDGE)),
            E::Bottom => d.bottom_0().left(px(CORNER)).right(px(CORNER)).h(px(EDGE)),
            E::Left => d.left_0().top(px(CORNER)).bottom(px(CORNER)).w(px(EDGE)),
            E::Right => d.right_0().top(px(CORNER)).bottom(px(CORNER)).w(px(EDGE)),
            E::TopLeft => d.top_0().left_0().size(px(CORNER)),
            E::TopRight => d.top_0().right_0().size(px(CORNER)),
            E::BottomLeft => d.bottom_0().left_0().size(px(CORNER)),
            E::BottomRight => d.bottom_0().right_0().size(px(CORNER)),
        };
        d.id(SharedString::from(format!("resize-{edge:?}")))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(edge);
            })
            .into_any_element()
    })
    .collect()
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
/// Render pages (index and scale) of a document in the background, one after
/// the other and sharing what they have in common (see `pdf::render_pages`).
/// They arrive as they finish, for as long as they're being received and the
/// task is kept.
fn render_in_background(
    pdf: Arc<hayro::hayro_syntax::Pdf>,
    pages: Vec<(usize, f32)>,
    executor: &gpui::BackgroundExecutor,
) -> (mpsc::UnboundedReceiver<(usize, Option<pdf::RenderedPage>)>, Task<()>) {
    let (done, rendered) = mpsc::unbounded();
    let rendering = executor
        .spawn(async move { pdf::render_pages(&pdf, pages, |index, page| done.unbounded_send((index, page)).is_ok()) });
    (rendered, rendering)
}

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
        KeyBinding::new("secondary-o", Open, None),
        KeyBinding::new("secondary-s", Save, None),
        KeyBinding::new("secondary-alt-s", SaveWithPassword, None),
        KeyBinding::new("secondary-z", Undo, None),
        KeyBinding::new("secondary-shift-z", Redo, None),
        KeyBinding::new("secondary-c", Copy, None),
        KeyBinding::new("secondary-x", Cut, None),
        KeyBinding::new("secondary-v", Paste, None),
        KeyBinding::new("secondary-d", Duplicate, None),
        KeyBinding::new("secondary-1", ZoomFitWidth, None),
        KeyBinding::new("secondary-2", ZoomFitPage, None),
        KeyBinding::new("secondary-=", ZoomIn, None),
        KeyBinding::new("secondary-+", ZoomIn, None),
        KeyBinding::new("secondary--", ZoomOut, None),
        KeyBinding::new("secondary-0", ZoomReset, None),
        KeyBinding::new("secondary-shift-s", AddSignature, None),
        KeyBinding::new("secondary-w", CloseDocument, None),
        KeyBinding::new("secondary-q", Quit, None),
    ]);
}

pub fn open_editor(cx: &mut App, initial: Option<PathBuf>) -> anyhow::Result<gpui::WindowHandle<Editor>> {
    let bounds = Bounds::centered(None, size(px(1240.), px(900.)), cx);
    let window = cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("PDF Signer".into()),
                // On macOS the toolbar doubles as the title bar, around the traffic lights.
                appears_transparent: cfg!(target_os = "macos"),
                traffic_light_position: cfg!(target_os = "macos").then(|| point(px(18.), px(19.))),
            }),
            // Ask for the system's window frame. Some Linux compositors (GNOME) only
            // do client-side decorations; then the app draws the controls itself.
            window_decorations: Some(gpui::WindowDecorations::Server),
            app_id: Some(crate::about::APP_ID.into()),
            window_min_size: Some(size(px(760.), px(520.))),
            ..Default::default()
        },
        |window, cx| {
            cx.new(|cx| {
                let mut e = Editor::new(window, cx);
                if let Some(path) = initial {
                    e.open_path(path, window, cx);
                }
                e
            })
        },
    )?;
    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext};

    /// Where this run's tests keep their files.
    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pdf_signer_ui_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn setup(cx: &mut TestAppContext) -> (Entity<Editor>, &mut VisualTestContext) {
        // A file per test: tests run in parallel, and rewriting a shared file
        // while another test reads it makes that test's open fail.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = test_dir().join(format!("sample-{n}.pdf"));
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
        editor.read_with(cx, |e, _| e.slot_bounds(page).expect("page was painted").origin)
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
                ItemKind::Text { text, font_size } => {
                    assert_eq!(text, "Hi !");
                    // Its box is as wide as the text will be in the saved file.
                    assert!((item.w - pdf::measure_text(text, *font_size)).abs() < 0.01, "{}", item.w);
                }
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
        cx.simulate_keystrokes("secondary-z");
        editor.read_with(cx, |e, _| assert!((e.items[0].x - x).abs() < 0.01));
    }

    #[gpui::test]
    fn typing_through_an_input_method(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        editor.update(cx, |e, _| e.tool = Tool::Text);
        cx.simulate_click(origin + point(px(100.), px(150.)), Modifiers::none());
        cx.simulate_input("Ωμέγα");
        cx.simulate_keystrokes("space");
        // A Japanese input method composes the reading, then commits the kanji.
        editor.update_in(cx, |e, window, cx| {
            e.replace_and_mark_text_in_range(None, "に", None, window, cx);
            e.replace_and_mark_text_in_range(None, "にほん", None, window, cx);
            assert_eq!(e.marked_text_range(window, cx), Some(6..9));
            e.replace_text_in_range(None, "日本", window, cx);
            assert_eq!(e.marked_text_range(window, cx), None);
        });
        cx.simulate_keystrokes("backspace");
        editor.read_with(cx, |e, _| match &e.items[0].kind {
            ItemKind::Text { text, .. } => assert_eq!(text, "Ωμέγα 日"),
            _ => panic!("expected text"),
        });
    }

    #[gpui::test]
    fn filling_in_the_documents_own_form(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let dir = test_dir();
        let path = dir.join("form.pdf");
        std::fs::write(&path, crate::pdf::tests::sample_form()).unwrap();
        editor.update_in(cx, |e, window, cx| e.open_path(path, window, cx));
        cx.run_until_parked();
        editor.update_in(cx, |e, window, cx| e.set_zoom(1.0, window, cx));
        cx.run_until_parked();
        let origin = page_origin(&editor, cx, 0);
        let index = |editor: &Entity<Editor>, cx: &mut VisualTestContext, name: &str| {
            editor.read_with(cx, |e, _| e.form().iter().position(|f| f.name == name).unwrap())
        };
        let value = |editor: &Entity<Editor>, cx: &mut VisualTestContext, name: &str| {
            let i = index(editor, cx, name);
            editor.read_with(cx, |e, _| e.fields[i].clone())
        };
        let text = |s: &str| FieldValue::Text(s.into());
        editor.read_with(cx, |e, _| assert!(e.status.contains("7 form fields")));

        // Click into the name field and type; Tab goes on to the notes.
        cx.simulate_click(origin + point(px(150.), px(182.)), Modifiers::none());
        cx.simulate_input("Jax");
        cx.simulate_keystrokes("backspace");
        // Pasting goes into the field as well (not onto the page, as a copied item would).
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("ne".into()));
        editor.update(cx, |e, cx| e.paste(cx));
        editor.read_with(cx, |e, _| assert!(e.items.is_empty()));
        cx.simulate_keystrokes("tab");
        cx.simulate_input("line one");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("two");
        cx.simulate_keystrokes("escape");
        assert_eq!(value(&editor, cx, "name"), text("Jane"));
        assert_eq!(value(&editor, cx, "notes"), text("line one\ntwo"));

        // Tick the box, pick the other radio button, choose from the combo box.
        cx.simulate_click(origin + point(px(107.), px(384.)), Modifiers::none());
        cx.simulate_click(origin + point(px(146.), px(426.)), Modifiers::none());
        assert_eq!(value(&editor, cx, "agree"), FieldValue::On(Some("Yes".into())));
        assert_eq!(value(&editor, cx, "size"), FieldValue::On(Some("L".into())));
        // A selected radio button that can't be turned off stays on.
        cx.simulate_click(origin + point(px(146.), px(426.)), Modifiers::none());
        assert_eq!(value(&editor, cx, "size"), FieldValue::On(Some("L".into())));
        cx.simulate_click(origin + point(px(150.), px(483.)), Modifiers::none());
        let country = index(&editor, cx, "country");
        editor.update(cx, |e, cx| {
            assert_eq!(e.choice_open, Some(country), "the list of options opens");
            e.choose_option(country, "US".into(), cx);
        });
        assert_eq!(value(&editor, cx, "country"), text("US"));

        // Undo takes back one change at a time; typing counts as one.
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(value(&editor, cx, "country"), text("FR"));
        cx.simulate_keystrokes("secondary-z secondary-z");
        assert_eq!(value(&editor, cx, "agree"), FieldValue::On(None));
        assert_eq!(value(&editor, cx, "notes"), text("line one\ntwo"));
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(value(&editor, cx, "notes"), text(""));
        assert_eq!(value(&editor, cx, "name"), text("Jane"));
        cx.simulate_keystrokes("secondary-shift-z");

        // A field's text is laid out once for a value, and again when that changes.
        let name = index(&editor, cx, "name");
        editor.read_with(cx, |e, _| {
            let field = &e.form()[name];
            let layout = |value: &str| e.field_layout(name, field, 0, &field.widgets[0], &text(value));
            let jane = layout("Jane");
            assert_eq!(jane.runs[0].text, "Jane");
            assert!(Rc::ptr_eq(&jane, &layout("Jane")));
            assert_eq!(layout("Jo").runs[0].text, "Jo");
        });

        // Saving puts the values into the form. The file is parsed once (it was, to
        // read its form), and each save starts from that as it was.
        let (loaded, values) = editor.read_with(cx, |e, _| (e.doc.as_ref().unwrap().loaded.clone(), e.fields.clone()));
        let form = loaded.form.clone();
        let edits = crate::pdf::Edits { order: &[0, 1], form: &form, values: &values, ..Default::default() };
        let out = loaded.save(&edits).unwrap().bytes;
        assert_eq!(loaded.save(&edits).unwrap().bytes, out);
        assert_eq!(crate::pdf::save(&loaded.bytes, &loaded.pages, &edits).unwrap().bytes, out);
        let saved = crate::pdf::load(out).unwrap();
        let saved: Vec<_> = saved.form.iter().map(|f| (f.name.as_str(), f.value.clone())).collect();
        assert_eq!(saved[0], ("name", text("Jane")));
        assert_eq!(saved[1], ("notes", text("line one\ntwo")));

        // Reopening from Recents brings the values back, by field name; ones
        // that no longer fit the field are skipped.
        let original = editor.read_with(cx, |e, _| e.doc.as_ref().unwrap().path.clone());
        let entry = RecentEntry {
            id: "form-entry".into(),
            original: original.clone(),
            output: original.with_file_name("form-out.pdf"),
            page_count: 2,
            fields: vec![
                ("name".into(), text("Restored")),
                ("size".into(), FieldValue::On(Some("L".into()))),
                ("agree".into(), FieldValue::On(Some("Bogus".into()))),
                ("gone".into(), text("x")),
            ],
            ..RecentEntry::default()
        };
        editor.update_in(cx, |e, window, cx| {
            e.close_document(window, cx);
            e.open_path(original.clone(), window, cx);
        });
        cx.run_until_parked();
        editor.update(cx, |e, cx| {
            assert_eq!(e.doc.as_ref().map(|d| &d.path), Some(&original));
            e.restore_recent(&entry, cx);
            assert!(!e.dirty);
        });
        assert_eq!(value(&editor, cx, "name"), text("Restored"));
        assert_eq!(value(&editor, cx, "size"), FieldValue::On(Some("L".into())));
        assert_eq!(value(&editor, cx, "agree"), FieldValue::On(None));
        editor.read_with(cx, |e, _| assert!(e.status.contains("2 editable fields"), "{}", e.status));
    }

    #[gpui::test]
    fn recents_can_be_searched_and_cleared(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let entry = |name: &str| RecentEntry {
            id: name.into(),
            original: PathBuf::from(format!("/docs/{name}")),
            output: PathBuf::from(format!("/docs/out/{name}")),
            page_count: 1,
            ..RecentEntry::default()
        };
        editor.update_in(cx, |e, window, cx| {
            e.close_document(window, cx);
            e.recents = ["Lease 2024.pdf", "tax-return.pdf", "lease-renewal.pdf"].map(entry).to_vec();
        });
        cx.run_until_parked();
        let shown = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| e.shown_recents().iter().map(|e| e.id.clone()).collect::<Vec<_>>())
        };

        // Typing anywhere in the gallery searches it; every word has to match.
        cx.simulate_keystrokes("l e a s e");
        editor.read_with(cx, |e, _| assert!(e.searching_recents()));
        assert_eq!(shown(&editor, cx), ["Lease 2024.pdf", "lease-renewal.pdf"]);
        cx.simulate_input(" ren");
        assert_eq!(shown(&editor, cx), ["lease-renewal.pdf"]);
        cx.simulate_keystrokes("backspace backspace backspace backspace");
        assert_eq!(shown(&editor, cx).len(), 2);
        cx.simulate_input("zzz");
        assert!(shown(&editor, cx).is_empty());
        // Escape clears the search, then leaves the box.
        cx.simulate_keystrokes("escape");
        assert_eq!(shown(&editor, cx).len(), 3);
        cx.simulate_keystrokes("escape");
        editor.read_with(cx, |e, _| assert!(!e.searching_recents()));
        cx.simulate_keystrokes("secondary-f");
        editor.read_with(cx, |e, _| assert!(e.searching_recents()));

        // Removing one, then clearing the rest (after asking).
        editor.update(cx, |e, cx| e.remove_recent("tax-return.pdf", cx));
        assert_eq!(shown(&editor, cx).len(), 2);
        editor.update_in(cx, |e, window, cx| e.clear_recents(window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(shown(&editor, cx).len(), 2);
        editor.update_in(cx, |e, window, cx| e.clear_recents(window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Clear Recents");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert!(e.recents.is_empty()));
        assert!(crate::recents::load().is_empty());
    }

    #[gpui::test]
    fn protected_pdfs_ask_for_their_password(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let dir = test_dir();
        let path = dir.join("protected.pdf");
        std::fs::write(&path, crate::pdf::tests::encrypted(&crate::pdf::tests::sample_pdf(), "sesame", "owner")).unwrap();

        editor.update_in(cx, |e, window, cx| e.request_open(Some(path.clone()), window, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert!(e.password_prompt.is_some(), "asks for the password");
            assert_ne!(e.doc.as_ref().unwrap().path, path, "the open document stays open meanwhile");
        });

        cx.simulate_input("nope");
        cx.simulate_keystrokes("enter");
        editor.read_with(cx, |e, _| {
            let prompt = e.password_prompt.as_ref().expect("still asking");
            assert!(prompt.wrong);
            assert!(prompt.text.is_empty());
        });

        cx.simulate_input("sesame");
        cx.simulate_keystrokes("enter");
        editor.read_with(cx, |e, _| {
            assert!(e.password_prompt.is_none());
            let doc = e.doc.as_ref().unwrap();
            assert_eq!(doc.path, path);
            assert_eq!(doc.loaded.password.as_deref(), Some("sesame"));
        });
    }

    #[gpui::test]
    fn saving_a_protected_pdf_asks_how_to_protect_the_copy(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let dir = test_dir();
        let path = dir.join("protected-save.pdf");
        std::fs::write(&path, crate::pdf::tests::encrypted(&crate::pdf::tests::sample_pdf(), "sesame", "owner")).unwrap();
        editor.update_in(cx, |e, window, cx| e.open_path(path, window, cx));
        cx.run_until_parked();
        cx.simulate_input("sesame");
        cx.simulate_keystrokes("enter");
        let opens = |out: &Path, password: Option<&str>| {
            crate::pdf::load_with_password(std::fs::read(out).unwrap(), password).map(|_| ()).map_err(|e| e.to_string())
        };
        let save_as = |cx: &mut VisualTestContext, out: PathBuf| {
            cx.run_until_parked();
            assert!(cx.did_prompt_for_new_path());
            cx.simulate_new_path_selection(move |_| Some(out));
            cx.run_until_parked();
        };

        // A new password, typed twice. A repeat that doesn't match starts over.
        editor.update_in(cx, |e, window, cx| e.save(window, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Change Password…");
        cx.run_until_parked();
        cx.simulate_input("new-1");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("typo");
        cx.simulate_keystrokes("enter");
        editor.read_with(cx, |e, _| {
            let prompt = e.password_prompt.as_ref().expect("still asking");
            assert!(prompt.wrong);
            assert!(prompt.purpose == PasswordPurpose::NewPassword { first: None });
        });
        cx.simulate_input("new-1");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("new-1");
        cx.simulate_keystrokes("enter");
        let changed = dir.join("protected-new-password.pdf");
        save_as(cx, changed.clone());
        assert_eq!(opens(&changed, None).unwrap_err(), "this PDF is password protected");
        assert!(opens(&changed, Some("sesame")).is_err());
        assert_eq!(opens(&changed, Some("new-1")), Ok(()));
        let thumbnail = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| e.recents[0].thumbnail.as_ref().is_some_and(|t| t.exists()))
        };
        editor.read_with(cx, |e, _| assert!(e.status.contains("with the new password"), "{}", e.status));
        assert!(!thumbnail(&editor, cx), "no picture of a protected document");

        // No password at all.
        editor.update_in(cx, |e, window, cx| e.save(window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Remove Password");
        let open = dir.join("protected-removed.pdf");
        save_as(cx, open.clone());
        assert_eq!(opens(&open, None), Ok(()));
        editor.read_with(cx, |e, _| assert!(e.status.contains("without a password"), "{}", e.status));
        assert!(thumbnail(&editor, cx), "a copy without a password gets its thumbnail");

        // The same password as the original.
        editor.update_in(cx, |e, window, cx| e.save(window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Keep Password");
        let kept = dir.join("protected-kept.pdf");
        save_as(cx, kept.clone());
        assert_eq!(opens(&kept, Some("sesame")), Ok(()));
        assert!(!thumbnail(&editor, cx));
    }

    #[gpui::test]
    fn a_password_can_be_added_when_saving(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let dir = test_dir();
        let out = dir.join("now-protected.pdf");
        cx.simulate_keystrokes("secondary-alt-s");
        cx.simulate_input("p4ss");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("p4ss");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(cx.did_prompt_for_new_path());
        cx.simulate_new_path_selection({
            let out = out.clone();
            move |_| Some(out)
        });
        cx.run_until_parked();
        let bytes = std::fs::read(&out).unwrap();
        assert!(crate::pdf::load(bytes.clone()).is_err());
        assert!(crate::pdf::load_with_password(bytes, Some("p4ss")).is_ok());
        editor.read_with(cx, |e, _| {
            assert!(e.status.contains("with a password"), "{}", e.status);
            assert!(e.recents[0].thumbnail.is_none());
            assert!(e.doc.as_ref().unwrap().loaded.password.is_none(), "the open document itself is unchanged");
        });
    }

    #[gpui::test]
    fn check_and_signature_placement_then_save(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 1);

        editor.update(cx, |e, _| e.tool = Tool::Check);
        cx.simulate_click(origin + point(px(60.), px(60.)), Modifiers::none());

        let sig_path = test_dir().join("sig.png");
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
        cx.simulate_keystrokes("secondary-z");

        let (bytes, pages, items, metrics) = editor.read_with(cx, |e, _| {
            let d = e.doc.as_ref().unwrap();
            (d.loaded.bytes.clone(), d.loaded.pages.clone(), e.items.clone(), e.metrics)
        });
        assert_eq!(items.len(), 2);
        let out = crate::pdf::write(&bytes, &pages, &[0, 1], &items, None, metrics).unwrap();
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
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(count(&editor, cx), 0);
        cx.simulate_keystrokes("secondary-shift-z");
        assert_eq!(count(&editor, cx), 1);
        cx.simulate_keystrokes("secondary-shift-z");
        assert_eq!(count(&editor, cx), 1, "nothing left to redo");

        // Copy + paste: a copy offset from the original on the visible page.
        let original = editor.update(cx, |e, _| {
            e.selected = Some(e.items[0].id);
            e.items[0].clone()
        });
        cx.simulate_keystrokes("secondary-c secondary-v");
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 2);
            let pasted = &e.items[1];
            assert_ne!(pasted.id, original.id);
            assert_eq!((pasted.page, pasted.x, pasted.y), (0, original.x + 12., original.y + 12.));
            assert_eq!(e.selected, Some(pasted.id));
        });
        // Pasting again cascades instead of stacking on the previous copy.
        cx.simulate_keystrokes("secondary-v");
        editor.read_with(cx, |e, _| assert_eq!(e.items[2].x, original.x + 24.));

        // Duplicate, then undo it; a new edit clears the redo stack.
        cx.simulate_keystrokes("secondary-d");
        assert_eq!(count(&editor, cx), 4);
        cx.simulate_keystrokes("secondary-z");
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
        cx.simulate_keystrokes("secondary-z");
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

    /// A PDF of `count` blank pages, some upright and some on their side.
    fn long_pdf(count: usize) -> Vec<u8> {
        use lopdf::{Document, Object, dictionary};
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let kids: Vec<Object> = (0..count)
            .map(|i| {
                let (w, h) = if i % 3 == 0 { (792, 612) } else { (612, 792) };
                let page = dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
                };
                doc.add_object(page).into()
            })
            .collect();
        doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count as i64 }));
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[gpui::test]
    fn only_pages_near_the_viewport_are_drawn(cx: &mut TestAppContext) {
        const PAGES: usize = 40;
        let (editor, cx) = setup(cx);
        let path = test_dir().join("long.pdf");
        std::fs::write(&path, long_pdf(PAGES)).unwrap();
        editor.update_in(cx, |e, window, cx| e.open_path(path, window, cx));
        cx.run_until_parked();
        editor.update_in(cx, |e, window, cx| e.set_zoom(1.0, window, cx));
        cx.run_until_parked();
        let drawn = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| (0..PAGES).filter(|&i| e.slot_bounds(i).is_some()).collect::<Vec<_>>())
        };

        // At the top, only the first few pages are there.
        let at_top = drawn(&editor, cx);
        assert!(at_top.contains(&0) && at_top.len() < 10 && at_top.iter().all(|&i| i < 10), "{at_top:?}");

        // The rest still take up their space: a page far down is where it would be
        // with every page drawn, and is drawn once it's scrolled to.
        editor.update(cx, |e, cx| e.scroll_to_page(30, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            let (painted, expected) = (e.slot_bounds(30).expect("drawn when scrolled to"), e.live_page_bounds(30).unwrap());
            assert!((painted.top() - expected.top()).abs() < px(1.), "{painted:?} vs {expected:?}");
            assert!((painted.left() - expected.left()).abs() < px(1.), "{painted:?} vs {expected:?}");
            assert_eq!(e.current_page(), Some(30));
        });
        let in_the_middle = drawn(&editor, cx);
        assert!(in_the_middle.starts_with(&[0]) && !in_the_middle.contains(&10), "{in_the_middle:?}");

        // The same at a zoom that gives pages fractional sizes.
        editor.update_in(cx, |e, window, cx| e.set_zoom(0.73, window, cx));
        cx.run_until_parked();
        editor.update(cx, |e, cx| e.scroll_to_page(37, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            let (painted, expected) = (e.slot_bounds(37).expect("drawn when scrolled to"), e.live_page_bounds(37).unwrap());
            assert!((painted.top() - expected.top()).abs() < px(1.), "{painted:?} vs {expected:?}");
        });
        editor.update_in(cx, |e, window, cx| e.set_zoom(1.0, window, cx));
        editor.update(cx, |e, cx| e.scroll_to_page(30, cx));
        cx.run_until_parked();
        let origin = editor.read_with(cx, |e, _| e.slot_bounds(30).unwrap().origin);

        // It takes clicks and typing like any other page, and stays while it's typed into.
        editor.update(cx, |e, _| e.tool = Tool::Text);
        cx.simulate_click(origin + point(px(100.), px(150.)), Modifiers::none());
        cx.simulate_input("a");
        editor.update(cx, |e, cx| e.scroll_to_page(5, cx));
        cx.run_until_parked();
        assert!(drawn(&editor, cx).contains(&30));
        cx.simulate_input("b");
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 1);
            assert_eq!(e.items[0].page, 30);
            assert!(matches!(&e.items[0].kind, ItemKind::Text { text, .. } if text == "ab"));
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(!drawn(&editor, cx).contains(&30));

        // The thumbnail rail does the same, and is as long as with every thumbnail drawn.
        let rail = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            editor.read_with(cx, |e, _| (e.drawn_thumbs().iter().filter(|&&d| d).count(), e.thumbs_scroll.max_offset().height))
        };
        let (some, length) = rail(&editor, cx);
        assert!(some < PAGES && length > px(0.), "{some} thumbnails, {length:?}");
        editor.read_with(cx, |e, _| {
            e.painted_thumb.set(None);
            assert!(e.drawn_thumbs().iter().all(|&d| d));
        });
        editor.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        let full_length = editor.read_with(cx, |e, _| {
            assert!(e.painted_thumb.get().is_some(), "the rail was drawn again");
            e.thumbs_scroll.max_offset().height
        });
        assert!((length - full_length).abs() < px(1.), "{length:?} vs {full_length:?}");
        // Thumbnails are dropped between the ones they're over.
        editor.read_with(cx, |e, _| {
            let (top, height) = e.thumb_tops().unwrap().nth(2).unwrap();
            assert_eq!(e.page_gap_at(top + height / 2. + px(1.)), 3);
            assert_eq!(e.page_gap_at(top - px(1.)), 2);
        });
    }

    #[gpui::test]
    fn fit_zoom_thumbnails_and_page_navigation(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);

        // Fit width: the widest page spans the viewport (minus padding).
        cx.simulate_keystrokes("secondary-1");
        cx.run_until_parked();
        let (zoom, widest, viewport) = editor.read_with(cx, |e, _| {
            let widest = e.doc.as_ref().unwrap().loaded.pages.iter().map(|p| p.width).fold(0., f32::max);
            (e.zoom, widest, e.pages_scroll.bounds())
        });
        let expected = (viewport.size.width / px(1.) - 72.) / widest;
        assert!((zoom - expected.clamp(MIN_ZOOM, MAX_ZOOM)).abs() < 0.01, "zoom {zoom} vs {expected}");

        // Fit page: a whole page fits vertically too.
        cx.simulate_keystrokes("secondary-2");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert_eq!(e.zoom_mode, ZoomMode::FitPage);
            let page = e.slot_bounds(0).unwrap();
            assert!(page.size.height <= viewport.size.height, "page should fit: {page:?} in {viewport:?}");
        });

        // A fixed percentage leaves the fit modes.
        cx.simulate_keystrokes("secondary-0");
        editor.read_with(cx, |e, _| assert_eq!((e.zoom_mode, e.zoom), (ZoomMode::Fixed, 1.0)));

        // Every page gets a thumbnail; clicking one scrolls that page into view.
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert!(e.doc.as_ref().unwrap().pages.iter().all(|p| p.thumb.is_some()));
            assert_eq!(e.current_page(), Some(0));
        });
        // The highlight must follow straight away, before the pages are painted again
        // (it's computed in the render that the click triggers).
        editor.update(cx, |e, cx| {
            e.scroll_to_page(1, cx);
            assert_eq!(e.current_page(), Some(1));
        });
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert_eq!(e.current_page(), Some(1)));
        editor.update(cx, |e, cx| {
            e.scroll_to_page(0, cx);
            assert_eq!(e.current_page(), Some(0));
        });
    }

    #[gpui::test]
    fn zooming_keeps_the_pointed_spot_and_dragging_pans(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        let anchor = origin + point(px(200.), px(300.));
        let state = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| (e.zoom, e.pages_scroll.offset(), e.pages_scroll.max_offset(), e.page_at(anchor)))
        };

        // Zoom in around a point: the same spot of the page stays under it.
        editor.update_in(cx, |e, window, cx| e.zoom_by(3.0, Some(anchor), window, cx));
        cx.run_until_parked();
        let (zoom, start, max, hit) = state(&editor, cx);
        assert_eq!(zoom, 3.0);
        let (page, x, y) = hit.expect("anchor should still be over a page");
        assert_eq!(page, 0);
        assert!((x - 200.).abs() < 0.5 && (y - 300.).abs() < 0.5, "drifted to ({x}, {y})");
        assert!(max.width > px(0.), "zoomed-in pages should scroll horizontally");

        // Dragging empty page area pans (clamped to the scrollable range).
        let delta = point(px(-150.), px(-90.));
        cx.simulate_mouse_down(anchor, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(anchor + delta, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(anchor + delta, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let (_, panned, _, _) = state(&editor, cx);
        let expected = point(
            (start.x + delta.x).clamp(-max.width, px(0.)),
            (start.y + delta.y).clamp(-max.height, px(0.)),
        );
        assert_eq!(panned, expected);
        assert_ne!(panned, start);
        editor.read_with(cx, |e, _| assert!(e.pan.is_none() && e.items.is_empty()));

        // ⌘ + wheel zooms around the pointer instead of scrolling…
        let wheel = |modifiers| ScrollWheelEvent {
            position: anchor,
            delta: gpui::ScrollDelta::Lines(point(0., 2.)),
            modifiers,
            touch_phase: gpui::TouchPhase::Moved,
        };
        let before = editor.read_with(cx, |e, _| e.page_at(anchor)).unwrap();
        cx.simulate_event(wheel(Modifiers::command()));
        cx.run_until_parked();
        let (zoomed, _, _, hit) = state(&editor, cx);
        assert!(zoomed > 3.0, "zoom should grow, got {zoomed}");
        let hit = hit.unwrap();
        assert!((hit.1 - before.1).abs() < 0.5 && (hit.2 - before.2).abs() < 0.5);

        // …while a plain wheel still scrolls.
        let (_, offset, _, _) = state(&editor, cx);
        cx.simulate_event(wheel(Modifiers::none()));
        cx.run_until_parked();
        let (zoom_after, offset_after, _, _) = state(&editor, cx);
        assert_eq!(zoom_after, zoomed);
        assert_ne!(offset_after, offset);
    }

    #[gpui::test]
    fn redaction_boxes_are_drawn_by_dragging(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);

        // Drag from bottom-right to top-left: the box still comes out right.
        editor.update(cx, |e, _| e.tool = Tool::Redact);
        let (from, to) = (origin + point(px(360.), px(150.)), origin + point(px(60.), px(60.)));
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 1);
            let r = &e.items[0];
            assert!(r.is_redaction());
            assert_eq!((r.page, r.x, r.y, r.w, r.h), (0, 60., 60., 300., 90.));
            assert_eq!(e.tool, Tool::Select, "one box per use, like the other tools");
            assert_eq!(e.history.len(), 1, "one undo step per box");
        });

        // A plain click places a default-sized box centred on the click.
        editor.update(cx, |e, _| e.tool = Tool::Redact);
        cx.simulate_click(origin + point(px(200.), px(400.)), Modifiers::none());
        editor.read_with(cx, |e, _| {
            let r = &e.items[1];
            let (w, h) = DEFAULT_REDACTION;
            assert_eq!((r.x, r.y, r.w, r.h), (200. - w / 2., 400. - h / 2., w, h));
        });
        cx.simulate_keystrokes("secondary-z");
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 1));

        // Boxes can be white instead of black; new ones take the last colour chosen.
        let fill = |editor: &Entity<Editor>, cx: &mut VisualTestContext, i: usize| {
            editor.read_with(cx, |e, _| match e.items[i].kind {
                ItemKind::Redact { fill } => fill,
                _ => panic!("expected a redaction"),
            })
        };
        assert_eq!(fill(&editor, cx, 0), RedactFill::Black);
        let first = editor.read_with(cx, |e, _| e.items[0].id);
        editor.update(cx, |e, cx| {
            e.selected = Some(first);
            e.set_redact_fill(RedactFill::White, cx);
        });
        assert_eq!(fill(&editor, cx, 0), RedactFill::White);
        editor.update(cx, |e, _| e.tool = Tool::Redact);
        cx.simulate_click(origin + point(px(200.), px(400.)), Modifiers::none());
        assert_eq!(fill(&editor, cx, 1), RedactFill::White);
        cx.simulate_keystrokes("secondary-z secondary-z");
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 1));
        assert_eq!(fill(&editor, cx, 0), RedactFill::Black, "changing the colour is undoable");

        // Redactions resize freely (no fixed aspect ratio).
        let id = editor.read_with(cx, |e, _| e.items[0].id);
        let corner = origin + point(px(360.), px(150.));
        editor.update_in(cx, |e, window, cx| {
            let down = MouseDownEvent { button: MouseButton::Left, position: corner, modifiers: Modifiers::none(), click_count: 1, first_mouse: false };
            e.item_mouse_down(id, DragMode::Resize, &down, window, cx);
        });
        cx.simulate_mouse_move(corner + point(px(40.), px(-50.)), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(corner + point(px(40.), px(-50.)), MouseButton::Left, Modifiers::none());
        editor.read_with(cx, |e, _| assert_eq!((e.items[0].w, e.items[0].h), (340., 40.)));

        // Saving removes the covered text from the file.
        let (bytes, pages, items, metrics) = editor.read_with(cx, |e, _| {
            let d = e.doc.as_ref().unwrap();
            (d.loaded.bytes.clone(), d.loaded.pages.clone(), e.items.clone(), e.metrics)
        });
        let out = crate::pdf::write(&bytes, &pages, &[0, 1], &items, None, metrics).unwrap();
        let mut doc = lopdf::Document::load_mem(&out).unwrap();
        doc.decompress();
        let mut raw = Vec::new();
        doc.save_to(&mut raw).unwrap();
        let raw = String::from_utf8_lossy(&raw);
        assert!(!raw.contains("Page 1 - Name:") && raw.contains("Page 2 - Name:"));
    }

    #[gpui::test]
    fn watermark_typing_presets_undo_and_discard(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let watermark_text = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| e.watermark.as_ref().map(|w| w.text.clone()))
        };

        // Typing replaces the suggested text; the whole typing session is one undo step.
        editor.update_in(cx, |e, window, cx| {
            e.watermark_menu_open = true;
            e.start_watermark_typing(window, cx);
        });
        cx.simulate_keystrokes("d r a f t space shift-x backspace");
        assert_eq!(watermark_text(&editor, cx).as_deref(), Some("draft "));
        cx.simulate_keystrokes("enter");
        editor.read_with(cx, |e, _| assert!(e.dirty && !e.watermark_editing && e.history.len() == 1));

        // The preview is rendered for each page size (the sample has two).
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert_eq!(e.watermark_preview.images.len(), 2));

        // Presets and options are undoable steps too.
        editor.update(cx, |e, cx| e.update_watermark(cx, |w| w.text = "COPY".into()));
        editor.update(cx, |e, cx| e.update_watermark(cx, |w| w.color = 0xd93025));
        cx.simulate_keystrokes("secondary-z");
        editor.read_with(cx, |e, _| assert_eq!(e.watermark.as_ref().unwrap().color, Watermark::default().color));
        cx.simulate_keystrokes("secondary-z secondary-z");
        assert_eq!(watermark_text(&editor, cx), None, "undone back to no watermark");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert!(e.watermark_preview.images.is_empty(), "preview cleared"));
        cx.simulate_keystrokes("secondary-shift-z");
        assert_eq!(watermark_text(&editor, cx).as_deref(), Some("draft "));

        // Discard returns to the last save (no watermark); removing works too.
        editor.update(cx, |e, cx| e.discard_changes(cx));
        assert_eq!(watermark_text(&editor, cx), None);
        cx.simulate_keystrokes("secondary-z");
        assert!(watermark_text(&editor, cx).is_some());
        editor.update(cx, |e, cx| e.remove_watermark(cx));
        assert_eq!(watermark_text(&editor, cx), None);
    }

    #[gpui::test]
    fn pages_can_be_deleted_and_reordered(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let order = |editor: &Entity<Editor>, cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.doc.as_ref().unwrap().order.clone());
        let pages_of = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| e.items.iter().map(|i| (i.page, i.x)).collect::<Vec<_>>())
        };
        // A checkmark on each page, told apart by x.
        editor.update(cx, |e, cx| {
            e.add_item(0, 10., 10., 16., 16., ItemKind::Check, cx);
            e.add_item(1, 20., 20., 16., 16., ItemKind::Check, cx);
            e.history.clear();
        });

        // Drag page 2 above page 1: its field moves with it. The view stays at the top,
        // so the first position is still the current page — straight away, before
        // the pages are painted in their new order.
        editor.update(cx, |e, cx| {
            e.move_page(1, 0, cx);
            assert_eq!(e.current_page(), Some(0));
        });
        assert_eq!(order(&editor, cx), vec![1, 0]);
        assert_eq!(pages_of(&editor, cx), vec![(1, 10.), (0, 20.)]);
        // Dropping a page next to itself does nothing (and adds no undo step).
        editor.update(cx, |e, cx| {
            e.move_page(0, 0, cx);
            e.move_page(0, 1, cx);
            assert_eq!(e.history.len(), 1);
        });

        // The page shown first is now the rotated one, everywhere in the UI.
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            let geom = e.page_geom(0).unwrap();
            assert_eq!((geom.width, geom.height), (750., 580.));
            let first = e.slot_bounds(0).unwrap();
            assert_eq!(first.size.width / first.size.height, 750. / 580.);
        });

        // Save: the output has the new order, with each field on its page.
        let save = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            let (bytes, pages, order, items, metrics) = editor.read_with(cx, |e, _| {
                let d = e.doc.as_ref().unwrap();
                (d.loaded.bytes.clone(), d.loaded.pages.clone(), d.order.clone(), e.items.clone(), e.metrics)
            });
            crate::pdf::load(crate::pdf::write(&bytes, &pages, &order, &items, None, metrics).unwrap()).unwrap()
        };
        let out = save(&editor, cx);
        assert_eq!((out.pages[0].width, out.pages[1].width), (750., 612.));

        // Delete the (new) first page: its field goes too; the rest renumber.
        editor.update(cx, |e, cx| e.delete_page(0, cx));
        assert_eq!(order(&editor, cx), vec![0]);
        assert_eq!(pages_of(&editor, cx), vec![(0, 10.)]);
        assert_eq!(save(&editor, cx).pages.len(), 1);
        // The last page can't be deleted.
        editor.update(cx, |e, cx| e.delete_page(0, cx));
        assert_eq!(order(&editor, cx), vec![0]);

        // Undo brings the page back with its field; redo deletes it again.
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(order(&editor, cx), vec![1, 0]);
        assert_eq!(pages_of(&editor, cx), vec![(1, 10.), (0, 20.)]);
        cx.simulate_keystrokes("secondary-shift-z");
        assert_eq!(order(&editor, cx), vec![0]);

        // Discard returns to the saved (original) order.
        editor.update(cx, |e, cx| e.discard_changes(cx));
        assert_eq!(order(&editor, cx), vec![0, 1]);
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
        cx.simulate_keystrokes("secondary-z");
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
                SavedItem { page: 0, x: 20., y: 20., w: 90., h: 14., kind: SavedKind::Redact { fill: RedactFill::White } },
            ],
            watermark: Some(Watermark { text: "COPY".into(), ..Watermark::default() }),
            order: Some(vec![1, 0]),
            fields: Vec::new(),
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
            assert_eq!(e.items.len(), 3);
            assert!(e.items[2].is_redaction());
            assert_eq!(e.watermark.as_ref().map(|w| w.text.as_str()), Some("COPY"));
            assert_eq!(e.doc.as_ref().unwrap().order, vec![1, 0]);
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
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 3));
    }
}
