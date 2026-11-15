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
    AnyElement, AnyWindowHandle, App, Axis, Bounds, ClickEvent, Context, CursorStyle, Entity, ExternalPaths, FocusHandle, Focusable,
    FontWeight, Global, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    PathPromptOptions, Pixels, Point, PromptLevel, RenderImage, ScrollHandle, ScrollWheelEvent, SharedString,
    Subscription, Task, WeakEntity, Window, DispatchPhase, actions, anchored, canvas, deferred, div, font, img, point, prelude::*, px, rgb, rgba,
    size,
};

use crate::model::{self, Watermark, Item, ItemKind, LINE_HEIGHT, PageGeom, PageImport, RedactFill, Signature, TEXT_FONT, TextMetrics};

mod fields;
mod input;
use input::InputTarget;
use crate::fonts::FontBook;
use crate::forms::{self, FieldKind, FieldValue};
use crate::pdf::{self, LoadedPdf};
use crate::recents::{self, RecentEntry};
use crate::scrollbar::{self, scrollbar};
use crate::clipboard;
use crate::search;
use crate::gestures;
use crate::settings::{self, Settings};
use crate::signatures;
use crate::ui;
use crate::updater::{self, Updater};

actions!(
    patrapdf,
    [
        NewWindow,
        Open,
        Save,
        SaveWithPassword,
        CopyPdf,
        Undo,
        Redo,
        Copy,
        Cut,
        Paste,
        Duplicate,
        Find,
        FindNext,
        FindPrevious,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ZoomFitWidth,
        ZoomFitPage,
        AddSignature,
        AddPages,
        CloseDocument,
        DiscardChanges,
        MinimizeWindow,
        ZoomWindow,
        ToggleLeftSidebar,
        ToggleRightSidebar,
        KeyboardShortcuts,
        VisitWebsite,
        About,
        CheckForUpdates,
        Quit
    ]
);

const MIN_ZOOM: f32 = 0.3;
const MAX_ZOOM: f32 = 4.0;
const MAX_RENDER_SCALE: f32 = 6.0;
/// How many bytes of page bitmaps a document keeps: the pages around the one
/// in view, the rest being drawn again when they are scrolled to. A page that
/// has been on screen takes twice its bitmap, as the graphics card has a copy.
const RENDER_BUDGET: usize = 128 << 20;
/// Thumbnails are drawn for the pages in sight in the rail, and for those this
/// far either side of the page in view (which they stand in for until the
/// page itself is drawn). Past `THUMBS_KEPT` of them, the others are let go.
const THUMBS_AROUND: usize = 8;
const THUMBS_KEPT: usize = 96;

/// The pages (by position) to keep drawn at `scale`, nearest to `current`
/// first: as many as `budget` bytes hold, and the nearest three whatever they take.
fn pages_in_budget(sizes: &[(f32, f32)], current: usize, scale: f32, budget: usize) -> Vec<usize> {
    let mut positions: Vec<usize> = (0..sizes.len()).collect();
    positions.sort_by_key(|&position| position.abs_diff(current));
    let mut left = budget;
    let mut kept = 0;
    for (n, &position) in positions.iter().enumerate() {
        let (width, height) = sizes[position];
        let scale = pdf::render_scale(width, height, scale);
        let bytes = (width * scale) as usize * (height * scale) as usize * 4;
        if n >= 3 && bytes > left {
            break;
        }
        left = left.saturating_sub(bytes);
        kept = n + 1;
    }
    positions.truncate(kept);
    positions
}

/// In a document too long to keep every page drawn: what the drawn pages were
/// chosen for. They are chosen again once the view has moved `slack` pages
/// from `page`, or the pages are not in this `order` any more.
struct DrawnAround {
    page: usize,
    slack: usize,
    order: Vec<usize>,
}
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

/// Every keyboard shortcut, as the Help menu lists them (written the macOS way; see `ui::keys`).
const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    (
        "File",
        &[
            ("Open a PDF", "⌘O"),
            ("New window", "⌘N"),
            ("Save a filled copy", "⌘S"),
            ("Save with a password", "⌥⌘S"),
            ("Copy the PDF to the clipboard", "⇧⌘C"),
            ("Add signature image", "⇧⌘S"),
            ("All documents", "⌘W"),
            ("Search recent documents", "⌘F"),
        ],
    ),
    (
        "Edit",
        &[
            ("Undo / Redo", "⌘Z  ⇧⌘Z"),
            ("Cut / Copy / Paste field", "⌘X  ⌘C  ⌘V"),
            ("Duplicate field", "⌘D"),
            ("Delete field", "⌫"),
            ("Find in the document", "⌘F"),
            ("Next / Previous match", "⌘G  ⇧⌘G"),
            ("Nudge (Shift = 10pt)", "←↑→↓"),
            ("Next / Previous form field", "Tab  ⇧Tab"),
        ],
    ),
    (
        "View",
        &[
            ("Zoom in / out", "⌘+  ⌘−"),
            ("Actual size", "⌘0"),
            ("Fit width / page", "⌘1  ⌘2"),
            ("Left sidebar", "⌘B"),
            ("Right sidebar", "⌥⌘B"),
            ("Help and shortcuts", "⌘/"),
        ],
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Select,
    Text,
    Date,
    Check,
    Redact,
    Highlight,
    Pen,
    Rectangle,
    Oval,
    Line,
    Arrow,
    /// Picks out the document's own text, to copy.
    SelectText,
}

impl Tool {
    /// Whether it stays in hand after it is used on the page (until Esc), and
    /// is used by dragging: over an item as anywhere else.
    fn stays(self) -> bool {
        self.draws() || self == Tool::SelectText
    }

    /// Whether it draws an annotation where it is dragged over the page. Such
    /// a tool stays in hand afterwards, to draw the next.
    fn draws(self) -> bool {
        matches!(self, Tool::Highlight | Tool::Pen) || self.is_shape()
    }

    fn is_shape(self) -> bool {
        matches!(self, Tool::Rectangle | Tool::Oval | Tool::Line | Tool::Arrow)
    }
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
        Tool::Highlight => ("highlight", "Highlight"),
        Tool::Pen => ("pen", "Draw"),
        Tool::Rectangle => ("square", "Rectangle"),
        Tool::Oval => ("circle", "Oval"),
        Tool::Line => ("line", "Line"),
        Tool::Arrow => ("arrow", "Arrow"),
        Tool::SelectText => ("select-text", "Select text"),
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
    /// Drawing a new box (a redaction, a highlight, a shape) from the point where the drag started.
    Draw,
    /// Drawing a straight line from there.
    Line,
    /// Drawing freehand.
    Ink,
}

impl DragMode {
    /// Whether the drag draws a new item.
    fn draws(self) -> bool {
        matches!(self, DragMode::Draw | DragMode::Line | DragMode::Ink)
    }
}

struct DragState {
    id: u64,
    mode: DragMode,
    start_mouse: Point<Pixels>,
    start: (f32, f32, f32, f32),
    snapshot: Snapshot,
    moved: bool,
    /// The page points (view space) a line being drawn runs through.
    points: Vec<(f32, f32)>,
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
    /// The scale it is drawn at, or is being drawn at by the render under way.
    requested_scale: f32,
}

struct OpenDoc {
    path: PathBuf,
    /// The file at `path`, followed by the pages added to it from other PDFs
    /// (`imports`). "Original" pages and indices below are this document's.
    loaded: LoadedPdf,
    /// Where the pages after the file's own came from, in the order they were added.
    imports: Vec<PageImport>,
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
    /// The text of each page of the original file, by original index, once it
    /// has been read (which starts when it is first looked through, see `FindBar`).
    text: Vec<Option<search::PageText>>,
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

    /// What has to be added to the file again to get the pages `order`
    /// counts: the imports up to the last one with a page still in use.
    fn imports_in_use(&self) -> Vec<PageImport> {
        let own = self.loaded.pages.len() - self.imports.iter().map(|i| i.pages.len()).sum::<usize>();
        let last = self.order.iter().copied().max().unwrap_or(0);
        let mut first = own;
        let in_use = |import: &&PageImport| {
            let used = first <= last;
            first += import.pages.len();
            used
        };
        self.imports.iter().take_while(in_use).cloned().collect()
    }
}

/// Asking which pages of another PDF to add to the document.
struct ImportPrompt {
    path: PathBuf,
    source: LoadedPdf,
    /// The pages as typed: "1-3, 5".
    text: String,
    /// `text` is still the suggestion (every page), which typing replaces.
    untouched: bool,
}

/// A PDF that pages were added from can't be read again, on reopening the
/// document they were added to.
#[derive(Debug)]
struct ImportGone(PathBuf);

impl std::fmt::Display for ImportGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} can't be read any more", self.0.display())
    }
}

impl std::error::Error for ImportGone {}

/// The most a file to open may hold: more is not a document this can work on.
const MAX_FILE_BYTES: u64 = 2 << 30;

/// Read a file to open. Its path can come from outside the app (the command
/// line, another app), and name anything: only a real file is read, not a
/// device or a pipe, and not more than could be a document.
fn read_document(path: &Path) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::metadata(path)?;
    anyhow::ensure!(file.is_file(), "{} is not a file", path.display());
    anyhow::ensure!(file.len() <= MAX_FILE_BYTES, "{} is too large to open", path.display());
    Ok(std::fs::read(path)?)
}

/// `base` with the pages added to it again that a Recents entry remembers.
fn with_imports(base: LoadedPdf, imports: &[PageImport]) -> anyhow::Result<LoadedPdf> {
    if imports.is_empty() {
        return Ok(base);
    }
    let gone = |import: &PageImport| anyhow::Error::new(ImportGone(import.path.clone()));
    let mut sources = Vec::with_capacity(imports.len());
    for import in imports {
        let source = read_document(&import.path).ok().and_then(|bytes| pdf::open_as_source(bytes, None).ok());
        sources.push(source.ok_or_else(|| gone(import))?);
    }
    let additions: Vec<_> = sources.iter().zip(imports).map(|(source, import)| (source, import.pages.as_slice())).collect();
    base.with_pages(&additions).map_err(|_| gone(&imports[0]))
}

/// The pages (from 0) that `text` names out of `count`: numbers and ranges
/// like "1-3, 5", in the order given, each once. Or what's wrong with it.
fn parse_page_ranges(text: &str, count: usize) -> Result<Vec<usize>, String> {
    let page = |number: &str| match number.parse::<usize>() {
        Ok(n) if (1..=count).contains(&n) => Ok(n - 1),
        Ok(n) => Err(format!("There's no page {n}: it has {count} pages.")),
        Err(_) => Err("Type page numbers and ranges, like 1-3, 5.".to_string()),
    };
    // "1 - 3" is a range too.
    let text = text.replace(['–', '—'], "-").split('-').map(str::trim).collect::<Vec<_>>().join("-");
    let mut pages = Vec::new();
    for part in text.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()) {
        let (first, last) = match part.split_once('-') {
            Some((first, last)) => (page(first)?, page(last)?),
            None => (page(part)?, page(part)?),
        };
        let range: Vec<usize> = if first <= last { (first..=last).collect() } else { (last..=first).rev().collect() };
        for p in range {
            if !pages.contains(&p) {
                pages.push(p);
            }
        }
    }
    if pages.is_empty() {
        return Err("Type page numbers and ranges, like 1-3, 5.".to_string());
    }
    Ok(pages)
}

/// A stretch of a page's own text picked out with the Select text tool.
#[derive(Clone, Copy, PartialEq, Debug)]
struct TextSelection {
    /// The page, by original index.
    source: usize,
    /// Where the drag began and where it has got to: places between the
    /// page's characters (see `search::PageText::caret_at`).
    anchor: usize,
    head: usize,
}

impl TextSelection {
    fn range(&self) -> std::ops::Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
}

/// Looking for text in the document (⌘F): the find bar is open.
#[derive(Default)]
struct FindBar {
    query: String,
    /// The bar has the keyboard: typing goes into `query`.
    focused: bool,
    /// Where `query` occurs on each page, by original index (nowhere, on pages not read yet).
    hits: Vec<Vec<Vec<search::Area>>>,
    /// The occurrence being shown: its page (original index) and which one on that page.
    current: Option<(usize, usize)>,
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
    /// Say that the saved copy was opened, as a PDF that pages were added
    /// from (this) can't be read any more.
    NoteImportGone(PathBuf),
}

/// Where a finished copy goes.
#[derive(Clone, Copy, PartialEq, Debug)]
enum SaveTo {
    /// A file, where the user says.
    File,
    /// The clipboard, to paste into an email, a chat or a folder.
    Clipboard,
}

#[derive(Clone, PartialEq)]
enum PasswordPurpose {
    /// Opening the document at `path`, and what to do once it's open.
    Open { then: Option<AfterOpen> },
    /// Choosing the password for a saved copy; `first` is set once it's been
    /// typed, while it's typed again to confirm.
    NewPassword { first: Option<String>, to: SaveTo },
    /// Reading the PDF at `path` to add pages from it.
    Import,
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
    import_prompt: Option<ImportPrompt>,
    find: Option<FindBar>,
    /// The pages' text being read (see `read_text`).
    text_task: Option<Task<()>>,
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
    help_menu_open: bool,
    /// Set when a sidebar was shown or hidden: the page view's new size is
    /// only known after the next layout, so "fit" zoom has to be worked out again.
    refit: bool,
    /// Which shape the "Annotate" section's shape row draws, and whether its menu is open.
    shape_tool: Tool,
    shape_menu_open: bool,
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
    /// The page whose thumbnail the rail was last brought to (see `follow_in_rail`).
    rail_page: Option<usize>,
    /// Scroll state of the signature list and of the gallery of recent documents.
    signatures_scroll: ScrollHandle,
    gallery_scroll: ScrollHandle,
    /// The scroll bar being dragged.
    scrollbar_held: scrollbar::Held,
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
    render_budget: usize,
    drawn_around: Option<DrawnAround>,
    /// Text picked out with the Select text tool, and whether the drag that
    /// picks it out is under way.
    text_selection: Option<TextSelection>,
    selecting_text: bool,
    /// The document being read and parsed, to be shown when that's done.
    open_task: Option<Task<()>>,
    /// Which document that is, until it is shown (or can't be).
    opening: Option<PathBuf>,
    /// The installed fonts being found (see `load_fonts`).
    fonts_task: Option<Task<()>>,
    thumb_task: Option<Task<()>>,
    /// The pages (original indices) `thumb_task` was started for.
    thumbs_pending: Vec<usize>,
    _appearance_observer: Subscription,
    /// The app's updater (absent in tests), and our subscriptions to it.
    updater: Option<Entity<Updater>>,
    _updater_subscriptions: Vec<Subscription>,
    /// "Check for Updates" was chosen in this window, which gives the answer.
    asked_for_updates: bool,
}

/// Every window and its editor. The windows show the same signatures, Recents
/// and settings, and a document is open in one of them at a time.
#[derive(Default)]
struct Windows(Vec<(AnyWindowHandle, WeakEntity<Editor>)>);

impl Global for Windows {}

/// The open windows and their editors.
fn editor_windows(cx: &App) -> Vec<(AnyWindowHandle, Entity<Editor>)> {
    let open = cx.windows();
    let Some(windows) = cx.try_global::<Windows>() else { return Vec::new() };
    windows.0.iter().filter(|(window, _)| open.contains(window)).filter_map(|(window, editor)| Some((*window, editor.upgrade()?))).collect()
}

/// Whether two paths name one file, however they are spelled.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
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

        let this = (window.window_handle(), cx.weak_entity());
        let windows = cx.default_global::<Windows>();
        windows.0.retain(|(_, editor)| editor.upgrade().is_some());
        windows.0.push(this);
        // Closing the window asks first if that would lose changes.
        let editor = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| editor.update(cx, |this, cx| this.may_close(window, cx)).unwrap_or(true));

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
            import_prompt: None,
            find: None,
            text_task: None,
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
            help_menu_open: false,
            refit: false,
            shape_tool: Tool::Rectangle,
            shape_menu_open: false,
            menu_dismissed: false,
            settings: Settings::load(),
            pages_scroll: ScrollHandle::new(),
            page_slots: Vec::new(),
            painted_scroll: Rc::new(Cell::new(Point::default())),
            thumbs_scroll: ScrollHandle::new(),
            painted_thumb: Rc::default(),
            rail_page: None,
            signatures_scroll: ScrollHandle::new(),
            gallery_scroll: ScrollHandle::new(),
            scrollbar_held: Rc::default(),
            painted_zoom: Rc::new(Cell::new(0.)),
            pan: None,
            _pinch_task: {
                let mut pinches = gestures::subscribe(window);
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
            render_budget: RENDER_BUDGET,
            drawn_around: None,
            text_selection: None,
            selecting_text: false,
            open_task: None,
            opening: None,
            fonts_task: None,
            thumb_task: None,
            thumbs_pending: Vec::new(),
            _appearance_observer: appearance_observer,
            updater: updater.clone(),
            _updater_subscriptions: updater
                .iter()
                .flat_map(|u| [cx.observe(u, |_, _, cx| cx.notify()), cx.subscribe_in(u, window, Self::on_update_event)])
                .collect(),
            asked_for_updates: false,
        }
    }

    // -----------------------------------------------------------------------
    // Windows
    // -----------------------------------------------------------------------

    /// The other windows and their editors.
    fn others(&self, cx: &Context<Self>) -> Vec<(AnyWindowHandle, Entity<Editor>)> {
        let me = cx.entity_id();
        editor_windows(cx).into_iter().filter(|(_, editor)| editor.entity_id() != me).collect()
    }

    /// Hand the other windows the signatures, Recents and settings, after one of
    /// them changed here: they are the same in every window.
    fn share(&self, cx: &mut Context<Self>) {
        for (_, editor) in self.others(cx) {
            editor.update(cx, |other, cx| {
                other.signatures = self.signatures.clone();
                other.recents = self.recents.clone();
                // Except which sidebars are shown, which is each window's own business.
                let sidebars = (other.settings.show_left_sidebar, other.settings.show_right_sidebar);
                other.settings = self.settings.clone();
                (other.settings.show_left_sidebar, other.settings.show_right_sidebar) = sidebars;
                cx.notify();
            });
        }
    }

    /// Whether the window may close now. With unsaved changes it asks first, and
    /// closes when the answer is to discard them.
    fn may_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.finish_editing(cx);
        let Some(confirm) = self.confirm_discard(window, cx) else { return true };
        cx.spawn_in(window, async move |_, cx| {
            if confirm.await.ok() == Some(0) {
                cx.update(|window, _| window.remove_window()).ok();
            }
        })
        .detach();
        false
    }

    /// Whether this window has the document open, or is about to.
    fn shows(&self, path: &Path) -> bool {
        let open = self.doc.as_ref().map(|doc| &doc.path);
        let asked = self.password_prompt.as_ref().filter(|p| matches!(p.purpose, PasswordPurpose::Open { .. })).map(|p| &p.path);
        [open, self.opening.as_ref(), asked].into_iter().flatten().any(|shown| same_file(shown, path))
    }

    /// Whether this window has nothing in it, so a document can open here.
    fn is_free(&self) -> bool {
        self.doc.is_none() && self.opening.is_none() && self.password_prompt.is_none()
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

    /// Open a PDF: the one given, or those picked in the file dialog. Each opens in
    /// this window if nothing is open here, and in a window of its own otherwise.
    pub fn request_open(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let here = window.window_handle();
        if let Some(path) = path {
            open_document(cx, path, Some(here));
            return;
        }
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open PDF".into()),
        });
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(paths))) = picker.await else { return };
            cx.update(|cx| {
                for path in paths {
                    open_document(cx, path, Some(here));
                }
            })
            .ok();
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
        self.opening = Some(path.clone());
        cx.notify();
        let tried_password = password.is_some();
        // Reopened with its edits: the pages added from other PDFs are added again first.
        let imports = match &then {
            Some(AfterOpen::Restore(id)) => self.recents.iter().find(|e| e.id == *id).map(|e| e.imports.clone()),
            _ => None,
        };
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move {
                let loaded = pdf::open_for_editing(read_document(&path)?, password.as_deref())?;
                with_imports(loaded, &imports.unwrap_or_default())
            }
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
        self.opening = None;
        let loaded = match loaded {
            Ok(l) => l,
            Err(e) if e.downcast_ref::<ImportGone>().is_some() => {
                self.password_prompt = None;
                let source = e.downcast_ref::<ImportGone>().map(|gone| gone.0.clone()).unwrap_or_default();
                let saved = match &then {
                    Some(AfterOpen::Restore(id)) => self.recents.iter().find(|e| e.id == *id).and_then(|e| e.saved_copy()).map(Path::to_path_buf),
                    _ => None,
                };
                match saved.filter(|saved| saved.exists()) {
                    Some(saved) => self.open_with_password(saved, None, Some(AfterOpen::NoteImportGone(source)), window, cx),
                    None => self.show_error(
                        &format!("Couldn't open {}", file_name(&path)),
                        &format!("Pages were added to it from {}, which can't be read any more.", source.display()),
                        window,
                        cx,
                    ),
                }
                return;
            }
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
                requested_scale: 0.0,
            })
            .collect();
        let field_count = loaded.form.len();
        // Fields may hold text in any script, which is measured with the installed fonts.
        if field_count > 0 {
            self.load_fonts(cx);
        }
        self.status = format!(
            "Opened {} ({}){}{}",
            file_name(&path),
            match loaded.pages.len() {
                1 => "1 page".to_string(),
                n => format!("{n} pages"),
            },
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
        window.set_window_title(&format!("PatraPDF — {}", file_name(&path)));
        let order = (0..loaded.pages.len()).collect::<Vec<_>>();
        self.doc = Some(OpenDoc {
            path,
            widgets: forms::widgets_by_page(&loaded.form, loaded.pages.len()),
            layouts: RefCell::default(),
            text: loaded.pages.iter().map(|_| None).collect(),
            loaded,
            imports: Vec::new(),
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
        self.thumbs_scroll.set_offset(point(px(0.), px(0.)));
        self.rail_page = None;
        self.ensure_rendered(window, cx);
        self.ensure_thumbnails(window, cx);
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
            Some(AfterOpen::NoteImportGone(source)) => {
                self.status = format!(
                    "{}, which pages were added from, can't be read any more, so the saved copy was opened instead. Its fields are part of the page now.",
                    source.display()
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
        self.import_prompt = None;
        self.find = None;
        self.text_task = None;
        (self.text_selection, self.selecting_text) = (None, false);
        self.render_task = None;
        self.thumb_task = None;
        self.thumbs_pending.clear();
    }

    /// Close the document and go back to the gallery (asking first if there are unsaved changes).
    /// From the gallery there is nothing to go back to: the window closes, unless it is the last.
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            if !self.others(cx).is_empty() {
                window.remove_window();
            }
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
        window.set_window_title("PatraPDF");
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
            PasswordPurpose::Import => {
                let (path, password) = (prompt.path.clone(), prompt.text.clone());
                self.read_pages_from(path, Some(password), window, cx);
            }
            PasswordPurpose::NewPassword { first, to } => {
                let to = *to;
                let typed = std::mem::take(&mut prompt.text);
                match first.take() {
                    // Now type it again.
                    None => {
                        *first = Some(typed);
                        prompt.wrong = false;
                    }
                    Some(first) if first == typed => {
                        self.password_prompt = None;
                        self.save_with_password(Some(first), to, window, cx);
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
        // Open in another window already: show that one.
        if let Some((other, _)) = self.others(cx).into_iter().find(|(_, editor)| editor.read(cx).shows(&entry.original)) {
            let _ = other.update(cx, |_, window, _| window.activate_window());
            return;
        }
        if entry.original.exists() {
            self.open_with_password(entry.original, None, Some(AfterOpen::Restore(entry.id)), window, cx);
        } else if let Some(saved) = entry.saved_copy().filter(|saved| saved.exists()).map(Path::to_path_buf) {
            self.open_with_password(saved, None, Some(AfterOpen::NoteMoved(entry.original)), window, cx);
        } else {
            let gone = match entry.saved_copy() {
                Some(saved) => format!("Neither {} nor the saved copy {} exists", entry.original.display(), saved.display()),
                None => format!("{} doesn't exist", entry.original.display()),
            };
            self.show_error("Couldn't find this document", &format!("{gone} any more. You can remove it from Recents."), window, cx);
        }
    }

    /// Bring back the fields, watermark and page order saved with a recent entry
    /// (its original has just been opened).
    fn restore_recent(&mut self, entry: &RecentEntry, cx: &mut Context<Self>) {
        if let Some(doc) = &mut self.doc {
            // Added again while it was opened (see `open_with_password`).
            doc.imports = entry.imports.clone();
        }
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
            doc.last_output = entry.saved_copy().map(Path::to_path_buf);
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
                this.share(cx);
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
        self.share(cx);
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // Find
    // -----------------------------------------------------------------------

    /// Whether typing goes into the find bar.
    fn find_focused(&self) -> bool {
        self.doc.is_some() && self.editing.is_none() && self.field_editing.is_none() && self.find.as_ref().is_some_and(|f| f.focused)
    }

    /// ⌘F: look for text in the document, or among the recent documents when none is open.
    fn start_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            if !self.recents.is_empty() {
                self.start_recents_search(window, cx);
            }
            return;
        }
        self.finish_editing(cx);
        self.close_menus();
        window.focus(&self.focus);
        self.find.get_or_insert_default().focused = true;
        self.ime_marked = None;
        self.read_text(cx);
        cx.notify();
    }

    /// Take a tool in hand, or put it down if it is the one in hand.
    fn take_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        self.tool = if self.tool == tool { Tool::Select } else { tool };
        self.text_selection = None;
        if self.tool == Tool::SelectText {
            // The pages' text is read for it, as for finding.
            self.selected = None;
            self.read_text(cx);
        }
        cx.notify();
    }

    /// The place in a page's text nearest a point in the window, if the page's text has been read.
    fn text_caret_at(&self, position: usize, at: Point<Pixels>) -> Option<(usize, usize)> {
        let doc = self.doc.as_ref()?;
        let source = *doc.order.get(position)?;
        let origin = self.live_page_bounds(position)?.origin;
        let (x, y) = ((at.x - origin.x) / px(self.zoom), (at.y - origin.y) / px(self.zoom));
        Some((source, doc.text.get(source)?.as_ref()?.caret_at(x, y)?))
    }

    /// The text picked out, if any is.
    fn selected_text(&self) -> Option<String> {
        let selection = self.text_selection?;
        let text = self.doc.as_ref()?.text.get(selection.source)?.as_ref()?.text(selection.range());
        (!text.is_empty()).then_some(text)
    }

    /// Say how much text is picked out, if any is.
    fn announce_selected_text(&mut self) -> bool {
        let Some(text) = self.selected_text() else { return false };
        self.status = format!("{} selected. Press {} to copy.", count(text.chars().count(), "character"), ui::keys("⌘C")).into();
        true
    }

    /// The text picked out on a page (by original index), shown as on any screen.
    fn render_text_selection(&self, source: usize) -> Vec<gpui::Div> {
        let Some(selection) = self.text_selection.filter(|selection| selection.source == source) else { return Vec::new() };
        let Some(text) = self.doc.as_ref().and_then(|doc| doc.text.get(source)?.as_ref()) else { return Vec::new() };
        let z = self.zoom;
        let mark = |[x, y, w, h]: search::Area| div().absolute().left(px(x * z)).top(px(y * z)).w(px(w * z)).h(px(h * z)).bg(rgba(0x3b82f659));
        text.boxes(selection.range()).into_iter().map(mark).collect()
    }

    /// The find bar stays open, but typing goes elsewhere.
    fn unfocus_find(&mut self) {
        if let Some(find) = &mut self.find {
            find.focused = false;
        }
    }

    fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find = None;
        self.ime_marked = None;
        cx.notify();
    }

    /// Read the text of the pages that haven't been read, in the background,
    /// looking through each as it arrives.
    fn read_text(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
        let unread: Vec<usize> = (0..doc.text.len()).filter(|&i| doc.text[i].is_none()).collect();
        if unread.is_empty() || self.text_task.is_some() {
            return;
        }
        let (pdf, path) = (doc.loaded.pdf.clone(), doc.path.clone());
        let (done, mut read) = mpsc::unbounded();
        let reading = cx
            .background_executor()
            .spawn(async move { search::read_pages(&pdf, unread, |index, text| done.unbounded_send((index, text)).is_ok()) });
        self.text_task = Some(cx.spawn(async move |this, cx| {
            // Kept for as long as this task: once it goes, the reading stops at the next page.
            let _reading = reading;
            while let Some((index, text)) = read.next().await {
                let kept = this.update(cx, |this, cx| {
                    // Unless another document was opened meanwhile.
                    let Some(slot) = this.doc.as_mut().filter(|d| d.path == path).and_then(|d| d.text.get_mut(index)) else {
                        return false;
                    };
                    *slot = Some(text);
                    this.look_through(Some(index), cx);
                    true
                });
                if !kept.unwrap_or(false) {
                    break;
                }
            }
        }));
    }

    /// Whether some of the document's text is still to be read.
    fn reading_text(&self) -> bool {
        self.doc.as_ref().is_some_and(|doc| doc.text.iter().any(Option::is_none))
    }

    /// Find where the phrase occurs on one page (it has just been read), or on
    /// all of them (the phrase changed), and show the first if none is shown.
    fn look_through(&mut self, page: Option<usize>, cx: &mut Context<Self>) {
        let (Some(find), Some(doc)) = (&mut self.find, &self.doc) else { return };
        let phrase = search::fold(&find.query);
        find.hits.resize(doc.text.len(), Vec::new());
        for index in page.map_or(0..doc.text.len(), |index| index..index + 1) {
            find.hits[index] = doc.text[index].as_ref().map(|text| text.find(&phrase)).unwrap_or_default();
        }
        if find.current.is_none() {
            // The first from the page in view on; from the top again once they have all been read.
            let from = self.current_page().unwrap_or(0);
            let hits = self.find_hits();
            let first = hits.iter().find(|hit| hit.0 >= from).or_else(|| hits.first().filter(|_| !self.reading_text())).copied();
            if let Some(hit) = first {
                self.show_hit(hit, cx);
            }
        }
        cx.notify();
    }

    fn set_find_query(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else { return };
        find.query = query;
        find.current = None;
        self.look_through(None, cx);
    }

    /// Every occurrence, in the order the pages are shown: the page's position,
    /// its original index, and which occurrence on the page.
    fn find_hits(&self) -> Vec<(usize, usize, usize)> {
        let (Some(find), Some(doc)) = (&self.find, &self.doc) else { return Vec::new() };
        doc.order
            .iter()
            .enumerate()
            .flat_map(|(position, &source)| (0..find.hits.get(source).map_or(0, Vec::len)).map(move |which| (position, source, which)))
            .collect()
    }

    /// Which of `hits` is the one being shown.
    fn current_hit(&self, hits: &[(usize, usize, usize)]) -> Option<usize> {
        let current = self.find.as_ref()?.current?;
        hits.iter().position(|&(_, source, which)| (source, which) == current)
    }

    /// Go to the next occurrence, or the one before, around the ends.
    fn find_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            return self.start_find(window, cx);
        }
        let hits = self.find_hits();
        if hits.is_empty() {
            return;
        }
        let next = match (self.current_hit(&hits), forward) {
            (Some(at), true) => (at + 1) % hits.len(),
            (Some(at), false) => (at + hits.len() - 1) % hits.len(),
            (None, true) => 0,
            (None, false) => hits.len() - 1,
        };
        self.show_hit(hits[next], cx);
    }

    /// Make an occurrence the current one and bring it into view.
    fn show_hit(&mut self, (position, source, which): (usize, usize, usize), cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else { return };
        find.current = Some((source, which));
        let area = find.hits.get(source).and_then(|hits| hits.get(which)).and_then(|areas| areas.first()).copied();
        if let (Some([x, y, w, h]), Some(page)) = (area, self.live_page_bounds(position)) {
            let z = self.zoom;
            let hit = Bounds::new(page.origin + point(px(x * z), px(y * z)), size(px(w * z), px(h * z)));
            let viewport = self.pages_scroll.bounds();
            let mut offset = self.pages_scroll.offset();
            // Clear of the viewport's edges, and of the find bar at its top.
            if hit.top() < viewport.top() + px(64.) || hit.bottom() > viewport.bottom() - px(32.) {
                offset.y -= hit.top() - (viewport.top() + viewport.size.height / 3.);
            }
            if hit.left() < viewport.left() + px(32.) || hit.right() > viewport.right() - px(32.) {
                offset.x -= hit.center().x - viewport.center().x;
            }
            let max = self.pages_scroll.max_offset();
            offset = point(offset.x.clamp(-max.width, px(0.)), offset.y.clamp(-max.height, px(0.)));
            self.pages_scroll.set_offset(offset);
        }
        cx.notify();
    }

    /// The find bar, floating over the top right of the pages.
    fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let find = self.find.as_ref()?;
        let focused = self.find_focused();
        let hits = self.find_hits();
        let count: SharedString = match (find.query.trim().is_empty(), hits.len(), self.current_hit(&hits)) {
            (true, ..) => "".into(),
            (_, 0, _) if self.reading_text() => "Searching…".into(),
            (_, 0, _) => "No matches".into(),
            (_, 1, None) => "1 match".into(),
            (_, n, None) => format!("{n} matches").into(),
            (_, n, Some(at)) => format!("{} of {n}", at + 1).into(),
        };
        let caret = focused.then(|| ui::caret(px(15.)).relative().flex_none().child(self.input_anchor(cx)));
        let step = |id: &'static str, icon_name: &'static str, label: &'static str, keys: &'static str, forward: bool| {
            ui::icon_button(id, icon_name)
                .size(px(24.))
                .flex_none()
                .tooltip(ui::tooltip(label, Some(keys)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.find_step(forward, window, cx)))
        };
        let bar = ui::text_field(if focused { ui::accent() } else { ui::border_strong() })
            .id("find-bar")
            .gap_1()
            .w(px(340.))
            .h(px(36.))
            .shadow(if focused { ui::ring() } else { ui::lifted_shadow() })
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.start_find(window, cx);
                }),
            )
            .child(ui::icon("search", ui::text_tertiary()).size(px(15.)).flex_none())
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .ml_1()
                    .items_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(ui::text_primary())
                    .when(!find.query.is_empty(), |d| d.child(find.query.clone()))
                    .children(caret)
                    .when(find.query.is_empty(), |d| d.child(div().text_color(ui::text_tertiary()).child("Find in document"))),
            )
            .child(div().flex_none().px_1().text_xs().text_color(ui::text_tertiary()).child(count))
            .child(step("find-previous", "chevron-up", "Previous match", "⇧⌘G", false))
            .child(step("find-next", "chevron-down", "Next match", "⌘G", true))
            .child(
                ui::icon_button("find-close", "close")
                    .size(px(24.))
                    .flex_none()
                    .tooltip(ui::tooltip("Close", Some("Esc")))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_find(cx))),
            );
        Some(div().absolute().top(px(12.)).right(px(20.)).child(bar).into_any_element())
    }

    /// The occurrences on a page, marked over it: the current one stands out.
    fn render_find_marks(&self, source: usize) -> Vec<gpui::Div> {
        let Some(find) = &self.find else { return Vec::new() };
        let z = self.zoom;
        let mut marks = Vec::new();
        for (which, areas) in find.hits.get(source).into_iter().flatten().enumerate() {
            let color = if find.current == Some((source, which)) { rgba(0xff8c00a0) } else { rgba(0xffd60a70) };
            for &[x, y, w, h] in areas {
                marks.push(div().absolute().left(px(x * z - 1.)).top(px(y * z - 1.)).w(px(w * z + 2.)).h(px(h * z + 2.)).rounded_xs().bg(color));
            }
        }
        marks
    }

    /// Render the small page images: for the pages in sight in the thumbnail
    /// rail, and for those around the page in view. The rest are drawn when
    /// they are scrolled to, and let go once many are held: a long document
    /// has hundreds of pages.
    fn ensure_thumbnails(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_page().unwrap_or(0);
        // Before the rail has been laid out, which thumbnails are in sight isn't known.
        let rail = self.settings.show_right_sidebar && self.painted_thumb.get().is_some() && self.thumbs_scroll.bounds().size.height > px(0.);
        let in_rail = if rail { self.drawn_thumbs() } else { Vec::new() };
        let Some(doc) = &mut self.doc else { return };
        // The pages (positions) to have thumbnails of, nearest the one in view first.
        let mut wanted: Vec<usize> =
            (0..doc.order.len()).filter(|&p| p.abs_diff(current) <= THUMBS_AROUND || in_rail.get(p).is_some_and(|&shown| shown)).collect();
        wanted.sort_by_key(|&position| position.abs_diff(current));
        let wanted: Vec<usize> = wanted.into_iter().map(|position| doc.order[position]).collect();
        if doc.pages.iter().filter(|page| page.thumb.is_some()).count() > THUMBS_KEPT {
            let mut keep = vec![false; doc.pages.len()];
            wanted.iter().for_each(|&i| keep[i] = true);
            for (page, _) in doc.pages.iter_mut().zip(keep).filter(|(_, keep)| !keep) {
                if let Some(old) = page.thumb.take() {
                    cx.drop_image(old, Some(window));
                }
            }
        }
        let missing: Vec<usize> = wanted.into_iter().filter(|&i| doc.pages[i].thumb.is_none()).collect();
        // Nothing to do, either, if the render under way is drawing them.
        if missing.iter().all(|i| self.thumbs_pending.contains(i)) {
            return;
        }
        let path = doc.path.clone();
        let pages: Vec<(usize, f32)> = missing.iter().map(|&i| (i, THUMB_RENDER_PX / doc.loaded.pages[i].width.max(1.))).collect();
        let pdf = doc.loaded.pdf.clone();
        self.thumbs_pending = missing;
        // (This replaces `thumb_task`, which stops the render it was waiting for.)
        self.thumb_task = Some(cx.spawn_in(window, async move |this, cx| {
            let (mut rendered, _rendering) = render_in_background(pdf, pages, cx.background_executor());
            while let Some((index, page)) = rendered.next().await {
                let Some(image) = page.and_then(to_render_image) else { continue };
                let ok = this.update_in(cx, |this, window, cx| {
                    let Some(doc) = this.doc.as_mut().filter(|d| d.path == path) else { return false };
                    if let Some(old) = doc.pages[index].thumb.replace(image) {
                        cx.drop_image(old, Some(window));
                    }
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
        // As sharp as the screen shows it. A bitmap will do while it is nearly
        // that sharp, and not so much sharper (after zooming out) that it takes
        // far more memory than it need.
        let scale = (self.zoom * window.scale_factor()).clamp(1.0, MAX_RENDER_SCALE);
        let fits = |drawn: f32| drawn >= scale * 0.95 && drawn <= scale * 1.5;
        let current = self.current_page().unwrap_or(0);
        let budget = self.render_budget;
        let Some(doc) = &mut self.doc else { return };
        // The displayed pages to have drawn (original indices): the page in view
        // first, then outwards, for as far as the budget goes.
        let sizes: Vec<(f32, f32)> = doc.geoms().map(|g| (g.width, g.height)).collect();
        let near = pages_in_budget(&sizes, current, scale, budget);
        self.drawn_around =
            (near.len() < doc.order.len()).then(|| DrawnAround { page: current, slack: (near.len() / 4).max(1), order: doc.order.clone() });
        let near: Vec<usize> = near.into_iter().map(|position| doc.order[position]).collect();
        if self.drawn_around.is_some() {
            // The others give up their bitmaps.
            let mut keep = vec![false; doc.pages.len()];
            near.iter().for_each(|&i| keep[i] = true);
            for (page, _) in doc.pages.iter_mut().zip(keep).filter(|(_, keep)| !keep) {
                if let Some(old) = page.image.take() {
                    cx.drop_image(old, Some(window));
                }
                (page.rendered_scale, page.requested_scale) = (0., 0.);
            }
        }
        let stale: Vec<usize> = near.into_iter().filter(|&i| !fits(doc.pages[i].rendered_scale)).collect();
        // Nothing to do, either, if the render under way is drawing them sharply
        // enough: when a document opens, the zoom that fits the window is only
        // known a frame after its pages were first asked for.
        if stale.iter().all(|&i| fits(doc.pages[i].requested_scale)) {
            return;
        }
        stale.iter().for_each(|&i| doc.pages[i].requested_scale = scale);
        // While zooming continuously (pinch, wheel), the stretched bitmaps are fine
        // for a moment; wait for the zoom to settle rather than re-render every step.
        // (Each call replaces `render_task`, cancelling the previous wait.) A
        // page with nothing on screen yet renders at once.
        let settle = stale.iter().all(|&i| doc.pages[i].image.is_some());
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

    /// Make a copy of the document with everything added to it: a file, or on the clipboard.
    fn save(&mut self, to: SaveTo, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        let Some(doc) = &self.doc else {
            self.status = "Nothing to save — open a PDF first.".into();
            cx.notify();
            return;
        };
        let Some(password) = doc.loaded.password.clone() else {
            self.save_with_password(None, to, window, cx);
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
                0 => this.save_with_password(Some(password), to, window, cx),
                1 => this.save_with_new_password(to, window, cx),
                2 => this.save_with_password(None, to, window, cx),
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// Ask for a password (typed twice), then save a copy that needs it to open.
    fn save_with_new_password(&mut self, to: SaveTo, window: &mut Window, cx: &mut Context<Self>) {
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
            purpose: PasswordPurpose::NewPassword { first: None, to },
        });
        window.focus(&self.focus);
        cx.notify();
    }

    /// Make a copy, encrypted with `password` if set: saved where the user
    /// says, or put on the clipboard.
    fn save_with_password(&mut self, password: Option<String>, to: SaveTo, window: &mut Window, cx: &mut Context<Self>) {
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
        let picker = (to == SaveTo::File).then(|| cx.prompt_for_new_path(&dir, Some(&suggested)));

        let original = doc.path.clone();
        let last_output = doc.last_output.clone();
        let loaded = doc.loaded.clone();
        let imports = doc.imports_in_use();
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
            let target = match picker {
                Some(picker) => {
                    let Ok(Ok(Some(path))) = picker.await else { return };
                    Some(path)
                }
                None => None,
            };
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
                    // What goes on the clipboard is a file too: one in the app's own folder.
                    let path = match target {
                        Some(path) => {
                            std::fs::write(&path, &written.bytes)?;
                            path
                        }
                        None => clipboard::keep(&suggested, &written.bytes)?,
                    };
                    // No thumbnail of a protected document: it would be an unprotected
                    // picture of its first page.
                    let thumbnail = if protected { None } else { recents::make_thumbnail(written.bytes) };
                    anyhow::Ok((path, thumbnail, written.missing))
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok((path, thumbnail, missing)) => {
                    if to == SaveTo::Clipboard {
                        if let Err(e) = clipboard::put(&path, cx) {
                            this.show_error("Couldn't copy the PDF to the clipboard", &format!("{e:#}"), window, cx);
                            return;
                        }
                    }
                    // What Recents has as the saved copy is a file the user chose
                    // (the one on the clipboard is not theirs to keep).
                    let output = match to {
                        SaveTo::File => path.clone(),
                        SaveTo::Clipboard => last_output.unwrap_or_else(|| original.clone()),
                    };
                    // Recents are stored unencrypted, so for a protected document they
                    // remember only where it is, not what was typed into it.
                    let contents = (!protected).then(|| recents::Contents {
                        items: &saved.items,
                        watermark: saved.watermark.as_ref(),
                        fields: filled,
                        thumbnail,
                    });
                    let reordered = Some(saved.order.as_slice()).filter(|o| !o.iter().enumerate().all(|(i, &p)| i == p));
                    match recents::record(this.recents.clone(), &original, &output, page_count, reordered, &imports, contents) {
                        Ok(entries) => {
                            this.recents = entries;
                            this.share(cx);
                        }
                        Err(e) => eprintln!("could not update recents: {e:#}"),
                    }
                    // Unless another document was opened while the save was running.
                    if let Some(doc) = this.doc.as_mut().filter(|d| d.path == original) {
                        if to == SaveTo::File {
                            doc.last_output = Some(path.clone());
                        }
                        // A protected copy on the clipboard is all there is of the
                        // work (Recents doesn't remember it): it still counts as unsaved.
                        if to == SaveTo::File || !protected {
                            this.saved = saved;
                            // Only clear "Edited" if nothing changed meanwhile.
                            if this.revision == revision {
                                this.dirty = false;
                            }
                        }
                    }
                    let done = match to {
                        SaveTo::File => format!("Saved {}{protection_note}", path.display()),
                        SaveTo::Clipboard => format!("Copied {}{protection_note} to the clipboard, ready to paste", file_name(&path)),
                    };
                    this.status = match redacted_pages {
                        0 => done,
                        n => format!(
                            "{done} · {n} redacted page{} saved as image{} (their text is no longer selectable)",
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
                Err(e) => {
                    let title = if to == SaveTo::File { "Couldn't save the PDF" } else { "Couldn't copy the PDF" };
                    this.show_error(title, &format!("{e:#}"), window, cx)
                }
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
                self.share(cx);
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
            // so only delete it if no page uses it, in this window or another.
            let placed = |editor: &Editor| editor.items.iter().any(|i| matches!(&i.kind, ItemKind::Image { sig: s } if s.path == sig.path));
            let in_use = placed(self) || self.others(cx).iter().any(|(_, editor)| placed(editor.read(cx)));
            if !in_use {
                signatures::delete(&sig);
            }
            self.share(cx);
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
        let pdfs: Vec<_> = paths.paths().iter().filter(|p| signatures::is_pdf(p)).cloned().collect();
        if !pdfs.is_empty() {
            for pdf in pdfs {
                self.request_open(Some(pdf), window, cx);
            }
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
        // Text picked out of the document itself.
        if let Some(text) = self.selected_text().filter(|_| self.selected.is_none()) {
            self.status = format!("Copied {}.", count(text.chars().count(), "character")).into();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            cx.notify();
            return;
        }
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
        let answer = window.prompt(PromptLevel::Info, "PatraPDF", Some(&detail), &buttons, cx);
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
            self.asked_for_updates = true;
            updater.update(cx, |u, cx| u.check(true, cx));
        }
    }

    fn set_automatic_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.check_for_updates = on;
        self.save_settings(cx);
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
            // Found by the daily check: the status bar offers it, without interrupting.
            updater::Event::Ready { manual: false } => {}
            // Every window hears the answer to "Check for Updates"; the one it was asked in gives it.
            _ if !std::mem::take(&mut self.asked_for_updates) => {}
            updater::Event::UpToDate => {
                let detail = format!("PatraPDF {} is the newest version.", crate::about::VERSION);
                let _ = window.prompt(PromptLevel::Info, "You're up to date", Some(&detail), &["OK"], cx);
            }
            updater::Event::Failed(message) => {
                self.show_update_error("Couldn't check for updates", message, window, cx);
            }
            updater::Event::Ready { manual: true } => self.prompt_install_update(window, cx),
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
        let dirty = self.dirty || self.others(cx).iter().any(|(_, editor)| editor.read(cx).dirty);
        detail.push_str(if dirty {
            "Restarting now discards your unsaved changes. Choose Later to save them first; the update is installed when you quit."
        } else {
            "PatraPDF restarts to finish. Choose Later to install it when you quit."
        });
        let restart = if dirty { "Discard Changes and Restart" } else { "Restart Now" };
        let title = format!("PatraPDF {} is ready to install", release.version);
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

    /// Add pages from another PDF: choose the file, then which of its pages.
    fn add_pages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        self.finish_editing(cx);
        self.close_menus();
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Add Pages".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match picker.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                _ => return,
            };
            this.update_in(cx, |this, window, cx| this.read_pages_from(path, None, window, cx)).ok();
        })
        .detach();
    }

    /// Read the PDF to add pages from, in the background, then ask which
    /// pages (or for its password first).
    fn read_pages_from(&mut self, path: PathBuf, password: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
        let target = doc.path.clone();
        self.status = format!("Reading {}…", file_name(&path)).into();
        cx.notify();
        let tried_password = password.is_some();
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move { pdf::open_as_source(read_document(&path)?, password.as_deref()) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let source = read.await;
            this.update_in(cx, |this, window, cx| {
                // Unless the document was closed meanwhile.
                if this.doc.as_ref().is_some_and(|d| d.path == target) {
                    this.choose_pages_from(path, source, tried_password, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn choose_pages_from(
        &mut self,
        path: PathBuf,
        source: anyhow::Result<LoadedPdf>,
        tried_password: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = match source {
            Ok(source) => source,
            Err(e) if e.downcast_ref::<pdf::PasswordError>().is_some() => {
                self.status = format!("{} is password protected", file_name(&path)).into();
                self.password_prompt =
                    Some(PasswordPrompt { path, text: String::new(), wrong: tried_password, purpose: PasswordPurpose::Import });
                window.focus(&self.focus);
                cx.notify();
                return;
            }
            Err(e) => {
                self.password_prompt = None;
                self.show_error(&format!("Couldn't read {}", file_name(&path)), &e.to_string(), window, cx);
                return;
            }
        };
        self.password_prompt = None;
        let count = source.pages.len();
        if count == 1 {
            // Nothing to choose.
            self.import_pages(path, source, vec![0], window, cx);
            return;
        }
        self.status = format!("{} has {count} pages", file_name(&path)).into();
        self.import_prompt = Some(ImportPrompt { path, source, text: format!("1-{count}"), untouched: true });
        window.focus(&self.focus);
        cx.notify();
    }

    fn submit_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // What's typed has to make sense first: the prompt says what's wrong with it.
        let Some(pages) = self.import_prompt.as_ref().and_then(|p| parse_page_ranges(&p.text, p.source.pages.len()).ok()) else {
            return;
        };
        if let Some(prompt) = self.import_prompt.take() {
            self.import_pages(prompt.path, prompt.source, pages, window, cx);
        }
    }

    fn cancel_import(&mut self, cx: &mut Context<Self>) {
        self.import_prompt = None;
        self.status = "No pages added".into();
        cx.notify();
    }

    /// Add `pages` of `source` (the PDF at `path`) after the document's last
    /// page. They're put together in the background.
    fn import_pages(&mut self, path: PathBuf, source: LoadedPdf, pages: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else { return };
        let (base, target, base_pages) = (doc.loaded.clone(), doc.path.clone(), doc.loaded.pages.len());
        self.status = format!("Adding pages from {}…", file_name(&path)).into();
        cx.notify();
        let merge = cx.background_executor().spawn({
            let pages = pages.clone();
            async move { base.with_pages(&[(&source, &pages)]) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let merged = merge.await;
            this.update_in(cx, |this, window, cx| {
                // Unless the document was closed meanwhile.
                let Some(doc) = this.doc.as_ref().filter(|d| d.path == target) else { return };
                match merged {
                    // Other pages were added in the meantime: these went onto the document as it was before.
                    Ok(_) if doc.loaded.pages.len() != base_pages => {
                        this.status = format!("Pages from {} weren't added. Add them again.", file_name(&path)).into();
                        cx.notify();
                    }
                    Ok(merged) => this.pages_added(PageImport { path, pages }, merged, window, cx),
                    Err(e) => this.show_error(&format!("Couldn't add pages from {}", file_name(&path)), &format!("{e:#}"), window, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Show the pages just added: `merged` is the document with them after
    /// its own. Undoable.
    fn pages_added(&mut self, import: PageImport, merged: LoadedPdf, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        self.checkpoint();
        let Some(doc) = &mut self.doc else { return };
        let (first, count) = (doc.loaded.pages.len(), merged.pages.len());
        doc.pages.extend((first..count).map(|_| PageView { image: None, thumb: None, rendered_scale: 0.0, requested_scale: 0.0 }));
        doc.text.extend((first..count).map(|_| None));
        doc.widgets = forms::widgets_by_page(&merged.form, count);
        doc.loaded = merged;
        // The pages stay in the document from here on; undo only takes them out of `order`.
        let position = doc.order.len();
        doc.order.extend(first..count);
        self.status = format!(
            "Added {} from {} as page{} {}. Drag them in the list to move them.",
            match count - first {
                1 => "1 page".to_string(),
                n => format!("{n} pages"),
            },
            file_name(&import.path),
            if count - first == 1 { "" } else { "s" },
            match count - first {
                1 => format!("{}", position + 1),
                n => format!("{}–{}", position + 1, position + n),
            },
        )
        .into();
        doc.imports.push(import);
        self.selected = None;
        self.ensure_rendered(window, cx);
        self.ensure_thumbnails(window, cx);
        // The find bar looks through the new pages too.
        self.text_task = None;
        if self.find.is_some() {
            self.read_text(cx);
        }
        self.scroll_to_page(position, cx);
        self.thumbs_scroll.scroll_to_bottom();
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

    /// Keep the thumbnail of the page in view in sight: when scrolling the
    /// document brings another page into view, the rail is scrolled by as
    /// little as shows that page's thumbnail whole. Only then, so the rail can
    /// be scrolled away from it to look through the other pages.
    fn follow_in_rail(&mut self) {
        let current = self.current_page().filter(|_| self.settings.show_right_sidebar);
        if current == self.rail_page {
            return;
        }
        if let Some(page) = current {
            let viewport = self.thumbs_scroll.bounds();
            let thumb = self.thumb_tops().filter(|_| viewport.size.height > px(0.)).and_then(|mut tops| tops.nth(page));
            // The rail hasn't been laid out yet (it has just been shown): once it has.
            let Some((top, height)) = thumb else { return };
            let margin = px(THUMB_GAP).min((viewport.size.height - height) / 2.).max(px(0.));
            let shift = if top < viewport.top() + margin {
                viewport.top() + margin - top
            } else {
                (viewport.bottom() - margin - top - height).min(px(0.))
            };
            let offset = self.thumbs_scroll.offset();
            let y = (offset.y + shift).clamp(-self.thumbs_scroll.max_offset().height, px(0.));
            self.thumbs_scroll.set_offset(point(offset.x, y));
        }
        self.rail_page = current;
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
        self.save_settings(cx);
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
        self.unfocus_find();
        let Some((page, x, y)) = self.page_at(event.position) else { return };
        match self.tool {
            // Let the press reach the canvas, which starts panning.
            Tool::Select => (self.selected, self.text_selection) = (None, None),
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
                    points: Vec::new(),
                });
            }
            // Text is picked out from where the press is to where it is dragged;
            // a double click picks out a word.
            Tool::SelectText => {
                cx.stop_propagation();
                self.selected = None;
                self.text_selection = None;
                match self.text_caret_at(page, event.position) {
                    Some((source, caret)) => {
                        let word = (event.click_count == 2).then(|| self.doc.as_ref()?.text[source].as_ref().map(|text| text.word_at(caret))).flatten();
                        let (anchor, head) = word.map_or((caret, caret), |word| (word.start, word.end));
                        self.text_selection = Some(TextSelection { source, anchor, head });
                        self.selecting_text = event.click_count < 2;
                        self.announce_selected_text();
                    }
                    None if self.reading_text() => self.status = "Still reading this document's text…".into(),
                    None => self.status = "There is no text on this page to select (a scan is a picture of text).".into(),
                }
            }
            // Annotations are drawn by dragging too.
            tool if tool.draws() => {
                cx.stop_propagation();
                if let Some((kind, mode)) = self.new_annotation(tool) {
                    // It becomes an undo step when the drag ends, if that drew anything.
                    let snapshot = self.snapshot();
                    let id = self.next_id;
                    self.next_id += 1;
                    self.items.push(Item { id, page, x, y, w: 0., h: 0., kind });
                    self.selected = None;
                    self.drag = Some(DragState {
                        id,
                        mode,
                        start_mouse: event.position,
                        start: (x, y, 0., 0.),
                        snapshot,
                        moved: false,
                        points: vec![(x, y)],
                    });
                }
            }
            tool => {
                cx.stop_propagation();
                self.place_tool(tool, page, x, y, cx);
                self.tool = Tool::Select;
            }
        }
        cx.notify();
    }

    /// What a drawing tool starts where it is pressed on the page, and how the drag shapes it.
    fn new_annotation(&self, tool: Tool) -> Option<(ItemKind, DragMode)> {
        let (color, width) = (self.settings.pen_color, self.settings.pen_width);
        let stroke = |arrow| ItemKind::Stroke { points: Arc::default(), color, width, arrow };
        Some(match tool {
            Tool::Highlight => (ItemKind::Highlight { color: self.settings.highlight_color }, DragMode::Draw),
            Tool::Rectangle | Tool::Oval => (ItemKind::Outline { oval: tool == Tool::Oval, color, width }, DragMode::Draw),
            Tool::Pen => (stroke(false), DragMode::Ink),
            Tool::Line | Tool::Arrow => (stroke(tool == Tool::Arrow), DragMode::Line),
            _ => return None,
        })
    }

    /// Choose the highlighter's colour (`highlight`) or the pen's: for what is
    /// drawn next, and for the selected item if it is drawn with that.
    fn set_pen_color(&mut self, highlight: bool, color: u32, cx: &mut Context<Self>) {
        *(if highlight { &mut self.settings.highlight_color } else { &mut self.settings.pen_color }) = color;
        self.pen_changed(cx, |item| item.color().is_some_and(|(h, c)| h == highlight && c != color), |item| item.set_color(color));
    }

    fn set_pen_width(&mut self, width: f32, cx: &mut Context<Self>) {
        self.settings.pen_width = width;
        self.pen_changed(cx, |item| item.pen_width().is_some_and(|w| w != width), |item| item.set_pen_width(width));
    }

    /// Keep a new pen setting, and apply it to the selected item if it `differs` there.
    fn pen_changed(&mut self, cx: &mut Context<Self>, differs: impl Fn(&Item) -> bool, apply: impl FnOnce(&mut Item)) {
        self.save_settings(cx);
        if let Some(id) = self.selected.filter(|id| self.items.iter().any(|i| i.id == *id && differs(i))) {
            self.checkpoint();
            if let Some(item) = self.item_mut(id) {
                apply(item);
            }
        }
        cx.notify();
    }

    /// Create the field for `tool` centred (vertically, for text) on page point (x, y).
    fn place_tool(&mut self, tool: Tool, page: usize, x: f32, y: f32, cx: &mut Context<Self>) {
        match tool {
            Tool::Select | Tool::SelectText => {}
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
            // Drawn by dragging over the page (see `page_mouse_down`).
            Tool::Highlight | Tool::Pen | Tool::Rectangle | Tool::Oval | Tool::Line | Tool::Arrow => {}
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
        self.shape_menu_open = false;
        self.help_menu_open = false;
        self.watermark_menu_open = false;
        self.watermark_editing = false;
        self.choice_open = None;
    }

    fn any_menu_open(&self) -> bool {
        self.zoom_menu_open || self.date_menu_open || self.shape_menu_open || self.help_menu_open || self.watermark_menu_open || self.choice_open.is_some()
    }

    fn toggle_help_menu(&mut self, cx: &mut Context<Self>) {
        let open = !self.help_menu_open;
        self.close_menus();
        self.help_menu_open = open;
        cx.notify();
    }

    fn toggle_left_sidebar(&mut self, cx: &mut Context<Self>) {
        // The date and watermark menus hang off the sidebar.
        self.close_menus();
        self.settings.show_left_sidebar = !self.settings.show_left_sidebar;
        self.sidebars_changed(cx);
    }

    /// The page thumbnails, which are only there with a document open.
    fn toggle_right_sidebar(&mut self, cx: &mut Context<Self>) {
        if self.doc.is_none() {
            return;
        }
        self.settings.show_right_sidebar = !self.settings.show_right_sidebar;
        self.sidebars_changed(cx);
    }

    /// Keep the settings for the next time, and for the other windows (which
    /// keep their own sidebars).
    fn save_settings(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = self.settings.save() {
            self.status = format!("Couldn't save settings: {e:#}").into();
        }
        self.share(cx);
    }

    fn sidebars_changed(&mut self, cx: &mut Context<Self>) {
        self.save_settings(cx);
        self.refit = true;
        cx.notify();
    }

    fn item_mouse_down(&mut self, id: u64, mode: DragMode, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // With a tool in hand that is dragged over the page, the press goes on
        // to the page: it draws, or picks out text, over the item.
        if self.tool.stays() {
            return;
        }
        cx.stop_propagation();
        window.focus(&self.focus);
        self.unfocus_find();
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
            points: Vec::new(),
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
        if self.selecting_text {
            let position = self.text_selection.and_then(|s| self.doc.as_ref()?.order.iter().position(|&source| source == s.source));
            match position.and_then(|position| self.text_caret_at(position, event.position)) {
                Some((_, head)) if event.pressed_button == Some(MouseButton::Left) => {
                    if let Some(selection) = &mut self.text_selection {
                        selection.head = head;
                    }
                }
                _ => self.selecting_text = false,
            }
            cx.notify();
            return;
        }
        let Some(drag) = &mut self.drag else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            // The button was let go where that wasn't seen (outside the window).
            self.finish_drag(cx);
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
        // Where a drag that draws has got to: no further than the page's edge.
        let (mut x, mut y) = (x0 + dx, y0 + dy);
        if let Some(g) = self.page_geom(item.page) {
            (x, y) = (x.clamp(0., g.width), y.clamp(0., g.height));
        }
        match mode {
            DragMode::Move => {
                item.x = x0 + dx;
                item.y = y0 + dy;
            }
            DragMode::Draw => (item.x, item.y, item.w, item.h) = (x0.min(x), y0.min(y), (x - x0).abs(), (y - y0).abs()),
            DragMode::Line | DragMode::Ink => {
                if let Some(drag) = &mut self.drag {
                    if mode == DragMode::Line {
                        drag.points.truncate(1);
                        drag.points.push((x, y));
                    } else if drag.points.last().is_none_or(|last| (x - last.0).hypot(y - last.1) * self.zoom >= 1.5) {
                        // A new point for every pixel and a half the pointer has moved.
                        drag.points.push((x, y));
                    }
                    item.fit_stroke(&drag.points);
                }
            }
            DragMode::Resize if item.stretches() => {
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
        // A line's points are on the page already; the box around a thin one may reach over its edge.
        if !matches!(mode, DragMode::Line | DragMode::Ink) {
            self.clamp_item(&mut item);
        }
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
        self.finish_drag(cx);
    }

    /// The mouse button was let go: what was being dragged or drawn is settled.
    fn finish_drag(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.selecting_text) {
            if !self.announce_selected_text() {
                self.text_selection = None;
            }
            cx.notify();
        }
        if let Some(drag) = self.drag.take() {
            let drawn = self.items.iter().find(|i| i.id == drag.id).filter(|i| drag.mode.draws() && !i.is_redaction()).map(|i| (i.w, i.h));
            if let Some((w, h)) = drawn {
                let reaches = |far: f32| drag.points.iter().any(|p| (p.0 - drag.points[0].0).hypot(p.1 - drag.points[0].1) >= far);
                // Nothing was drawn (a click, or a twitch): no item, and nothing to undo.
                let empty = match drag.mode {
                    DragMode::Draw => w < 4. || h < 4.,
                    DragMode::Line => !reaches(4.),
                    // The pen leaves small marks too (the dot on an i): anything that shows on screen.
                    _ => !reaches(2. / self.zoom),
                };
                if empty {
                    self.items.retain(|i| i.id != drag.id);
                } else {
                    self.push_history(drag.snapshot);
                }
                // Left unselected: the tool is still in hand, for the next one.
                self.selected = None;
            } else if drag.mode == DragMode::Draw {
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

        // So does the question of which pages to add: numbers and ranges only.
        if let Some(prompt) = &mut self.import_prompt {
            match ks.key.as_str() {
                "escape" => self.cancel_import(cx),
                "enter" => self.submit_import(window, cx),
                "backspace" => {
                    if std::mem::take(&mut prompt.untouched) {
                        prompt.text.clear();
                    } else {
                        prompt.text.pop();
                    }
                }
                _ if !shortcut => {
                    let typed = ks.key_char.clone().filter(|c| c.chars().all(|c| c.is_ascii_digit() || " ,-–".contains(c)));
                    let Some(typed) = typed else { return };
                    if std::mem::take(&mut prompt.untouched) {
                        prompt.text.clear();
                    }
                    prompt.text.push_str(&typed);
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

        // The find bar: Return goes to the next occurrence (with Shift, the one before).
        if self.find_focused() {
            match ks.key.as_str() {
                "escape" => self.close_find(cx),
                "enter" => self.find_step(!ks.modifiers.shift, window, cx),
                // Characters arrive through the input handler.
                _ => return,
            }
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
                // Put the tool down; with none in hand, close the find bar.
                if self.tool == Tool::Select {
                    self.find = None;
                }
                self.tool = Tool::Select;
                self.text_selection = None;
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

    fn render_titlebar(&self, client_decorated: bool, maximized: bool, window_h: Pixels, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let has_doc = self.doc.is_some();
        let (left_shown, right_shown) = (self.settings.show_left_sidebar, self.settings.show_right_sidebar);
        let (title, subtitle) = match &self.doc {
            Some(doc) => {
                let n = doc.page_count();
                let pages = format!("{n} page{}", if n == 1 { "" } else { "s" });
                (file_name(&doc.path), if self.dirty { format!("{pages} · Edited") } else { pages })
            }
            None => ("PatraPDF".to_string(), "No document".to_string()),
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
                    .child(
                        ui::icon_button("toggle-left-sidebar", "sidebar-left")
                            .tooltip(ui::tooltip(if left_shown { "Hide left sidebar" } else { "Show left sidebar" }, Some("⌘B")))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_left_sidebar(cx))),
                    )
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
                            ui::icon_button("copy-pdf", "clipboard")
                                .tooltip(ui::tooltip("Copy the finished PDF to the clipboard, to paste into an email, a chat or a folder", Some("⇧⌘C")))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.save(SaveTo::Clipboard, window, cx))),
                        )
                        .child(div().w(px(4.)))
                        .child(
                            ui::primary_button("save", "save", "Save PDF")
                                .tooltip(ui::tooltip("Save a filled copy", Some("⌘S")))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.save(SaveTo::File, window, cx))),
                        )
                        .child(div().w(px(4.)))
                    })
                    // No document: the start screen (gallery or empty state) has its own open action.
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .child(
                                ui::icon_button("help", "help")
                                    .when(self.help_menu_open, |b| b.bg(ui::hover_bg()))
                                    .when(!self.help_menu_open, |b| b.tooltip(ui::tooltip("Help and shortcuts", Some("⌘/"))))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        if !std::mem::take(&mut this.menu_dismissed) {
                                            this.toggle_help_menu(cx);
                                        }
                                    })),
                            )
                            .when(self.help_menu_open, |d| d.child(self.render_help_menu(window_h, cx))),
                    )
                    .when(has_doc, |d| {
                        d.child(
                            ui::icon_button("toggle-right-sidebar", "sidebar-right")
                                .tooltip(ui::tooltip(if right_shown { "Hide right sidebar" } else { "Show right sidebar" }, Some("⌥⌘B")))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_right_sidebar(cx))),
                        )
                    }),
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
                        .child(control("win-close", "close").on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            if this.may_close(window, cx) {
                                window.remove_window();
                            }
                        }))),
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

    /// The Help menu: every keyboard shortcut, then the website and the About box.
    fn render_help_menu(&self, window_h: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let mut shortcuts = div().id("help-shortcuts").flex().flex_col().pb_1().overflow_y_scroll();
        for (group, rows) in SHORTCUTS {
            shortcuts = shortcuts.child(ui::menu_caption(group));
            for (label, keys) in *rows {
                shortcuts = shortcuts.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .h(px(24.))
                        .px_2()
                        .text_xs()
                        .text_color(ui::text_secondary())
                        .child(*label)
                        .child(ui::kbd(keys)),
                );
            }
        }
        let panel = ui::menu_panel()
            .w(px(290.))
            .child(div().px_2().pt_1p5().text_sm().font_weight(FontWeight::SEMIBOLD).child("Keyboard shortcuts"))
            // Scrolls in a short window: the space under the title bar, less the rows around the list.
            .child(shortcuts.max_h((window_h - px(190.)).max(px(120.))))
            .child(ui::menu_separator())
            .child(ui::menu_action("help-website", "link", "Visit website").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.close_menus();
                cx.open_url(crate::about::REPOSITORY);
                cx.notify();
            })))
            .child(ui::menu_action("help-about", "info", "About PatraPDF").on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| {
                    this.close_menus();
                    this.show_about(window, cx);
                    cx.notify();
                },
            )));
        self.floating_menu(panel, true, cx)
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
        self.unfocus_find();
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

    /// "Annotate": picking out text, the highlighter, the pen, and a shape.
    fn render_annotate_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = [Tool::SelectText, Tool::Highlight, Tool::Pen, self.shape_tool].map(|tool| self.render_tool_row(tool, cx));
        Self::sidebar_section("Annotate", rows.into())
    }

    fn render_shape_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = ui::menu_panel().child(div().px_2().pt_1().pb_1p5().text_xs().text_color(ui::text_tertiary()).child("Shape"));
        for (i, tool) in [Tool::Rectangle, Tool::Oval, Tool::Line, Tool::Arrow].into_iter().enumerate() {
            panel = panel.child(ui::menu_item(("shape", i), tool_label(tool).1, self.shape_tool == tool, None).on_click(cx.listener(
                move |this, _: &ClickEvent, window, cx| {
                    this.finish_editing(cx);
                    this.close_menus();
                    (this.shape_tool, this.tool) = (tool, tool);
                    window.focus(&this.focus);
                    cx.notify();
                },
            )));
        }
        self.floating_menu(panel, false, cx)
    }

    /// The colours (and, for the pen, the line widths) to choose from, as
    /// buttons on a dark bar: for the tool in hand, or for the selected item.
    fn render_pen_options(&self, key: usize, highlight: bool, color: u32, width: Option<f32>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let button = |id: (&'static str, usize)| {
            div()
                .id(id)
                .size(px(26.))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor(CursorStyle::PointingHand)
                .hover(|s| s.bg(rgba(0xffffff1f)))
        };
        let colors = if highlight { model::HIGHLIGHT_COLORS } else { model::PEN_COLORS };
        let mut options: Vec<AnyElement> = colors
            .iter()
            .enumerate()
            .map(|(i, &(label, choice))| {
                button(("pen-color", key * 16 + i))
                    .child(
                        div()
                            .size(px(14.))
                            .rounded_full()
                            .bg(rgb(choice))
                            .border_1()
                            .border_color(rgba(0xffffff8c))
                            .when(choice == color, |d| d.border_2().border_color(gpui::white())),
                    )
                    .tooltip(ui::tooltip(label, None))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_pen_color(highlight, choice, cx)))
                    .into_any_element()
            })
            .collect();
        if let Some(width) = width {
            options.push(div().w(px(1.)).h(px(16.)).mx_0p5().bg(rgba(0xffffff26)).into_any_element());
            options.extend(model::PEN_WIDTHS.iter().enumerate().map(|(i, &(label, choice))| {
                button(("pen-width", key * 16 + i))
                    .when(choice == width, |d| d.bg(rgba(0xffffff33)))
                    .child(div().size(px(4. + 3. * i as f32)).rounded_full().bg(gpui::white()))
                    .tooltip(ui::tooltip(label, None))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_pen_width(choice, cx)))
                    .into_any_element()
            }));
        }
        options
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
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.save_with_new_password(SaveTo::File, window, cx)))
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
                Tool::Highlight => "Drag over text".into(),
                Tool::Pen => "Write or draw freehand".into(),
                Tool::Rectangle | Tool::Oval | Tool::Line | Tool::Arrow => "Drag to draw one".into(),
                Tool::SelectText => "Drag over text to copy it".into(),
                Tool::Check | Tool::Select => "Tick a box".into(),
            };
            let id = match tool {
                Tool::Text => "fill-Text",
                Tool::Date => "fill-Date",
                Tool::Check => "fill-Checkmark",
                Tool::Redact => "protect-Redact",
                Tool::Select => "fill-Select",
                Tool::Highlight => "annotate-Highlight",
                Tool::Pen => "annotate-Draw",
                Tool::Rectangle | Tool::Oval | Tool::Line | Tool::Arrow => "annotate-Shape",
                Tool::SelectText => "annotate-SelectText",
            };
            let how = if tool.stays() {
                "Click, then drag on the page. It stays in hand until you press Esc".to_string()
            } else {
                format!("Click, then click the page — or drag {} onto the page", label.to_lowercase())
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
                .when(tool.is_shape(), |d| {
                    d.child(
                        div()
                            .id("shape-trigger")
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
                            .tooltip(ui::tooltip("Which shape to draw", None))
                            .child("Shape")
                            .child(ui::icon("chevron-down", ui::text_tertiary()).size(px(11.)))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                if std::mem::take(&mut this.menu_dismissed) {
                                    return;
                                }
                                let open = !this.shape_menu_open;
                                this.close_menus();
                                this.shape_menu_open = open;
                                cx.notify();
                            }))
                            .when(self.shape_menu_open, |d| d.child(self.render_shape_menu(cx))),
                    )
                })
                .when(has_doc, |d| {
                    d.cursor(CursorStyle::PointingHand)
                        .tooltip(ui::tooltip(how, None))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.take_tool(tool, cx);
                            window.focus(&this.focus);
                        }))
                        .when(!tool.stays(), |d| d.on_drag(ToolDrag(tool), |drag: &ToolDrag, _, _, cx| cx.new(|_| *drag)))
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
            .child(self.render_annotate_section(cx))
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
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                    div()
                        .id("sig-list")
                        .flex()
                        .flex_col()
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.signatures_scroll)
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
                        })
                    )
                    .child(scrollbar("signatures", Axis::Vertical, false, &self.signatures_scroll, &self.scrollbar_held, cx.entity_id())),
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
            // On screen the colour is see-through; in the file it darkens what's under it.
            ItemKind::Highlight { color } => {
                div().w(px(item.w * z)).h(px(item.h * z)).rounded(px(1.)).bg(rgba((color << 8) | 0x66)).into_any_element()
            }
            ItemKind::Outline { oval, color, width } => {
                let (oval, color, line) = (*oval, *color, width * z);
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        if let Some(path) = outline_path(bounds, oval, line) {
                            window.paint_path(path, rgb(color));
                        }
                    },
                )
                .w(px(item.w * z))
                .h(px(item.h * z))
                .into_any_element()
            }
            ItemKind::Stroke { points, color, width, arrow } => {
                let (points, color, line, arrow) = (points.clone(), *color, width * z, *arrow);
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        if let Some(path) = stroke_path(bounds, &points, line, arrow.then_some(z)) {
                            window.paint_path(path, rgb(color));
                        }
                    },
                )
                .w(px(item.w * z))
                .h(px(item.h * z))
                .into_any_element()
            }
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
            .when_some(item.color(), |h, (highlight, color)| {
                h.children(self.render_pen_options(id as usize + 1, highlight, color, item.pen_width(), cx))
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
            .cursor(if editing {
                CursorStyle::IBeam
            } else if self.tool.draws() {
                CursorStyle::Crosshair
            } else if self.tool == Tool::SelectText {
                CursorStyle::IBeam
            } else {
                CursorStyle::OpenHand
            })
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
                let output_exists = entry.saved_copy().is_some_and(|saved| self.file_exists(saved));
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
            .relative()
            .flex_1()
            .h_full()
            .child(
                div()
                    .id("gallery")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.gallery_scroll)
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
            )
            .child(scrollbar("gallery", Axis::Vertical, false, &self.gallery_scroll, &self.scrollbar_held, cx.entity_id()))
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

        let list = div()
            .id("thumb-rail")
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .overflow_y_scroll()
            .track_scroll(&self.thumbs_scroll)
            .gap(px(THUMB_GAP))
            .pt_3()
            .pb_6()
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
            .children(thumbs);
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(THUMB_RAIL_W))
            .h_full()
            .bg(ui::sidebar_bg())
            .border_l_1()
            .border_color(ui::border())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .pl_4()
                    .pr_2()
                    .pt_3()
                    .pb_1()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Pages"))
                    .child(
                        ui::icon_button("add-pages", "plus")
                            .tooltip(ui::tooltip("Add pages from another PDF", None))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.add_pages(window, cx))),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(list)
                    .child(scrollbar("thumbs", Axis::Vertical, false, &self.thumbs_scroll, &self.scrollbar_held, cx.entity_id())),
            )
            .into_any_element()
    }

    /// The modal question of which pages of another PDF to add.
    fn render_import_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.import_prompt.as_ref()?;
        let count = prompt.source.pages.len();
        let chosen = parse_page_ranges(&prompt.text, count);
        let after = self.doc.as_ref().map_or(0, |d| d.page_count());
        let note = match &chosen {
            Ok(pages) => format!(
                "{} of its {count} pages will be added after page {after}.",
                if pages.len() == count { "All".to_string() } else { pages.len().to_string() }
            ),
            Err(problem) => problem.clone(),
        };
        let wrong = chosen.is_err() && !prompt.text.trim().is_empty();
        let field = ui::text_field(if wrong { ui::danger() } else { ui::accent() })
            .shadow(ui::ring())
            .when(prompt.text.is_empty(), |d| d.child(div().text_color(ui::text_tertiary()).child("1-3, 5")))
            // The suggestion looks selected: typing replaces it.
            .when(!prompt.text.is_empty(), |d| {
                d.child(div().rounded(px(2.)).when(prompt.untouched, |d| d.bg(ui::accent().opacity(0.25))).child(prompt.text.clone()))
            })
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
                    .child(ui::icon("file", ui::accent()).size(px(18.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("Add pages from “{}”", file_name(&prompt.path))),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(ui::text_secondary())
                    .child("Which pages? Type page numbers and ranges, like 1-3, 5."),
            )
            .child(field)
            .child(div().text_xs().text_color(if wrong { ui::danger() } else { ui::text_tertiary() }).child(note))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        ui::secondary_button("import-cancel", "close", "Cancel")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_import(cx))),
                    )
                    .child(
                        ui::primary_button("import-add", "plus", "Add Pages")
                            .when(chosen.is_err(), |d| d.opacity(0.5))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.submit_import(window, cx))),
                    ),
            );
        Some(
            div()
                .id("import-prompt")
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
            PasswordPurpose::Import => (
                format!("“{name}” is password protected"),
                "Enter its password to add pages from it.",
                "Password",
                "That password isn't right. Try again.",
                "Continue",
                "open",
            ),
            PasswordPurpose::NewPassword { first: None, .. } => (
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
            PasswordPurpose::NewPassword { first: Some(_), .. } => (
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
        let cursor = match self.tool {
            Tool::Select => hand,
            Tool::SelectText => CursorStyle::IBeam,
            _ => CursorStyle::Crosshair,
        };
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
            let mut found = self.render_find_marks(source);
            found.extend(self.render_text_selection(source));
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
                // Until the page is drawn, its thumbnail stands in for it.
                .when_some(view.image.clone().or_else(|| view.thumb.clone()), |d, image| {
                    d.child(img(image).absolute().size_full().object_fit(ObjectFit::Fill))
                })
                .when(view.image.is_none() && view.thumb.is_none(), |d| {
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
                // What was looked for, the document's own fields, then the items placed on top.
                .children(found)
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
            Tool::Highlight => Some(("highlight", "Drag over text to highlight it")),
            Tool::Pen => Some(("pen", "Draw on the page")),
            Tool::Rectangle | Tool::Oval | Tool::Line | Tool::Arrow => Some((tool_label(self.tool).0, "Drag on the page to draw")),
            Tool::SelectText => Some(("select-text", "Drag over text to select it, or double-click a word")),
        };
        // A drawing tool's colours (and line widths) are chosen here, and it is put down with Esc.
        let (options, done) = if self.tool.draws() {
            let highlight = self.tool == Tool::Highlight;
            let color = if highlight { self.settings.highlight_color } else { self.settings.pen_color };
            (self.render_pen_options(0, highlight, color, (!highlight).then_some(self.settings.pen_width), cx), "Esc when done")
        } else if self.tool == Tool::SelectText {
            // Text picked out can be copied from here, too.
            let copy = self.selected_text().map(|_| {
                div()
                    .id("copy-text")
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .bg(ui::accent())
                    .cursor(CursorStyle::PointingHand)
                    .hover(|s| s.opacity(0.85))
                    .tooltip(ui::tooltip("Copy the selected text", Some("⌘C")))
                    .child("Copy")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy(cx)))
                    .into_any_element()
            });
            (copy.into_iter().collect(), "Esc when done")
        } else {
            (Vec::new(), "Esc to cancel")
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
            .child(scrollbar("pages", Axis::Vertical, true, &self.pages_scroll, &self.scrollbar_held, cx.entity_id()))
            .child(scrollbar("pages", Axis::Horizontal, true, &self.pages_scroll, &self.scrollbar_held, cx.entity_id()))
            .children(self.render_find_bar(cx))
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
                            .cursor(CursorStyle::Arrow)
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(ui::icon(icon_name, gpui::white()).size(px(14.)))
                            .child(text)
                            .children(options)
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded_full()
                                    .bg(rgba(0xffffff1f))
                                    .text_color(ui::hud_muted())
                                    .child(done),
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
        // A long document's drawn pages follow the view, and the pages as they come and go.
        if let (Some(drawn), Some(doc)) = (&self.drawn_around, &self.doc) {
            if doc.order != drawn.order || self.current_page().is_some_and(|page| page.abs_diff(drawn.page) >= drawn.slack) {
                self.ensure_rendered(window, cx);
            }
        }
        self.follow_in_rail();
        self.ensure_thumbnails(window, cx);
        // A sidebar came or went: this frame lays the page view out at its new
        // size, and the next one can fit the zoom to it.
        if std::mem::take(&mut self.refit) {
            cx.on_next_frame(window, |_, _, cx| cx.notify());
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
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(SaveTo::File, window, cx)))
            .on_action(cx.listener(|this, _: &SaveWithPassword, window, cx| this.save_with_new_password(SaveTo::File, window, cx)))
            .on_action(cx.listener(|this, _: &CopyPdf, window, cx| this.save(SaveTo::Clipboard, window, cx)))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.undo(cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.redo(cx)))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &Duplicate, _, cx| this.duplicate(cx)))
            .on_action(cx.listener(|this, _: &Find, window, cx| this.start_find(window, cx)))
            .on_action(cx.listener(|this, _: &FindNext, window, cx| this.find_step(true, window, cx)))
            .on_action(cx.listener(|this, _: &FindPrevious, window, cx| this.find_step(false, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomFitWidth, window, cx| this.set_zoom_mode(ZoomMode::FitWidth, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomFitPage, window, cx| this.set_zoom_mode(ZoomMode::FitPage, window, cx)))
            .on_action(cx.listener(|this, _: &AddSignature, window, cx| this.add_signature_dialog(window, cx)))
            .on_action(cx.listener(|this, _: &AddPages, window, cx| this.add_pages(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| this.set_zoom(this.zoom * 1.2, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| this.set_zoom(this.zoom / 1.2, window, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, window, cx| this.set_zoom(1.0, window, cx)))
            .on_action(cx.listener(|this, _: &CloseDocument, window, cx| this.request_close(window, cx)))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _| window.minimize_window()))
            .on_action(cx.listener(|_, _: &ZoomWindow, window, _| window.zoom_window()))
            .on_action(cx.listener(|this, _: &DiscardChanges, window, cx| this.request_discard(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleLeftSidebar, _, cx| this.toggle_left_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleRightSidebar, _, cx| this.toggle_right_sidebar(cx)))
            .on_action(cx.listener(|this, _: &KeyboardShortcuts, _, cx| this.toggle_help_menu(cx)))
            .on_action(cx.listener(|_, _: &VisitWebsite, _, cx| cx.open_url(crate::about::REPOSITORY)))
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
            .child(self.render_titlebar(client_decorated, window.is_maximized(), window.viewport_size().height, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .when(self.settings.show_left_sidebar, |d| d.child(self.render_sidebar(cx)))
                    .child(self.render_pages(cx))
                    .when(self.doc.is_some() && self.settings.show_right_sidebar, |d| d.child(self.render_thumbnail_rail(cx))),
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
                                .child(format!("PatraPDF {}", crate::about::VERSION))
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
            .when_some(self.render_import_prompt(cx), |d, prompt| d.child(prompt))
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

/// "1 page", "3 pages".
fn count(n: usize, of: &str) -> String {
    format!("{n} {of}{}", if n == 1 { "" } else { "s" })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The outline of a box, or of the oval that fills it, just inside `bounds` and `line` wide.
fn outline_path(bounds: Bounds<Pixels>, oval: bool, line: f32) -> Option<gpui::Path<Pixels>> {
    let inset = px(line / 2.).min(bounds.size.width / 2.).min(bounds.size.height / 2.);
    let (left, top, right, bottom) = (bounds.left() + inset, bounds.top() + inset, bounds.right() - inset, bounds.bottom() - inset);
    let mut builder = gpui::PathBuilder::stroke(px(line));
    if oval {
        let (rx, ry) = ((right - left) / 2., (bottom - top) / 2.);
        for i in 0..64 {
            let angle = i as f32 / 64. * std::f32::consts::TAU;
            let at = point(left + rx + rx * angle.cos(), top + ry + ry * angle.sin());
            if i == 0 { builder.move_to(at) } else { builder.line_to(at) }
        }
    } else {
        builder.move_to(point(left, top));
        for corner in [point(right, top), point(right, bottom), point(left, bottom)] {
            builder.line_to(corner);
        }
    }
    builder.close();
    builder.build().ok()
}

/// A line `line` wide through `points` (fractions of `bounds`), with an arrow
/// head at its end if `arrow` (which is how long a point is on screen).
fn stroke_path(bounds: Bounds<Pixels>, points: &[[f32; 2]], line: f32, arrow: Option<f32>) -> Option<gpui::Path<Pixels>> {
    let at = |[x, y]: [f32; 2]| bounds.origin + point(bounds.size.width * x, bounds.size.height * y);
    let [.., from, to] = points else { return None };
    let mut builder = gpui::PathBuilder::stroke(px(line));
    for (i, &fraction) in points.iter().enumerate() {
        if i == 0 { builder.move_to(at(fraction)) } else { builder.line_to(at(fraction)) }
    }
    if let Some(unit) = arrow {
        let (from, to) = (at(*from), at(*to));
        let xy = |p: Point<Pixels>| (p.x / px(1.), p.y / px(1.));
        let [left, right] = model::arrow_barbs(xy(from), xy(to), line, unit).map(|(x, y)| point(px(x), px(y)));
        builder.move_to(left);
        builder.line_to(to);
        builder.line_to(right);
    }
    builder.build().ok()
}

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("secondary-n", NewWindow, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-m", MinimizeWindow, None),
        KeyBinding::new("secondary-o", Open, None),
        KeyBinding::new("secondary-s", Save, None),
        KeyBinding::new("secondary-alt-s", SaveWithPassword, None),
        KeyBinding::new("secondary-shift-c", CopyPdf, None),
        KeyBinding::new("secondary-z", Undo, None),
        KeyBinding::new("secondary-shift-z", Redo, None),
        KeyBinding::new("secondary-c", Copy, None),
        KeyBinding::new("secondary-x", Cut, None),
        KeyBinding::new("secondary-v", Paste, None),
        KeyBinding::new("secondary-d", Duplicate, None),
        KeyBinding::new("secondary-f", Find, None),
        KeyBinding::new("secondary-g", FindNext, None),
        KeyBinding::new("secondary-shift-g", FindPrevious, None),
        KeyBinding::new("secondary-1", ZoomFitWidth, None),
        KeyBinding::new("secondary-2", ZoomFitPage, None),
        KeyBinding::new("secondary-=", ZoomIn, None),
        KeyBinding::new("secondary-+", ZoomIn, None),
        KeyBinding::new("secondary--", ZoomOut, None),
        KeyBinding::new("secondary-0", ZoomReset, None),
        KeyBinding::new("secondary-shift-s", AddSignature, None),
        KeyBinding::new("secondary-b", ToggleLeftSidebar, None),
        KeyBinding::new("secondary-alt-b", ToggleRightSidebar, None),
        KeyBinding::new("secondary-/", KeyboardShortcuts, None),
        KeyBinding::new("secondary-w", CloseDocument, None),
        KeyBinding::new("secondary-q", Quit, None),
    ]);
}

/// Open a PDF in a window: the one that already shows it, else one with nothing in
/// it (`preferred`, or the one in front, before any other), else a new one.
///
/// It happens once whatever is running has finished, so it can be asked for from
/// inside a window.
pub fn open_document(cx: &mut App, path: PathBuf, preferred: Option<AnyWindowHandle>) {
    cx.defer(move |cx| {
        let first = preferred.or_else(|| cx.active_window());
        let mut windows = editor_windows(cx);
        windows.sort_by_key(|(window, _)| Some(*window) != first);
        let target = windows
            .iter()
            .find(|(_, editor)| editor.read(cx).shows(&path))
            .or_else(|| windows.iter().find(|(_, editor)| editor.read(cx).is_free()))
            .cloned();
        let opened = match target {
            Some((window, editor)) => window.update(cx, |_, window, cx| {
                window.activate_window();
                editor.update(cx, |editor, cx| {
                    if !editor.shows(&path) {
                        editor.open_path(path, window, cx);
                    }
                });
            }),
            None => open_editor(cx, Some(path)).map(|_| ()),
        };
        if let Err(e) = opened {
            eprintln!("failed to open a window: {e:#}");
        }
    });
}

/// Quit, asking first if a window has unsaved changes.
pub fn request_quit(cx: &mut App) {
    cx.defer(|cx| {
        let unsaved: Vec<_> = editor_windows(cx).into_iter().filter(|(_, editor)| editor.read(cx).dirty).collect();
        let Some((window, _)) = unsaved.first() else { return cx.quit() };
        let detail = match unsaved.len() {
            1 => "Fields you added or changed since the last save will be lost.".to_string(),
            n => format!("{n} documents have changes since they were last saved, which will be lost."),
        };
        let answer = window.update(cx, |_, window, cx| {
            window.activate_window();
            window.prompt(PromptLevel::Warning, "Quit and discard unsaved changes?", Some(&detail), &["Quit", "Cancel"], cx)
        });
        let Ok(answer) = answer else { return cx.quit() };
        cx.spawn(async move |cx| {
            if answer.await.ok() == Some(0) {
                cx.update(|cx| cx.quit()).ok();
            }
        })
        .detach();
    });
}

/// Where a new window goes: a step down and to the right of the one in front, so
/// that neither hides the other, or the middle of the screen for the first.
fn new_window_bounds(cx: &mut App) -> Bounds<Pixels> {
    const STEP: Pixels = px(28.);
    // At launch no window is in front yet: the newest one stands in.
    let front = cx.active_window().or_else(|| editor_windows(cx).last().map(|(window, _)| *window)).and_then(|window| {
        window
            .update(cx, |_, window, cx| {
                let windowed = !window.is_fullscreen() && !window.is_maximized();
                windowed.then(|| (window.bounds(), window.display(cx).map(|display| display.bounds())))
            })
            .ok()
            .flatten()
    });
    let Some((front, display)) = front else {
        return Bounds::centered(None, size(px(1240.), px(900.)), cx);
    };
    let taken: Vec<Point<Pixels>> =
        cx.windows().into_iter().filter_map(|window| window.update(cx, |_, window, _| window.bounds().origin).ok()).collect();
    let is_taken = |origin: Point<Pixels>| taken.iter().any(|t| (t.x - origin.x).abs() < px(4.) && (t.y - origin.y).abs() < px(4.));
    let mut origin = front.origin + point(STEP, STEP);
    // Off the bottom or the right of the screen: start again from its top left.
    if let Some(display) = display.filter(|d| origin.x + front.size.width > d.right() || origin.y + front.size.height > d.bottom()) {
        origin = display.origin + point(STEP, STEP * 2.);
    }
    for _ in 0..20 {
        if !is_taken(origin) {
            break;
        }
        origin += point(STEP, STEP);
    }
    Bounds { origin, size: front.size }
}

pub fn open_editor(cx: &mut App, initial: Option<PathBuf>) -> anyhow::Result<gpui::WindowHandle<Editor>> {
    let bounds = new_window_bounds(cx);
    let window = cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("PatraPDF".into()),
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
        let dir = std::env::temp_dir().join(format!("patrapdf_ui_test_{}", std::process::id()));
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
    fn the_finished_pdf_can_go_to_the_clipboard(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        let (original, name) = editor.read_with(cx, |e, _| {
            let path = e.doc.as_ref().unwrap().path.clone();
            let name = format!("{}-signed.pdf", path.file_stem().unwrap().to_str().unwrap());
            (path, name)
        });
        editor.update(cx, |e, _| e.tool = Tool::Check);
        cx.simulate_click(origin + point(px(300.), px(400.)), Modifiers::none());
        editor.read_with(cx, |e, _| assert!(e.dirty));

        // Copied without asking where to save: the copy is a file of the app's own.
        cx.simulate_keystrokes("secondary-shift-c");
        cx.run_until_parked();
        assert!(!cx.did_prompt_for_new_path());
        let copy = clipboard::last_put().expect("a copy on the clipboard");
        assert_eq!(copy.file_name().unwrap().to_str(), Some(name.as_str()));
        assert!(!copy.starts_with(original.parent().unwrap()), "not beside the original");
        let written = crate::pdf::load(std::fs::read(&copy).unwrap()).unwrap();
        assert_eq!(written.pages.len(), 2);
        let page = crate::pdf::render_page(&written.pdf, 0, 1.0).unwrap();
        let dark = |x: u32, y: u32| page.bgra[((y * page.width + x) * 4) as usize] < 100;
        assert!((290..310).any(|x| (390..410).any(|y| dark(x, y))), "with the checkmark on it");

        // It counts as saved, and Recents can bring it back to work on; but there is no saved file.
        editor.read_with(cx, |e, _| {
            assert!(!e.dirty);
            assert!(e.status.contains(&format!("Copied {name} to the clipboard")), "{}", e.status);
            assert_eq!(e.doc.as_ref().unwrap().last_output, None);
            assert_eq!((e.recents.len(), e.recents[0].items.len()), (1, 1));
            assert_eq!(e.recents[0].saved_copy(), None);
        });
        let id = editor.read_with(cx, |e, _| e.recents[0].id.clone());
        editor.update_in(cx, |e, window, cx| {
            e.close_document(window, cx);
            e.open_recent(&id, window, cx);
        });
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 1);
            assert_eq!(e.doc.as_ref().unwrap().last_output, None, "the next save is not offered the original's name");
        });

        // After a save to a file, a copy to the clipboard leaves that file as the saved one.
        let out = test_dir().join("clipboard-then-file.pdf");
        editor.update_in(cx, |e, window, cx| e.save(SaveTo::File, window, cx));
        cx.run_until_parked();
        cx.simulate_new_path_selection({
            let out = out.clone();
            move |_| Some(out)
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("secondary-shift-c");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert_eq!(e.recents[0].saved_copy(), Some(out.as_path()));
            assert_eq!(e.doc.as_ref().unwrap().last_output.as_deref(), Some(out.as_path()));
        });
        // The copy is named like the saved file, and takes the place of the one before.
        let second = clipboard::last_put().unwrap();
        assert_eq!(second.file_name(), out.file_name());
        assert!(second.exists() && !copy.exists());

        // With nothing open there is nothing to copy.
        editor.update_in(cx, |e, window, cx| e.close_document(window, cx));
        cx.simulate_keystrokes("secondary-shift-c");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert!(e.status.contains("Nothing to save"), "{}", e.status));
        assert_eq!(clipboard::last_put(), Some(second));
    }

    #[gpui::test]
    fn text_is_selected_by_dragging_and_copied(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        let at = |x: f32, y: f32| origin + point(px(x), px(y));
        let selected = |cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.selected_text());
        let copied = |cx: &mut VisualTestContext| cx.read_from_clipboard().and_then(|c| c.text());

        // The tool reads the pages' text, as finding does.
        editor.update(cx, |e, cx| e.take_tool(Tool::SelectText, cx));
        cx.run_until_parked();
        // The first page says "Page 1 - Name:"; this is where "Name:" is.
        let [x, y, w, h] = editor.read_with(cx, |e, _| {
            let text = e.doc.as_ref().unwrap().text[0].as_ref().expect("the page's text was read");
            assert_eq!(text.text(0..usize::MAX), "Page 1 - Name:");
            text.find(&search::fold("name:"))[0][0]
        });

        // Dragged over, from its start to past the end of the line.
        cx.simulate_mouse_down(at(x + 1., y + h / 2.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(x + w / 2., y + h / 2.), MouseButton::Left, Modifiers::none());
        assert_eq!(selected(cx).map(|text| "Name:".starts_with(&text)), Some(true), "as far as the pointer has got");
        cx.simulate_mouse_move(at(x + w + 80., y + h + 30.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(x + w + 80., y + h + 30.), MouseButton::Left, Modifiers::none());
        assert_eq!(selected(cx).as_deref(), Some("Name:"));
        editor.read_with(cx, |e, _| {
            assert_eq!(e.tool, Tool::SelectText, "still in hand");
            assert!(e.status.contains("5 characters selected"), "{}", e.status);
            assert!(e.items.is_empty() && !e.dirty, "the document is as it was");
        });
        // ⌘C copies it, as the page has it.
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(copied(cx).as_deref(), Some("Name:"));

        // Backwards is the same, and the whole line is taken from beyond its ends.
        cx.simulate_mouse_down(at(x + w + 80., y + h / 2.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(2., y + h / 2.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(2., y + h / 2.), MouseButton::Left, Modifiers::none());
        assert_eq!(selected(cx).as_deref(), Some("Page 1 - Name:"));

        // A double click picks out a word.
        let press = |position, click_count| MouseDownEvent { button: MouseButton::Left, position, modifiers: Modifiers::none(), click_count, first_mouse: false };
        let release = |position, click_count| MouseUpEvent { button: MouseButton::Left, position, modifiers: Modifiers::none(), click_count };
        for click_count in [1, 2] {
            cx.simulate_event(press(at(x + w / 2., y + h / 2.), click_count));
            cx.simulate_event(release(at(x + w / 2., y + h / 2.), click_count));
        }
        assert_eq!(selected(cx).as_deref(), Some("Name:"));
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(copied(cx).as_deref(), Some("Name:"));

        // A click that picks nothing out clears what was.
        cx.simulate_click(at(x + 1., y + h / 2.), Modifiers::none());
        assert_eq!(selected(cx), None);
        editor.read_with(cx, |e, _| assert_eq!(e.text_selection, None));

        // Text under a field is picked out through it.
        editor.update(cx, |e, cx| {
            e.add_item(0, x, y, w, h, ItemKind::Highlight { color: model::HIGHLIGHT_COLORS[0].1 }, cx);
            e.selected = None;
        });
        cx.simulate_mouse_down(at(x + 1., y + h / 2.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(x + w + 80., y + h / 2.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(x + w + 80., y + h / 2.), MouseButton::Left, Modifiers::none());
        assert_eq!(selected(cx).as_deref(), Some("Name:"));
        editor.read_with(cx, |e, _| assert_eq!((e.items.len(), e.selected), (1, None), "the field is left alone"));

        // Esc puts the tool down, and with it what was picked out.
        cx.simulate_keystrokes("escape");
        editor.read_with(cx, |e, _| assert_eq!((e.tool, e.text_selection), (Tool::Select, None)));
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(copied(cx).as_deref(), Some("Name:"), "nothing new is copied");
    }

    #[gpui::test]
    fn annotations_are_drawn_by_dragging(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let origin = page_origin(&editor, cx, 0);
        let at = |x: f32, y: f32| origin + point(px(x), px(y));
        let drag = |cx: &mut VisualTestContext, through: &[Point<Pixels>]| {
            cx.simulate_mouse_down(through[0], MouseButton::Left, Modifiers::none());
            for &position in &through[1..] {
                cx.simulate_mouse_move(position, MouseButton::Left, Modifiers::none());
            }
            cx.simulate_mouse_up(*through.last().unwrap(), MouseButton::Left, Modifiers::none());
        };
        let geometry = |editor: &Entity<Editor>, cx: &mut VisualTestContext, i: usize| {
            editor.read_with(cx, |e, _| (e.items[i].x, e.items[i].y, e.items[i].w, e.items[i].h))
        };

        // The highlighter marks where it is dragged, in the colour chosen for it.
        editor.update(cx, |e, cx| {
            e.tool = Tool::Highlight;
            e.set_pen_color(true, model::HIGHLIGHT_COLORS[2].1, cx);
        });
        drag(cx, &[at(60., 70.), at(260., 96.)]);
        editor.read_with(cx, |e, _| {
            assert!(matches!(e.items[0].kind, ItemKind::Highlight { color } if color == model::HIGHLIGHT_COLORS[2].1));
            assert_eq!(e.tool, Tool::Highlight, "still in hand, for the next");
            assert_eq!(e.selected, None);
            assert!(e.dirty);
        });
        assert_eq!(geometry(&editor, cx, 0), (60., 70., 200., 26.));
        drag(cx, &[at(60., 120.), at(260., 146.)]);
        editor.read_with(cx, |e, _| assert_eq!((e.items.len(), e.history.len()), (2, 2), "one undo step for each"));
        cx.simulate_keystrokes("secondary-z");

        // A click draws nothing, and leaves nothing to undo or save.
        cx.simulate_click(at(300., 300.), Modifiers::none());
        editor.read_with(cx, |e, _| assert_eq!((e.items.len(), e.history.len()), (1, 1)));
        cx.simulate_keystrokes("secondary-z");
        editor.update(cx, |e, _| e.dirty = false);
        cx.simulate_click(at(300., 300.), Modifiers::none());
        editor.read_with(cx, |e, _| assert!(e.items.is_empty() && e.history.is_empty() && !e.dirty));
        cx.simulate_keystrokes("secondary-shift-z");

        // The pen draws through wherever the pointer goes, over other items too.
        editor.update(cx, |e, cx| {
            e.tool = Tool::Pen;
            e.set_pen_color(false, model::PEN_COLORS[1].1, cx);
            e.set_pen_width(model::PEN_WIDTHS[2].1, cx);
        });
        drag(cx, &[at(100., 80.), at(150., 180.), at(200., 80.)]);
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 2, "a new item, though it starts on the highlight");
            assert_eq!(e.items[1].stroke_points(), vec![(100., 80.), (150., 180.), (200., 80.)]);
            let ItemKind::Stroke { color, width, arrow, .. } = &e.items[1].kind else { panic!("not a stroke") };
            assert_eq!((*color, *width, *arrow), (model::PEN_COLORS[1].1, model::PEN_WIDTHS[2].1, false));
        });
        assert_eq!(geometry(&editor, cx, 1), (100., 80., 100., 100.));

        // A line is straight from where it starts to where it ends, however the pointer got there;
        // a level one gets a box thick enough to take hold of.
        editor.update(cx, |e, _| e.tool = Tool::Arrow);
        drag(cx, &[at(300., 400.), at(350., 300.), at(500., 400.)]);
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items[2].stroke_points(), vec![(300., 400.), (500., 400.)]);
            assert!(matches!(e.items[2].kind, ItemKind::Stroke { arrow: true, .. }));
        });
        assert_eq!(geometry(&editor, cx, 2), (300., 396., 200., 8.));

        // Shapes are outlines of the box dragged out.
        editor.update(cx, |e, _| e.tool = Tool::Oval);
        drag(cx, &[at(400., 200.), at(300., 100.)]);
        editor.read_with(cx, |e, _| assert!(matches!(e.items[3].kind, ItemKind::Outline { oval: true, .. })));
        assert_eq!(geometry(&editor, cx, 3), (300., 100., 100., 100.));

        // A box dragged past the page's edge stops there, and reaches back to where it began.
        editor.update(cx, |e, _| e.tool = Tool::Rectangle);
        drag(cx, &[at(50., 600.), at(-30., 640.)]);
        assert_eq!(geometry(&editor, cx, 4), (0., 600., 50., 40.));
        // A line along the edge stays on it, though the box around it reaches over.
        editor.update(cx, |e, _| e.tool = Tool::Line);
        drag(cx, &[at(100., 2.), at(200., 2.)]);
        editor.read_with(cx, |e, _| assert_eq!(e.items[5].stroke_points(), vec![(100., 2.), (200., 2.)]));
        // The pen leaves small marks (a line that short is a slip); a release that isn't seen ends the stroke.
        drag(cx, &[at(400., 500.), at(403., 500.)]);
        editor.update(cx, |e, _| e.tool = Tool::Pen);
        drag(cx, &[at(400., 500.), at(403., 500.)]);
        editor.read_with(cx, |e, _| assert_eq!(e.items.len(), 7));
        cx.simulate_mouse_down(at(420., 520.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(440., 540.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(460., 560.), None, Modifiers::none());
        editor.read_with(cx, |e, _| {
            assert!(e.drag.is_none());
            assert_eq!(e.items[7].stroke_points(), vec![(420., 520.), (440., 540.)]);
            assert_eq!(e.history.len(), 8, "one step for each of the eight");
        });
        cx.simulate_keystrokes("secondary-z secondary-z secondary-z secondary-z");

        // Escape puts the tool down. Then items are selected, recoloured, resized and moved as usual.
        cx.simulate_keystrokes("escape");
        editor.read_with(cx, |e, _| assert_eq!(e.tool, Tool::Select));
        cx.simulate_click(at(350., 150.), Modifiers::none());
        let oval = editor.read_with(cx, |e, _| e.items[3].id);
        editor.read_with(cx, |e, _| assert_eq!(e.selected, Some(oval)));
        editor.update(cx, |e, cx| {
            e.set_pen_color(false, model::PEN_COLORS[3].1, cx);
            e.set_pen_width(model::PEN_WIDTHS[0].1, cx);
            // The highlighter's colour is not the oval's to take.
            e.set_pen_color(true, model::HIGHLIGHT_COLORS[1].1, cx);
        });
        editor.read_with(cx, |e, _| {
            assert!(matches!(e.items[3].kind, ItemKind::Outline { color, width, .. } if color == model::PEN_COLORS[3].1 && width == model::PEN_WIDTHS[0].1));
            assert!(matches!(e.items[1].kind, ItemKind::Stroke { color, .. } if color == model::PEN_COLORS[1].1), "others keep theirs");
        });
        cx.simulate_keystrokes("secondary-z secondary-z");
        editor.read_with(cx, |e, _| assert!(matches!(e.items[3].kind, ItemKind::Outline { color, width, .. } if color == model::PEN_COLORS[1].1 && width == model::PEN_WIDTHS[2].1)));
        let stroke = editor.read_with(cx, |e, _| e.items[1].id);
        editor.update_in(cx, |e, window, cx| {
            let down = MouseDownEvent { button: MouseButton::Left, position: at(200., 180.), modifiers: Modifiers::none(), click_count: 1, first_mouse: false };
            e.item_mouse_down(stroke, DragMode::Resize, &down, window, cx);
        });
        cx.simulate_mouse_move(at(300., 130.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(300., 130.), MouseButton::Left, Modifiers::none());
        assert_eq!(geometry(&editor, cx, 1), (100., 80., 200., 50.), "its width and height change separately");
        editor.read_with(cx, |e, _| assert_eq!(e.items[1].stroke_points(), vec![(100., 80.), (200., 130.), (300., 80.)], "and the line with them"));

        // The choices are kept for the next time.
        let saved = Settings::load();
        assert_eq!((saved.highlight_color, saved.pen_color, saved.pen_width), (model::HIGHLIGHT_COLORS[1].1, model::PEN_COLORS[3].1, model::PEN_WIDTHS[0].1));

        // What was drawn is saved into the file, and comes back from Recents.
        let out = test_dir().join("annotated.pdf");
        editor.update_in(cx, |e, window, cx| e.save(SaveTo::File, window, cx));
        cx.run_until_parked();
        cx.simulate_new_path_selection({
            let out = out.clone();
            move |_| Some(out)
        });
        cx.run_until_parked();
        let written = crate::pdf::load(std::fs::read(&out).unwrap()).unwrap();
        let page = crate::pdf::render_page(&written.pdf, 0, 1.0).unwrap();
        let blue = |x: u32, y: u32| {
            let i = ((y * page.width + x) * 4) as usize;
            page.bgra[i] > 180 && page.bgra[i + 2] < 80
        };
        assert!(blue(400, 400), "the arrow's line is on the saved page");
        let id = editor.read_with(cx, |e, _| e.recents[0].id.clone());
        editor.update_in(cx, |e, window, cx| {
            e.close_document(window, cx);
            e.open_recent(&id, window, cx);
        });
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert_eq!(e.items.len(), 4);
            assert_eq!(e.items[1].stroke_points(), vec![(100., 80.), (200., 130.), (300., 80.)]);
        });
    }

    #[gpui::test]
    fn text_is_found_in_the_document(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        // (which occurrence is shown, how many there are, the page position of each)
        let found = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            editor.read_with(cx, |e, _| {
                let hits = e.find_hits();
                (e.current_hit(&hits), hits.len(), hits.iter().map(|hit| hit.0).collect::<Vec<_>>())
            })
        };

        // Both pages say "Page N - Name:".
        cx.simulate_keystrokes("secondary-f");
        cx.simulate_input("NAME");
        assert_eq!(found(cx), (Some(0), 2, vec![0, 1]), "found whatever its case, and the first is shown");
        editor.read_with(cx, |e, _| assert!(!e.reading_text(), "every page has been read"));

        // Return and ⌘G step forward, Shift steps back, around the ends.
        cx.simulate_keystrokes("enter");
        assert_eq!(found(cx).0, Some(1));
        cx.simulate_keystrokes("enter");
        assert_eq!(found(cx).0, Some(0));
        cx.simulate_keystrokes("shift-enter");
        assert_eq!(found(cx).0, Some(1));
        cx.simulate_keystrokes("secondary-g");
        assert_eq!(found(cx).0, Some(0));
        cx.simulate_keystrokes("secondary-shift-g");
        assert_eq!(found(cx).0, Some(1));

        // The phrase is edited like any text, and looked for again as it changes.
        cx.simulate_keystrokes("backspace backspace backspace backspace");
        cx.simulate_input("page  2");
        assert_eq!(found(cx), (Some(0), 1, vec![1]));
        editor.read_with(cx, |e, _| assert_eq!(e.find.as_ref().unwrap().query, "page  2"));
        cx.simulate_input("x");
        assert_eq!(found(cx), (None, 0, vec![]));
        cx.simulate_keystrokes("backspace backspace backspace");
        assert_eq!(found(cx), (Some(1), 2, vec![0, 1]), "\"page\" is on both; the one on the page in view is shown");

        // Pages that are moved or deleted are looked through where they now are.
        editor.update(cx, |e, cx| e.move_page(1, 0, cx));
        cx.simulate_keystrokes("enter");
        let (current, count, _) = found(cx);
        assert_eq!((current.is_some(), count), (true, 2));
        editor.update(cx, |e, cx| e.delete_page(0, cx));
        assert_eq!(found(cx).1, 1);
        cx.simulate_keystrokes("secondary-z secondary-z");

        // A click on the page takes the keyboard back; the bar stays, and ⌘F returns to it.
        let origin = page_origin(&editor, cx, 0);
        cx.simulate_click(origin + point(px(300.), px(300.)), Modifiers::none());
        editor.read_with(cx, |e, _| assert!(e.find.is_some() && !e.find_focused()));
        cx.simulate_keystrokes("secondary-f");
        editor.read_with(cx, |e, _| assert!(e.find_focused()));

        // So does taking up an item: the keys are for it, not for the phrase.
        let query = |editor: &Entity<Editor>, cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.find.as_ref().unwrap().query.clone());
        let phrase = query(&editor, cx);
        let spot = origin + point(px(300.), px(500.));
        editor.update(cx, |e, _| e.tool = Tool::Check);
        cx.simulate_click(spot, Modifiers::none());
        cx.simulate_keystrokes("secondary-f");
        cx.simulate_click(spot, Modifiers::none());
        editor.read_with(cx, |e, _| assert!(e.selected.is_some() && e.find.is_some() && !e.find_focused()));
        cx.simulate_keystrokes("backspace");
        editor.read_with(cx, |e, _| assert!(e.items.is_empty(), "the checkmark is deleted"));
        assert_eq!(query(&editor, cx), phrase);

        cx.simulate_keystrokes("secondary-f");
        cx.simulate_keystrokes("escape");
        editor.read_with(cx, |e, _| assert!(e.find.is_none()));
        assert_eq!(found(cx).1, 0);
    }

    #[gpui::test]
    fn each_document_has_its_own_window(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let first = editor.read_with(cx, |e, _| e.doc.as_ref().unwrap().path.clone());
        let pdf = |name: &str| {
            let path = test_dir().join(name);
            std::fs::write(&path, crate::pdf::tests::sample_pdf()).unwrap();
            path
        };
        let (second, third) = (pdf("second.pdf"), pdf("third.pdf"));
        let others = |cx: &mut VisualTestContext| -> Vec<(AnyWindowHandle, Entity<Editor>)> {
            cx.update(|_, cx| editor_windows(cx)).into_iter().filter(|(_, e)| *e != editor).collect()
        };

        // A second document doesn't replace the first.
        editor.update_in(cx, |e, window, cx| e.request_open(Some(second.clone()), window, cx));
        cx.run_until_parked();
        let (other_window, other) = match others(cx).as_slice() {
            [one] => one.clone(),
            windows => panic!("expected one more window, found {}", windows.len()),
        };
        editor.read_with(cx, |e, _| assert_eq!(e.doc.as_ref().unwrap().path, first, "the first stays where it was"));
        other.read_with(cx, |e, _| assert_eq!(e.doc.as_ref().unwrap().path, second));

        // Asked for again, from either window, it isn't opened again.
        editor.update_in(cx, |e, window, cx| e.request_open(Some(second.clone()), window, cx));
        cx.update(|_, cx| open_document(cx, first.clone(), None));
        cx.run_until_parked();
        assert_eq!(others(cx).len(), 1);

        // What every window shows is changed in every window.
        editor.update(cx, |e, cx| e.set_date_format("%Y-%m-%d", cx));
        editor.update_in(cx, |e, window, cx| e.save(SaveTo::File, window, cx));
        cx.run_until_parked();
        cx.simulate_new_path_selection(|_| Some(test_dir().join("first-signed.pdf")));
        cx.run_until_parked();
        other.read_with(cx, |e, _| {
            assert_eq!(e.settings.date_format, "%Y-%m-%d");
            assert_eq!(e.recents.len(), 1, "the copy saved in the first window is in the second's Recents");
        });
        editor.update(cx, |e, cx| e.toggle_left_sidebar(cx));
        other.read_with(cx, |e, _| assert!(e.settings.show_left_sidebar, "but each window has its own sidebars"));

        // A window with nothing in it takes the next document...
        other_window.update(cx, |_, window, cx| other.update(cx, |e, cx| e.close_document(window, cx))).unwrap();
        editor.update_in(cx, |e, window, cx| e.request_open(Some(third.clone()), window, cx));
        cx.run_until_parked();
        assert_eq!(others(cx).len(), 1);
        other.read_with(cx, |e, _| assert_eq!(e.doc.as_ref().unwrap().path, third));

        // Closing a window with unsaved changes asks first.
        other.update(cx, |e, cx| e.add_item(0, 40., 40., 80., 20., ItemKind::Check, cx));
        let mut other_cx = VisualTestContext::from_window(other_window, cx);
        assert!(!other_cx.simulate_close(), "not closed until the answer is to discard");
        other_cx.run_until_parked();
        other_cx.simulate_prompt_answer("Cancel");
        other_cx.run_until_parked();
        assert_eq!(others(cx).len(), 1);
        assert!(!other_cx.simulate_close());
        other_cx.run_until_parked();
        other_cx.simulate_prompt_answer("Discard");
        other_cx.run_until_parked();
        assert!(others(cx).is_empty());

        // A window with nothing in it closes when asked to go back, unless it is the last.
        cx.update(|_, cx| open_editor(cx, None)).unwrap();
        let (empty_window, empty) = others(cx).pop().unwrap();
        empty_window.update(cx, |_, window, cx| empty.update(cx, |e, cx| e.request_close(window, cx))).unwrap();
        cx.run_until_parked();
        assert!(others(cx).is_empty());
        editor.update_in(cx, |e, window, cx| e.close_document(window, cx));
        editor.update_in(cx, |e, window, cx| e.request_close(window, cx));
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| cx.windows().len()), 1);
    }

    #[gpui::test]
    fn protected_pdfs_ask_for_their_password(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let dir = test_dir();
        let path = dir.join("protected.pdf");
        std::fs::write(&path, crate::pdf::tests::encrypted(&crate::pdf::tests::sample_pdf(), "sesame", "owner")).unwrap();

        editor.update_in(cx, |e, window, cx| e.open_path(path.clone(), window, cx));
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
        editor.update_in(cx, |e, window, cx| e.save(SaveTo::File, window, cx));
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
            assert!(prompt.purpose == PasswordPurpose::NewPassword { first: None, to: SaveTo::File });
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
        editor.update_in(cx, |e, window, cx| e.save(SaveTo::File, window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Remove Password");
        let open = dir.join("protected-removed.pdf");
        save_as(cx, open.clone());
        assert_eq!(opens(&open, None), Ok(()));
        editor.read_with(cx, |e, _| assert!(e.status.contains("without a password"), "{}", e.status));
        assert!(thumbnail(&editor, cx), "a copy without a password gets its thumbnail");

        // The same password as the original.
        editor.update_in(cx, |e, window, cx| e.save(SaveTo::File, window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Keep Password");
        let kept = dir.join("protected-kept.pdf");
        save_as(cx, kept.clone());
        assert_eq!(opens(&kept, Some("sesame")), Ok(()));
        assert!(!thumbnail(&editor, cx));

        // A copy for the clipboard is protected in the same ways. It is not a
        // save: Recents couldn't bring the work back.
        editor.update(cx, |e, _| e.mark_dirty());
        cx.simulate_keystrokes("secondary-shift-c");
        cx.run_until_parked();
        cx.simulate_prompt_answer("Keep Password");
        cx.run_until_parked();
        let copy = clipboard::last_put().expect("a copy on the clipboard");
        assert_eq!(opens(&copy, None).unwrap_err(), "this PDF is password protected");
        assert_eq!(opens(&copy, Some("sesame")), Ok(()));
        editor.read_with(cx, |e, _| assert!(e.dirty && e.status.contains("to the clipboard"), "{}", e.status));
        editor.update_in(cx, |e, window, cx| e.save(SaveTo::Clipboard, window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Change Password…");
        cx.run_until_parked();
        for _ in 0..2 {
            cx.simulate_input("new-2");
            cx.simulate_keystrokes("enter");
        }
        cx.run_until_parked();
        assert_eq!(opens(&clipboard::last_put().unwrap(), Some("new-2")), Ok(()));
        editor.read_with(cx, |e, _| assert!(e.status.contains("with the new password to the clipboard"), "{}", e.status));
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

    #[test]
    fn the_pages_kept_drawn_fit_the_budget() {
        // Ten pages that take 40,000 bytes each at 100%.
        let sizes = vec![(100., 100.); 10];
        assert_eq!(pages_in_budget(&sizes, 4, 1., 5 * 40_000), vec![4, 3, 5, 2, 6], "the nearest, nearest first");
        assert_eq!(pages_in_budget(&sizes, 4, 2., 5 * 40_000), vec![4, 3, 5], "fewer at a larger scale, but never under three");
        assert_eq!(pages_in_budget(&sizes, 9, 1., 0), vec![9, 8, 7]);
        assert_eq!(pages_in_budget(&sizes, 0, 1., usize::MAX).len(), 10);
        assert!(pages_in_budget(&[], 0, 1., 0).is_empty());
    }

    #[gpui::test]
    fn a_long_document_keeps_the_pages_around_the_view_drawn(cx: &mut TestAppContext) {
        const PAGES: usize = 12;
        let (editor, cx) = setup(cx);
        let path = test_dir().join("long-budget.pdf");
        std::fs::write(&path, long_pdf(PAGES)).unwrap();
        // Room for no more than the least: three pages.
        editor.update_in(cx, |e, window, cx| {
            e.render_budget = 0;
            e.open_path(path, window, cx);
        });
        cx.run_until_parked();
        let drawn = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| {
                let doc = e.doc.as_ref().unwrap();
                doc.order.iter().enumerate().filter(|&(_, &i)| doc.pages[i].image.is_some()).map(|(position, _)| position).collect::<Vec<_>>()
            })
        };
        assert_eq!(drawn(&editor, cx), vec![0, 1, 2]);

        // They follow the view, the others giving up their bitmaps...
        editor.update(cx, |e, cx| e.scroll_to_page(8, cx));
        cx.run_until_parked();
        assert_eq!(drawn(&editor, cx), vec![7, 8, 9]);
        // ...and the pages, as they go and come back.
        editor.update(cx, |e, cx| e.delete_page(8, cx));
        cx.run_until_parked();
        assert_eq!(drawn(&editor, cx).len(), 3);
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(drawn(&editor, cx).len(), 3);
        editor.read_with(cx, |e, _| {
            let (doc, current) = (e.doc.as_ref().unwrap(), e.current_page().unwrap());
            assert!(doc.pages[doc.order[current]].image.is_some(), "the page in view is drawn");
        });

        // With room for all of them, all are drawn.
        editor.update_in(cx, |e, window, cx| {
            e.render_budget = usize::MAX;
            e.ensure_rendered(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(drawn(&editor, cx).len(), PAGES);
        editor.read_with(cx, |e, _| assert!(e.drawn_around.is_none()));
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
    fn the_thumbnail_rail_follows_the_page_in_view(cx: &mut TestAppContext) {
        const PAGES: usize = 40;
        let (editor, cx) = setup(cx);
        let path = test_dir().join("long-rail.pdf");
        std::fs::write(&path, long_pdf(PAGES)).unwrap();
        editor.update_in(cx, |e, window, cx| {
            e.settings.show_right_sidebar = true;
            e.open_path(path, window, cx);
        });
        cx.run_until_parked();
        // Whether a page's thumbnail is in sight, whole.
        let in_sight = |editor: &Entity<Editor>, cx: &mut VisualTestContext, page: usize| {
            editor.read_with(cx, |e, _| {
                let (top, height) = e.thumb_tops().unwrap().nth(page).unwrap();
                let viewport = e.thumbs_scroll.bounds();
                top >= viewport.top() && top + height <= viewport.bottom()
            })
        };
        let rail = |editor: &Entity<Editor>, cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.thumbs_scroll.offset().y);
        assert!(in_sight(&editor, cx, 0) && !in_sight(&editor, cx, 30));
        assert_eq!(rail(&editor, cx), px(0.));

        // Scrolling the document down brings the rail along, and back up.
        editor.update(cx, |e, cx| e.scroll_to_page(30, cx));
        cx.run_until_parked();
        assert!(in_sight(&editor, cx, 30), "the rail is at {:?}", rail(&editor, cx));
        editor.read_with(cx, |e, _| assert!(e.drawn_thumbs()[30], "and the thumbnail is drawn"));
        editor.update(cx, |e, cx| e.scroll_to_page(12, cx));
        cx.run_until_parked();
        assert!(in_sight(&editor, cx, 12) && !in_sight(&editor, cx, 30));

        // With the wheel too, a page at a time.
        let viewport = editor.read_with(cx, |e, _| e.pages_scroll.bounds());
        for _ in 0..40 {
            cx.simulate_event(ScrollWheelEvent {
                position: viewport.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-400.))),
                modifiers: Modifiers::none(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
            let current = editor.read_with(cx, |e, _| e.current_page().unwrap());
            assert!(in_sight(&editor, cx, current), "page {current}'s thumbnail is out of sight");
        }
        let current = editor.read_with(cx, |e, _| e.current_page().unwrap());
        assert!(current > 20, "the wheel scrolled the document to page {current}");

        // The rail can be scrolled away from that page, to look through the
        // others: it stays where it's put until another page comes into view.
        editor.update(cx, |e, cx| {
            e.thumbs_scroll.set_offset(point(px(0.), px(0.)));
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(rail(&editor, cx), px(0.));
        assert!(!in_sight(&editor, cx, current));
        editor.update(cx, |e, cx| e.scroll_to_page(current + 1, cx));
        cx.run_until_parked();
        assert!(in_sight(&editor, cx, current + 1));

        // Hidden and shown again, it is where the document is.
        cx.dispatch_action(ToggleRightSidebar);
        cx.run_until_parked();
        editor.update(cx, |e, cx| e.scroll_to_page(3, cx));
        cx.run_until_parked();
        cx.dispatch_action(ToggleRightSidebar);
        cx.run_until_parked();
        assert!(in_sight(&editor, cx, 3), "the rail is at {:?}", rail(&editor, cx));

        // Another document starts at the top.
        editor.update(cx, |e, cx| e.scroll_to_page(35, cx));
        cx.run_until_parked();
        assert!(rail(&editor, cx) < px(0.));
        let path = test_dir().join("long-rail-2.pdf");
        std::fs::write(&path, long_pdf(PAGES)).unwrap();
        editor.update_in(cx, |e, window, cx| e.open_path(path, window, cx));
        cx.run_until_parked();
        assert_eq!(rail(&editor, cx), px(0.));
        assert!(in_sight(&editor, cx, 0));
    }

    #[gpui::test]
    fn scroll_bars_scroll_the_pages_and_the_rail(cx: &mut TestAppContext) {
        const PAGES: usize = 40;
        let (editor, cx) = setup(cx);
        let path = test_dir().join("long-bars.pdf");
        std::fs::write(&path, long_pdf(PAGES)).unwrap();
        editor.update_in(cx, |e, window, cx| {
            e.settings.show_right_sidebar = true;
            e.open_path(path, window, cx);
        });
        cx.run_until_parked();
        // Zoomed in, the pages scroll both ways.
        editor.update_in(cx, |e, window, cx| e.set_zoom(3.0, window, cx));
        cx.run_until_parked();
        let pages = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| (e.pages_scroll.offset(), e.pages_scroll.max_offset(), e.pages_scroll.bounds()))
        };
        let (start, max, viewport) = pages(&editor, cx);
        assert!(max.width > px(0.) && max.height > px(0.));
        let half = px(scrollbar::STRIP / 2.);
        let none = Modifiers::none();

        // Pressing the strip down the right edge brings the bar there: near the end of the document.
        let low = point(viewport.right() - half, viewport.bottom() - px(60.));
        cx.simulate_mouse_down(low, MouseButton::Left, none);
        cx.run_until_parked();
        let (jumped, _, _) = pages(&editor, cx);
        assert!(jumped.y < -max.height * 0.8 && jumped.x == start.x, "{jumped:?} of {max:?}");
        editor.read_with(cx, |e, _| assert!(e.pan.is_none() && e.items.is_empty(), "the press is the bar's alone"));
        // Dragged from there, the document follows: up, down to the end and no further.
        cx.simulate_mouse_move(low - point(px(0.), px(200.)), MouseButton::Left, none);
        cx.run_until_parked();
        let (dragged, _, _) = pages(&editor, cx);
        assert!(dragged.y > jumped.y && dragged.y < px(0.), "{dragged:?} after {jumped:?}");
        cx.simulate_mouse_move(low + point(px(-300.), px(900.)), MouseButton::Left, none);
        cx.run_until_parked();
        assert_eq!(pages(&editor, cx).0, point(start.x, -max.height));
        // All the way up, the bar is at the top again.
        cx.simulate_mouse_move(point(low.x, viewport.top() - px(500.)), MouseButton::Left, none);
        cx.simulate_mouse_up(point(low.x, viewport.top() - px(500.)), MouseButton::Left, none);
        cx.run_until_parked();
        assert_eq!(pages(&editor, cx).0, point(start.x, px(0.)));
        // Let go, it stays put.
        cx.simulate_mouse_move(low, None, none);
        cx.run_until_parked();
        assert_eq!(pages(&editor, cx).0, point(start.x, px(0.)));
        editor.read_with(cx, |e, _| assert!(e.scrollbar_held.get().is_none()));

        // Taken hold of, the bar doesn't jump: it moves as far as it's dragged.
        let top = point(viewport.right() - half, viewport.top() + px(10.));
        cx.simulate_mouse_down(top, MouseButton::Left, none);
        cx.run_until_parked();
        assert_eq!(pages(&editor, cx).0.y, px(0.));
        cx.simulate_mouse_move(top + point(px(0.), px(50.)), MouseButton::Left, none);
        cx.simulate_mouse_up(top + point(px(0.), px(50.)), MouseButton::Left, none);
        cx.run_until_parked();
        let (held, _, _) = pages(&editor, cx);
        assert!(held.y < px(0.) && held.y > -max.height / 2., "{held:?} of {max:?}");

        // The same along the bottom edge, which stops short of the corner.
        let right = point(viewport.right() - px(60.), viewport.bottom() - half);
        cx.simulate_mouse_down(right, MouseButton::Left, none);
        cx.simulate_mouse_move(right + point(px(400.), px(0.)), MouseButton::Left, none);
        cx.simulate_mouse_up(right + point(px(400.), px(0.)), MouseButton::Left, none);
        cx.run_until_parked();
        assert_eq!(pages(&editor, cx).0, point(-max.width, held.y));
        editor.read_with(cx, |e, _| assert!(e.pan.is_none() && e.items.is_empty()));

        // The wheel scrolls over a bar as anywhere else.
        cx.simulate_event(ScrollWheelEvent {
            position: top,
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-120.))),
            modifiers: none,
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();
        assert_eq!(pages(&editor, cx).0.y, held.y - px(120.));

        // The rail has one too, and scrolling it leaves the document where it is.
        let (before, _, _) = pages(&editor, cx);
        let rail = editor.read_with(cx, |e, _| e.thumbs_scroll.bounds());
        let low = point(rail.right() - half, rail.bottom() - px(40.));
        cx.simulate_mouse_down(low, MouseButton::Left, none);
        cx.simulate_mouse_move(low + point(px(0.), px(300.)), MouseButton::Left, none);
        cx.simulate_mouse_up(low + point(px(0.), px(300.)), MouseButton::Left, none);
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            assert_eq!(e.thumbs_scroll.offset().y, -e.thumbs_scroll.max_offset().height);
            assert!(e.drawn_thumbs()[PAGES - 1], "the last thumbnails are drawn");
            assert_eq!(e.pages_scroll.offset(), before);
        });

        // At a zoom where everything fits across, there is no bar along the bottom:
        // a press there is the page view's, as before.
        editor.update_in(cx, |e, window, cx| e.set_zoom(0.5, window, cx));
        cx.run_until_parked();
        let (_, max, viewport) = pages(&editor, cx);
        assert_eq!(max.width, px(0.));
        cx.simulate_mouse_down(point(viewport.left() + px(60.), viewport.bottom() - half), MouseButton::Left, none);
        editor.read_with(cx, |e, _| assert!(e.pan.is_some() && e.scrollbar_held.get().is_none()));
        cx.simulate_mouse_up(point(viewport.left() + px(60.), viewport.bottom() - half), MouseButton::Left, none);
    }

    #[gpui::test]
    fn a_long_document_holds_only_the_bitmaps_it_shows(cx: &mut TestAppContext) {
        const PAGES: usize = 150;
        let (editor, cx) = setup(cx);
        let path = test_dir().join("long-memory.pdf");
        std::fs::write(&path, long_pdf(PAGES)).unwrap();
        editor.update_in(cx, |e, window, cx| {
            e.settings.show_right_sidebar = true;
            e.open_path(path, window, cx);
        });
        cx.run_until_parked();
        // The positions with a thumbnail, and with the page itself drawn.
        let held = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| {
                let doc = e.doc.as_ref().unwrap();
                let positions = |has: fn(&PageView) -> bool| (0..PAGES).filter(|&p| has(&doc.pages[doc.order[p]])).collect::<Vec<_>>();
                (positions(|page| page.thumb.is_some()), positions(|page| page.image.is_some()))
            })
        };

        // Just opened: the first pages and their thumbnails, not all hundred and fifty.
        let (thumbs, pages) = held(&editor, cx);
        assert!(thumbs.starts_with(&[0, 1, 2]) && thumbs.len() < 40 && thumbs.iter().all(|&p| p < 40), "{thumbs:?}");
        assert!(pages.starts_with(&[0, 1, 2]) && pages.len() < PAGES, "{pages:?}");

        // Far down, the ones there: around the page in view, and in sight in the rail.
        editor.update(cx, |e, cx| e.scroll_to_page(100, cx));
        cx.run_until_parked();
        let (thumbs, pages) = held(&editor, cx);
        assert!((100 - THUMBS_AROUND..=100 + THUMBS_AROUND).all(|p| thumbs.contains(&p)), "{thumbs:?}");
        assert!(pages.contains(&100) && pages.iter().all(|&p| p.abs_diff(100) < 40), "{pages:?}");
        editor.read_with(cx, |e, _| {
            let shown = e.drawn_thumbs();
            let doc = e.doc.as_ref().unwrap();
            assert!((1..PAGES).filter(|&p| shown[p]).all(|p| doc.pages[doc.order[p]].thumb.is_some()), "those in sight in the rail");
        });

        // Paged through from end to end, it never holds many more than it keeps.
        for page in (0..PAGES).step_by(4) {
            editor.update(cx, |e, cx| e.scroll_to_page(page, cx));
            cx.run_until_parked();
            let (thumbs, pages) = held(&editor, cx);
            assert!(thumbs.contains(&page) && pages.contains(&page), "page {page} is drawn, and its thumbnail");
            assert!(thumbs.len() <= THUMBS_KEPT + 3 * THUMBS_AROUND + 24, "{} thumbnails at page {page}", thumbs.len());
        }
        let (thumbs, _) = held(&editor, cx);
        // (All but the first, which the rail always draws: the others are placed from it.)
        assert!(thumbs.iter().all(|&p| p == 0 || p > 20), "the first pages' thumbnails were let go: {thumbs:?}");

        // With the rail hidden, there are still thumbnails around the page in view, to stand in for it.
        cx.dispatch_action(ToggleRightSidebar);
        editor.update(cx, |e, cx| e.scroll_to_page(40, cx));
        cx.run_until_parked();
        let (thumbs, _) = held(&editor, cx);
        assert!((40 - THUMBS_AROUND..=40 + THUMBS_AROUND).all(|p| thumbs.contains(&p)), "{thumbs:?}");

        // A page is drawn as sharply as it's shown: sharper when zoomed in, and
        // not left that large once zoomed out again.
        let sharpness = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |e, _| {
                let doc = e.doc.as_ref().unwrap();
                doc.pages[doc.order[e.current_page().unwrap()]].rendered_scale
            })
        };
        let settle = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
        };
        let factor = cx.update(|window, _| window.scale_factor());
        editor.update_in(cx, |e, window, cx| e.set_zoom(1.0, window, cx));
        settle(cx);
        assert_eq!(sharpness(&editor, cx), factor.max(1.));
        editor.update_in(cx, |e, window, cx| e.set_zoom(3.0, window, cx));
        settle(cx);
        assert_eq!(sharpness(&editor, cx), 3. * factor);
        editor.update_in(cx, |e, window, cx| e.set_zoom(1.0, window, cx));
        settle(cx);
        assert_eq!(sharpness(&editor, cx), factor.max(1.));
    }

    #[test]
    fn page_ranges_are_read_as_typed() {
        assert_eq!(parse_page_ranges("1-3, 5", 6), Ok(vec![0, 1, 2, 4]));
        assert_eq!(parse_page_ranges("1 - 2,4 6", 6), Ok(vec![0, 1, 3, 5]));
        assert_eq!(parse_page_ranges("1–2", 6), Ok(vec![0, 1]));
        // In the order given, each page once.
        assert_eq!(parse_page_ranges("3-1", 6), Ok(vec![2, 1, 0]));
        assert_eq!(parse_page_ranges("2, 1-3, 2", 6), Ok(vec![1, 0, 2]));
        for wrong in ["", " , ", "0", "7", "2-9", "3-", "-3", "1-2-3", "x"] {
            assert!(parse_page_ranges(wrong, 6).is_err(), "{wrong:?}");
        }
    }

    #[gpui::test]
    fn pages_of_another_pdf_are_added_and_come_back_from_recents(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let original = editor.read_with(cx, |e, _| e.doc.as_ref().unwrap().path.clone());
        // Another copy of the sample: an upright page, then one on its side.
        let other = original.with_file_name(format!("more-{}", file_name(&original)));
        std::fs::write(&other, crate::pdf::tests::sample_pdf()).unwrap();
        let order = |cx: &mut VisualTestContext| editor.read_with(cx, |e, _| e.doc.as_ref().unwrap().order.clone());

        // Choosing a PDF asks which of its pages, suggesting all of them.
        editor.update_in(cx, |e, window, cx| e.read_pages_from(other.clone(), None, window, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            let prompt = e.import_prompt.as_ref().expect("asks which pages");
            assert_eq!((prompt.text.as_str(), prompt.untouched), ("1-2", true));
        });
        // Typing replaces the suggestion. Pages that aren't there can't be added.
        cx.simulate_input("3");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert_eq!(e.import_prompt.as_ref().map(|p| p.text.as_str()), Some("3")));
        assert_eq!(order(cx), [0, 1]);
        cx.simulate_keystrokes("backspace");
        cx.simulate_input("2, 1");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        // Its second page, then its first, after the document's own.
        assert_eq!(order(cx), [0, 1, 2, 3]);
        editor.read_with(cx, |e, _| {
            assert!(e.import_prompt.is_none());
            let doc = e.doc.as_ref().unwrap();
            let sizes: Vec<_> = doc.geoms().map(|g| (g.width, g.height)).collect();
            assert_eq!(sizes, [(612., 792.), (750., 580.), (750., 580.), (612., 792.)]);
            assert!(doc.pages.iter().all(|p| p.thumb.is_some() && p.image.is_some()), "the new pages are drawn");
            assert_eq!(doc.imports, [PageImport { path: other.clone(), pages: vec![1, 0] }]);
            assert!(e.dirty);
            assert_eq!(e.current_page(), Some(2), "the first new page is in view");
        });

        // Adding is undoable, and added pages are deleted and moved like any other.
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(order(cx), [0, 1]);
        cx.simulate_keystrokes("secondary-shift-z");
        assert_eq!(order(cx), [0, 1, 2, 3]);
        editor.update(cx, |e, cx| {
            e.delete_page(2, cx);
            e.move_page(2, 0, cx);
        });
        assert_eq!(order(cx), [3, 0, 1]);

        // Something on the added page, then save.
        editor.update(cx, |e, cx| e.place_tool(Tool::Check, 0, 100., 100., cx));
        let out = test_dir().join(format!("merged-{}", file_name(&original)));
        editor.update_in(cx, |e, window, cx| e.save(SaveTo::File, window, cx));
        cx.run_until_parked();
        cx.simulate_new_path_selection({
            let out = out.clone();
            move |_| Some(out)
        });
        cx.run_until_parked();
        let saved = pdf::load(std::fs::read(&out).unwrap()).unwrap();
        let sizes: Vec<_> = saved.pages.iter().map(|g| (g.width, g.height)).collect();
        assert_eq!(sizes, [(612., 792.), (612., 792.), (750., 580.)]);
        let id = editor.read_with(cx, |e, _| {
            assert!(!e.dirty);
            let entry = &e.recents[0];
            assert_eq!(entry.imports, [PageImport { path: other.clone(), pages: vec![1, 0] }]);
            assert_eq!(entry.order.as_deref(), Some(&[3, 0, 1][..]));
            entry.id.clone()
        });

        // Reopened from Recents, the pages are added again and everything is as it was.
        editor.update_in(cx, |e, window, cx| e.close_document(window, cx));
        editor.update_in(cx, |e, window, cx| e.open_recent(&id, window, cx));
        cx.run_until_parked();
        assert_eq!(order(cx), [3, 0, 1]);
        editor.read_with(cx, |e, _| {
            let doc = e.doc.as_ref().unwrap();
            assert_eq!((doc.path.as_path(), doc.loaded.pages.len(), doc.imports.len()), (original.as_path(), 4, 1));
            assert_eq!((e.items.len(), e.items[0].page, e.dirty), (1, 0, false));
        });

        // Once the other PDF is gone, the saved copy is opened instead.
        std::fs::remove_file(&other).unwrap();
        editor.update_in(cx, |e, window, cx| e.close_document(window, cx));
        editor.update_in(cx, |e, window, cx| e.open_recent(&id, window, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            let doc = e.doc.as_ref().unwrap();
            assert_eq!((doc.path.as_path(), doc.page_count()), (out.as_path(), 3));
            assert!(e.items.is_empty() && e.status.contains("can't be read any more"), "{}", e.status);
        });

        // A PDF with a single page is added without asking.
        let single = test_dir().join(format!("single-{}", file_name(&original)));
        let two = pdf::load(crate::pdf::tests::sample_pdf()).unwrap();
        std::fs::write(&single, two.save(&pdf::Edits { order: &[1], ..Default::default() }).unwrap().bytes).unwrap();
        editor.update_in(cx, |e, window, cx| e.read_pages_from(single, None, window, cx));
        cx.run_until_parked();
        editor.read_with(cx, |e, _| assert!(e.import_prompt.is_none()));
        assert_eq!(order(cx), [0, 1, 2, 3]);
    }

    #[gpui::test]
    fn sidebars_hide_and_show_and_help_opens(cx: &mut TestAppContext) {
        let (editor, cx) = setup(cx);
        let page_view = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            editor.read_with(cx, |e, _| e.pages_scroll.bounds())
        };
        let both = page_view(cx);

        // Hiding a sidebar gives its width to the pages.
        cx.simulate_keystrokes("secondary-b");
        let no_left = page_view(cx);
        assert_eq!(no_left.left(), px(0.));
        assert_eq!(no_left.size.width - both.size.width, px(SIDEBAR_W));
        cx.simulate_keystrokes("secondary-alt-b");
        let neither = page_view(cx);
        assert_eq!(neither.size.width - no_left.size.width, px(THUMB_RAIL_W));
        // The choice is kept for next time.
        let saved = Settings::load();
        assert!(!saved.show_left_sidebar && !saved.show_right_sidebar);

        // A menu in the sidebar closes with it.
        cx.simulate_keystrokes("secondary-b");
        editor.update(cx, |e, _| e.date_menu_open = true);
        cx.simulate_keystrokes("secondary-b");
        editor.read_with(cx, |e, _| assert!(!e.date_menu_open && !e.settings.show_left_sidebar));

        cx.simulate_keystrokes("secondary-b secondary-alt-b");
        assert_eq!(page_view(cx), both);
        let saved = Settings::load();
        assert!(saved.show_left_sidebar && saved.show_right_sidebar);

        // Help lists the shortcuts; Escape or a click elsewhere closes it.
        cx.simulate_keystrokes("secondary-/");
        editor.read_with(cx, |e, _| assert!(e.help_menu_open));
        cx.simulate_keystrokes("escape");
        editor.read_with(cx, |e, _| assert!(!e.help_menu_open));
        cx.simulate_keystrokes("secondary-/");
        cx.run_until_parked();
        cx.simulate_click(both.center(), Modifiers::none());
        editor.read_with(cx, |e, _| assert!(!e.help_menu_open));

        // Every shortcut listed is one the app has, written in a way `ui::keys` understands.
        for (label, keys) in SHORTCUTS.iter().flat_map(|(_, rows)| rows.iter()) {
            assert!(!label.is_empty() && !ui::keys(keys).is_empty(), "{label}");
        }
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
            imports: Vec::new(),
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
