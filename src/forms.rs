//! Interactive form fields (AcroForm): reading them from a PDF, and laying out
//! their values the same way on screen and in the saved file.
//!
//! Filled-in values are written back into the fields themselves (see
//! `pdf::fill_form`), so the saved PDF is still a fillable form that other
//! apps show and edit as usual.

use std::collections::HashMap;

use hayro::kurbo::{Affine, Point};
use lopdf::{Dictionary, Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::model::{LINE_HEIGHT, PageGeom};

/// A form field the user can fill in.
#[derive(Clone, Debug)]
pub struct Field {
    /// The field's dictionary (for radio buttons: the group, whose kids are the buttons).
    pub id: ObjectId,
    /// Fully qualified name, e.g. "applicant.name".
    pub name: String,
    pub kind: FieldKind,
    /// Where the field appears (usually once).
    pub widgets: Vec<Widget>,
    /// The value in the file as opened.
    pub value: FieldValue,
    /// Most characters a text field takes.
    pub max_len: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    Text { multiline: bool, comb: bool },
    CheckBox,
    Radio { no_toggle_off: bool },
    Choice { options: Vec<ChoiceOption>, combo: bool, editable: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChoiceOption {
    /// What is stored in the field.
    pub value: String,
    /// What is shown.
    pub label: String,
}

/// A field's value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FieldValue {
    /// Text fields and choices (the option's value).
    Text(String),
    /// Check boxes and radio buttons: the "on" state that is selected, if any.
    On(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// The mark drawn in a selected check box or radio button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Symbol {
    Check,
    Circle,
    Cross,
    Square,
    Diamond,
}

/// One place a field appears on a page.
#[derive(Clone, Debug)]
pub struct Widget {
    pub id: ObjectId,
    /// Index of the page in the original document.
    pub page: usize,
    /// Position on the displayed page (view space): x, y, width, height.
    pub rect: [f32; 4],
    /// Size in the widget's own frame (width and height swap when it's turned 90°).
    pub size: (f32, f32),
    /// Quarter turns of the widget's content (MK/R): 0, 90, 180 or 270.
    pub rotation: u16,
    /// Whether the content reads upright on screen (the widget is turned
    /// with its page). Others are filled in without a live preview.
    pub upright: bool,
    /// For check boxes and radio buttons: the state that means "selected here".
    pub on_state: Option<String>,
    /// Font size from the default appearance; 0 means "fit the field".
    pub font_size: f32,
    pub align: Align,
    pub text_color: [f32; 3],
    pub background: Option<[f32; 3]>,
    pub border: Option<[f32; 3]>,
    pub border_width: f32,
    pub symbol: Symbol,
}

impl Field {
    /// The text shown for the field's value (an option's label rather than its value).
    pub fn display_text(&self, value: &FieldValue) -> String {
        match (value, &self.kind) {
            (FieldValue::Text(v), FieldKind::Choice { options, .. }) => {
                options.iter().find(|o| o.value == *v).map_or_else(|| v.clone(), |o| o.label.clone())
            }
            (FieldValue::Text(v), _) => v.clone(),
            (FieldValue::On(_), _) => String::new(),
        }
    }

    /// `text` cut to what the field accepts.
    pub fn accept(&self, text: &str) -> String {
        let text = match self.kind {
            FieldKind::Text { multiline: true, .. } => text.to_string(),
            _ => text.replace('\n', " "),
        };
        match self.max_len {
            Some(n) => text.chars().take(n).collect(),
            None => text,
        }
    }
}

/// The shape of a check box or radio button mark, in a unit square (y up).
pub enum Mark {
    /// Stroked lines.
    Lines(Vec<Vec<(f32, f32)>>),
    Polygon(Vec<(f32, f32)>),
    /// A filled circle in the middle, with this radius.
    Disc(f32),
}

impl Symbol {
    pub fn mark(self) -> Mark {
        match self {
            Symbol::Check => Mark::Lines(vec![vec![(0.15, 0.5), (0.4, 0.2), (0.85, 0.85)]]),
            Symbol::Cross => Mark::Lines(vec![vec![(0.2, 0.2), (0.8, 0.8)], vec![(0.2, 0.8), (0.8, 0.2)]]),
            Symbol::Circle => Mark::Disc(0.3),
            Symbol::Square => Mark::Polygon(vec![(0.25, 0.25), (0.75, 0.25), (0.75, 0.75), (0.25, 0.75)]),
            Symbol::Diamond => Mark::Polygon(vec![(0.5, 0.15), (0.85, 0.5), (0.5, 0.85), (0.15, 0.5)]),
        }
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

const FF_READ_ONLY: i64 = 1;
const FF_MULTILINE: i64 = 1 << 12;
const FF_PASSWORD: i64 = 1 << 13;
const FF_NO_TOGGLE_TO_OFF: i64 = 1 << 14;
const FF_RADIO: i64 = 1 << 15;
const FF_PUSHBUTTON: i64 = 1 << 16;
const FF_COMBO: i64 = 1 << 17;
const FF_EDIT: i64 = 1 << 18;
const FF_FILE_SELECT: i64 = 1 << 20;
const FF_COMB: i64 = 1 << 24;
const ANNOT_HIDDEN: i64 = 1 << 1;
const ANNOT_NO_VIEW: i64 = 1 << 5;

/// Field attributes that kids inherit from their parents.
#[derive(Clone, Default)]
struct Inherited {
    name: Vec<String>,
    ft: Option<Vec<u8>>,
    ff: i64,
    v: Option<Object>,
    da: Option<String>,
    q: Option<i64>,
    max_len: Option<i64>,
    opt: Option<Object>,
}

/// The fields of a document that can be filled in. Read-only fields,
/// buttons, signature fields and password fields are left alone.
pub fn read(doc: &Document, pages: &[PageGeom]) -> Vec<Field> {
    let Some(form) = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"AcroForm").ok())
        .and_then(|f| resolve_dict(doc, f))
    else {
        return Vec::new();
    };
    // Widgets are placed on a page by being in its /Annots.
    let mut page_of: HashMap<ObjectId, usize> = HashMap::new();
    for (index, (_, page_id)) in doc.get_pages().into_iter().enumerate() {
        let annots = doc.get_dictionary(page_id).ok().and_then(|p| p.get(b"Annots").ok());
        if let Some(Object::Array(annots)) = annots.and_then(|a| resolve(doc, a)) {
            for annot in annots {
                if let Ok(id) = annot.as_reference() {
                    page_of.entry(id).or_insert(index);
                }
            }
        }
    }
    let defaults = Inherited {
        da: form.get(b"DA").ok().and_then(|o| text(doc, o)),
        q: form.get(b"Q").ok().and_then(|o| resolve(doc, o)).and_then(|o| o.as_i64().ok()),
        ..Inherited::default()
    };
    let mut fields = Vec::new();
    if let Ok(Object::Array(roots)) = form.get(b"Fields").map(|o| resolve(doc, o).unwrap_or(Object::Null)) {
        for root in roots {
            if let Ok(id) = root.as_reference() {
                walk(doc, id, &defaults, pages, &page_of, &mut fields, 0);
            }
        }
    }
    fields
}

fn walk(
    doc: &Document,
    id: ObjectId,
    parent: &Inherited,
    pages: &[PageGeom],
    page_of: &HashMap<ObjectId, usize>,
    out: &mut Vec<Field>,
    depth: usize,
) {
    let Ok(dict) = doc.get_dictionary(id) else { return };
    if depth > 32 {
        return;
    }
    let mut attrs = parent.clone();
    if let Some(t) = dict.get(b"T").ok().and_then(|o| text(doc, o)) {
        attrs.name.push(t);
    }
    if let Ok(ft) = dict.get(b"FT").and_then(Object::as_name) {
        attrs.ft = Some(ft.to_vec());
    }
    if let Some(ff) = dict.get(b"Ff").ok().and_then(|o| resolve(doc, o)).and_then(|o| o.as_i64().ok()) {
        attrs.ff = ff;
    }
    if let Ok(v) = dict.get(b"V") {
        attrs.v = resolve(doc, v);
    }
    if let Some(da) = dict.get(b"DA").ok().and_then(|o| text(doc, o)) {
        attrs.da = Some(da);
    }
    if let Some(q) = dict.get(b"Q").ok().and_then(|o| resolve(doc, o)).and_then(|o| o.as_i64().ok()) {
        attrs.q = Some(q);
    }
    if let Some(n) = dict.get(b"MaxLen").ok().and_then(|o| resolve(doc, o)).and_then(|o| o.as_i64().ok()) {
        attrs.max_len = Some(n);
    }
    if let Ok(opt) = dict.get(b"Opt") {
        attrs.opt = resolve(doc, opt);
    }

    // Kids with names are fields of their own; kids without are this field's widgets.
    let kids: Vec<ObjectId> = match dict.get(b"Kids").ok().and_then(|k| resolve(doc, k)) {
        Some(Object::Array(kids)) => kids.iter().filter_map(|k| k.as_reference().ok()).collect(),
        _ => Vec::new(),
    };
    let (subfields, widgets): (Vec<ObjectId>, Vec<ObjectId>) =
        kids.into_iter().partition(|k| doc.get_dictionary(*k).is_ok_and(|d| d.has(b"T")));
    for kid in subfields {
        walk(doc, kid, &attrs, pages, page_of, out, depth + 1);
    }
    let mut widget_ids = widgets;
    if dict.get(b"Subtype").and_then(Object::as_name).is_ok_and(|s| s == b"Widget") {
        widget_ids.insert(0, id);
    }
    if widget_ids.is_empty() || attrs.ft.is_none() {
        return;
    }
    if let Some(field) = make_field(doc, id, &attrs, &widget_ids, pages, page_of) {
        out.push(field);
    }
}

fn make_field(
    doc: &Document,
    id: ObjectId,
    attrs: &Inherited,
    widget_ids: &[ObjectId],
    pages: &[PageGeom],
    page_of: &HashMap<ObjectId, usize>,
) -> Option<Field> {
    let ff = attrs.ff;
    if ff & FF_READ_ONLY != 0 {
        return None;
    }
    let kind = match attrs.ft.as_deref()? {
        b"Tx" if ff & (FF_PASSWORD | FF_FILE_SELECT) == 0 => FieldKind::Text {
            multiline: ff & FF_MULTILINE != 0,
            comb: ff & FF_COMB != 0 && ff & (FF_MULTILINE | FF_PASSWORD | FF_FILE_SELECT) == 0 && attrs.max_len.is_some_and(|n| n > 0),
        },
        b"Btn" if ff & FF_PUSHBUTTON != 0 => return None,
        b"Btn" if ff & FF_RADIO != 0 => FieldKind::Radio { no_toggle_off: ff & FF_NO_TOGGLE_TO_OFF != 0 },
        b"Btn" => FieldKind::CheckBox,
        b"Ch" => FieldKind::Choice {
            options: options(doc, attrs.opt.as_ref()),
            combo: ff & FF_COMBO != 0,
            editable: ff & FF_COMBO != 0 && ff & FF_EDIT != 0,
        },
        _ => return None,
    };
    let button = matches!(kind, FieldKind::CheckBox | FieldKind::Radio { .. });
    let widgets: Vec<Widget> = widget_ids
        .iter()
        .filter_map(|&wid| make_widget(doc, wid, attrs, button, pages, page_of))
        .collect();
    if widgets.is_empty() {
        return None;
    }
    let value = if button {
        let on = match &attrs.v {
            Some(Object::Name(n)) if n.as_slice() != b"Off" => Some(String::from_utf8_lossy(n).into_owned()),
            Some(_) => None,
            // No value: whichever widget is shown "on".
            None => widget_ids.iter().find_map(|&w| {
                let state = doc.get_dictionary(w).ok()?.get(b"AS").ok()?.as_name().ok()?;
                (state != b"Off").then(|| String::from_utf8_lossy(state).into_owned())
            }),
        };
        FieldValue::On(on)
    } else {
        let v = match &attrs.v {
            Some(Object::Array(values)) => values.first().and_then(|o| text(doc, o)),
            Some(o) => text(doc, o),
            None => None,
        };
        FieldValue::Text(v.unwrap_or_default())
    };
    Some(Field {
        id,
        name: attrs.name.join("."),
        kind,
        widgets,
        value,
        max_len: attrs.max_len.filter(|&n| n > 0).map(|n| n as usize),
    })
}

fn make_widget(
    doc: &Document,
    id: ObjectId,
    attrs: &Inherited,
    button: bool,
    pages: &[PageGeom],
    page_of: &HashMap<ObjectId, usize>,
) -> Option<Widget> {
    let dict = doc.get_dictionary(id).ok()?;
    let flags = dict.get(b"F").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0);
    if flags & (ANNOT_HIDDEN | ANNOT_NO_VIEW) != 0 {
        return None;
    }
    let page = *page_of.get(&id)?;
    let geom = pages.get(page)?;
    let rect = numbers(doc, dict.get(b"Rect").ok()?);
    let [x0, y0, x1, y1] = <[f32; 4]>::try_from(rect.get(..4)?).ok()?;
    let (x0, x1, y0, y1) = (x0.min(x1), x0.max(x1), y0.min(y1), y0.max(y1));
    if x1 - x0 < 1. || y1 - y0 < 1. {
        return None;
    }
    // The rect in view space.
    let to_view = Affine::new(geom.view_to_user).inverse();
    let corners = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)].map(|(x, y)| to_view * Point::new(x as f64, y as f64));
    let (vx0, vx1) = corners.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.x), b.max(p.x)));
    let (vy0, vy1) = corners.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.y), b.max(p.y)));

    let mk = dict.get(b"MK").ok().and_then(|o| resolve_dict(doc, o));
    let rotation = mk
        .as_ref()
        .and_then(|mk| mk.get(b"R").ok().and_then(|o| o.as_i64().ok()))
        .map(|r| (r.rem_euclid(360) / 90 * 90) as u16)
        .unwrap_or(0);
    let (w, h) = (x1 - x0, y1 - y0);
    let size = if rotation % 180 == 90 { (h, w) } else { (w, h) };
    // The content's x axis, turned by R, as it points on screen.
    let r = (rotation as f64).to_radians();
    let axis = to_view * Point::new(r.cos(), r.sin()) - to_view * Point::ZERO;
    let upright = axis.x > 0. && axis.y.abs() < axis.x * 1e-3;
    let color = |key: &[u8]| mk.as_ref().and_then(|mk| mk.get(key).ok()).and_then(|o| rgb(&numbers(doc, o)));
    let border = color(b"BC");
    let border_width = dict
        .get(b"BS")
        .ok()
        .and_then(|o| resolve_dict(doc, o))
        .and_then(|bs| bs.get(b"W").ok().map(|w| numbers(doc, w)))
        .and_then(|w| w.first().copied())
        .unwrap_or(1.);
    let da = dict.get(b"DA").ok().and_then(|o| text(doc, o)).or_else(|| attrs.da.clone()).unwrap_or_default();
    let (font_size, text_color) = parse_da(&da);
    let q = dict.get(b"Q").ok().and_then(|o| o.as_i64().ok()).or(attrs.q).unwrap_or(0);

    let on_state = button.then(|| on_state(doc, dict)).flatten();
    let caption = mk
        .as_ref()
        .and_then(|mk| mk.get(b"CA").ok())
        .and_then(|o| o.as_str().ok().map(<[u8]>::to_vec));
    let radio = attrs.ff & FF_RADIO != 0;
    let symbol = match caption.as_deref().and_then(|c| c.first()) {
        Some(b'4') => Symbol::Check,
        Some(b'l') => Symbol::Circle,
        Some(b'8') => Symbol::Cross,
        Some(b'n') => Symbol::Square,
        Some(b'u') => Symbol::Diamond,
        _ if radio => Symbol::Circle,
        _ => Symbol::Check,
    };
    Some(Widget {
        id,
        page,
        rect: [vx0 as f32, vy0 as f32, (vx1 - vx0) as f32, (vy1 - vy0) as f32],
        size,
        rotation,
        upright,
        on_state: if button { Some(on_state.unwrap_or_else(|| "Yes".into())) } else { None },
        font_size,
        align: match q {
            1 => Align::Center,
            2 => Align::Right,
            _ => Align::Left,
        },
        text_color,
        background: color(b"BG"),
        border_width: if border.is_some() { border_width.clamp(0., 12.) } else { 0. },
        border,
        symbol,
    })
}

/// The name of a button widget's "on" appearance.
fn on_state(doc: &Document, widget: &Dictionary) -> Option<String> {
    let normal = widget
        .get(b"AP")
        .ok()
        .and_then(|ap| resolve_dict(doc, ap))
        .and_then(|ap| ap.get(b"N").ok().and_then(|n| resolve_dict(doc, n)));
    if let Some(normal) = normal {
        if let Some((name, _)) = normal.iter().find(|(name, _)| name.as_slice() != b"Off") {
            return Some(String::from_utf8_lossy(name).into_owned());
        }
    }
    let state = widget.get(b"AS").ok()?.as_name().ok()?;
    (state != b"Off").then(|| String::from_utf8_lossy(state).into_owned())
}

fn options(doc: &Document, opt: Option<&Object>) -> Vec<ChoiceOption> {
    let Some(Object::Array(items)) = opt else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| match resolve(doc, item)? {
            Object::Array(pair) if pair.len() >= 2 => Some(ChoiceOption {
                value: text(doc, &pair[0])?,
                label: text(doc, &pair[1])?,
            }),
            other => text(doc, &other).map(|t| ChoiceOption { value: t.clone(), label: t }),
        })
        .collect()
}

/// Font size and colour from a default appearance string such as "/Helv 0 Tf 0 g".
fn parse_da(da: &str) -> (f32, [f32; 3]) {
    let tokens: Vec<&str> = da.split_whitespace().collect();
    let num = |i: usize| tokens.get(i).and_then(|t| t.parse::<f32>().ok());
    let (mut size, mut color) = (0., [0.; 3]);
    for (i, &token) in tokens.iter().enumerate() {
        match token {
            "Tf" if i >= 1 => size = num(i - 1).unwrap_or(0.),
            "g" if i >= 1 => {
                if let Some(g) = num(i - 1) {
                    color = [g; 3];
                }
            }
            "rg" if i >= 3 => {
                if let (Some(r), Some(g), Some(b)) = (num(i - 3), num(i - 2), num(i - 1)) {
                    color = [r, g, b];
                }
            }
            "k" if i >= 4 => {
                if let (Some(c), Some(m), Some(y), Some(k)) = (num(i - 4), num(i - 3), num(i - 2), num(i - 1)) {
                    color = [(1. - c) * (1. - k), (1. - m) * (1. - k), (1. - y) * (1. - k)];
                }
            }
            _ => {}
        }
    }
    (size.max(0.), color.map(|c| c.clamp(0., 1.)))
}

/// A colour from 1 (gray), 3 (RGB) or 4 (CMYK) components; none means transparent.
fn rgb(c: &[f32]) -> Option<[f32; 3]> {
    match *c {
        [g] => Some([g; 3]),
        [r, g, b] => Some([r, g, b]),
        [c, m, y, k] => Some([(1. - c) * (1. - k), (1. - m) * (1. - k), (1. - y) * (1. - k)]),
        _ => None,
    }
}

fn resolve(doc: &Document, obj: &Object) -> Option<Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok().cloned(),
        other => Some(other.clone()),
    }
}

fn resolve_dict(doc: &Document, obj: &Object) -> Option<Dictionary> {
    match resolve(doc, obj)? {
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

fn text(doc: &Document, obj: &Object) -> Option<String> {
    match resolve(doc, obj)? {
        s @ Object::String(..) => lopdf::decode_text_string(&s).ok(),
        Object::Name(n) => Some(String::from_utf8_lossy(&n).into_owned()),
        _ => None,
    }
}

fn numbers(doc: &Document, obj: &Object) -> Vec<f32> {
    match resolve(doc, obj) {
        Some(Object::Array(items)) => items
            .iter()
            .filter_map(|o| match resolve(doc, o)? {
                Object::Integer(i) => Some(i as f32),
                Object::Real(r) => Some(r),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Space between a field's border and its text.
const PADDING: f32 = 2.;
/// Font size used when the field says "fit the field" and there's room.
const AUTO_MAX: f32 = 12.;
const AUTO_MIN: f32 = 4.;
/// Background of the selected line in a list box.
pub const LIST_HIGHLIGHT: [f32; 3] = [0.6, 0.75, 0.86];

/// A piece of text placed in a widget, in the widget's frame: points from its
/// top-left corner, y down.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    pub x: f32,
    pub baseline: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextLayout {
    pub size: f32,
    pub runs: Vec<Run>,
    /// A highlighted band (top, height) behind the selected line of a list box.
    pub highlight: Option<(f32, f32)>,
    /// The inner box the text is clipped to: x, y, width, height.
    pub clip: [f32; 4],
    /// Where typing continues: x and baseline.
    pub caret: (f32, f32),
}

/// Lay out a text or choice field's value in one of its widgets.
///
/// `measure(text, size)` gives the width of `text` in points at font size
/// `size`; `baseline_ratio` places the baseline in a line box (see
/// `TextMetrics`). The editor measures with its UI font and the writer with
/// the PDF fonts; they have the same metrics, so both agree.
pub fn layout(
    field: &Field,
    widget: &Widget,
    value: &FieldValue,
    baseline_ratio: f32,
    measure: &mut dyn FnMut(&str, f32) -> f32,
) -> TextLayout {
    let (w, h) = widget.size;
    let inset = widget.border_width + PADDING;
    let clip = [widget.border_width, widget.border_width, (w - 2. * widget.border_width).max(0.), (h - 2. * widget.border_width).max(0.)];
    let inner_w = (w - 2. * inset).max(1.);
    let line_box = |size: f32| size * LINE_HEIGHT;
    // Baseline of a single line centred vertically.
    let centred = |size: f32| (h - line_box(size)) / 2. + baseline_ratio * size;
    let aligned = |width: f32| match widget.align {
        Align::Left => inset,
        Align::Center => (w - width) / 2.,
        Align::Right => w - inset - width,
    };
    let mut out = TextLayout { clip, ..TextLayout::default() };

    match &field.kind {
        FieldKind::Text { comb: true, .. } => {
            let text = field.display_text(value);
            let cells = field.max_len.unwrap_or(1).max(1);
            let cell = w / cells as f32;
            let size = if widget.font_size > 0. { widget.font_size } else { ((h - 2. * inset) / LINE_HEIGHT).clamp(AUTO_MIN, AUTO_MAX) };
            out.size = size;
            for (i, c) in text.chars().take(cells).enumerate() {
                let s = c.to_string();
                let x = i as f32 * cell + (cell - measure(&s, size)) / 2.;
                out.runs.push(Run { text: s, x, baseline: centred(size) });
            }
            let next = text.chars().count().min(cells - 1) as f32;
            out.caret = (next * cell + cell / 2., centred(size));
        }
        FieldKind::Text { multiline: true, .. } => {
            let text = field.display_text(value);
            let fits = |size: f32, measure: &mut dyn FnMut(&str, f32) -> f32| {
                let lines = wrap(&text, inner_w, size, measure);
                (lines.len() as f32 * line_box(size) <= h - 2. * inset, lines)
            };
            let (size, lines) = if widget.font_size > 0. {
                (widget.font_size, wrap(&text, inner_w, widget.font_size, measure))
            } else {
                // Shrink until it fits, down to a legible minimum.
                let mut size = AUTO_MAX;
                loop {
                    let (ok, lines) = fits(size, measure);
                    if ok || size <= AUTO_MIN {
                        break (size, lines);
                    }
                    size = (size - 0.5).max(AUTO_MIN);
                }
            };
            out.size = size;
            for (i, line) in lines.into_iter().enumerate() {
                let width = measure(&line, size);
                let baseline = inset + i as f32 * line_box(size) + baseline_ratio * size;
                out.runs.push(Run { x: aligned(width), text: line, baseline });
            }
        }
        FieldKind::Choice { combo: false, options, .. } => {
            // A list box: every option, the selected one highlighted.
            let ListRows { size, row, first, visible, index } = list_rows(field, widget, value);
            out.size = size;
            for (row_index, (i, option)) in options.iter().enumerate().skip(first).take(visible).enumerate() {
                let top = widget.border_width + row_index as f32 * row;
                if Some(i) == index {
                    out.highlight = Some((top, row));
                }
                let width = measure(&option.label, size);
                out.runs.push(Run { x: aligned(width), text: option.label.clone(), baseline: top + baseline_ratio * size });
            }
        }
        // Single-line text, and combo boxes.
        _ => {
            let text = field.display_text(value);
            let size = if widget.font_size > 0. {
                widget.font_size
            } else {
                let by_height = ((h - 2. * inset) / LINE_HEIGHT).clamp(AUTO_MIN, AUTO_MAX);
                let width = measure(&text, by_height);
                if width > inner_w { (by_height * inner_w / width).max(AUTO_MIN) } else { by_height }
            };
            out.size = size;
            out.caret = (aligned(0.), centred(size));
            if !text.is_empty() {
                let width = measure(&text, size);
                out.runs.push(Run { x: aligned(width), text, baseline: centred(size) });
            }
        }
    }
    if let (Some(last), FieldKind::Text { comb: false, .. } | FieldKind::Choice { combo: true, .. }) = (out.runs.last(), &field.kind) {
        out.caret = (last.x + measure(&last.text, out.size), last.baseline);
    }
    out
}

/// The visible rows of a list box.
struct ListRows {
    size: f32,
    row: f32,
    /// The first option shown, and how many fit.
    first: usize,
    visible: usize,
    /// The selected option.
    index: Option<usize>,
}

fn list_rows(field: &Field, widget: &Widget, value: &FieldValue) -> ListRows {
    let size = if widget.font_size > 0. { widget.font_size } else { AUTO_MAX };
    let row = size * LINE_HEIGHT;
    let visible = (((widget.size.1 - 2. * widget.border_width) / row).floor() as usize).max(1);
    let index = match (&field.kind, value) {
        (FieldKind::Choice { options, .. }, FieldValue::Text(v)) => options.iter().position(|o| o.value == *v),
        _ => None,
    };
    // Scrolled so the selected option shows.
    let first = index.map_or(0, |i| i.saturating_sub(visible - 1));
    ListRows { size, row, first, visible, index }
}

/// The option of a list box shown at `y` (points down from the widget's top).
pub fn list_option_at(field: &Field, widget: &Widget, value: &FieldValue, y: f32) -> Option<usize> {
    let FieldKind::Choice { combo: false, options, .. } = &field.kind else { return None };
    let rows = list_rows(field, widget, value);
    let i = rows.first + ((y - widget.border_width) / rows.row).floor().max(0.) as usize;
    (i < options.len() && i < rows.first + rows.visible).then_some(i)
}

/// Break text into lines no wider than `width` (at `size`), at spaces where
/// possible. Explicit line breaks are kept.
fn wrap(text: &str, width: f32, size: f32, measure: &mut dyn FnMut(&str, f32) -> f32) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_inclusive(' ') {
            let candidate = format!("{line}{word}");
            if line.is_empty() || measure(candidate.trim_end(), size) <= width {
                line = candidate;
                // A single word wider than the field is broken by character.
                if measure(line.trim_end(), size) > width {
                    let mut piece = String::new();
                    for c in line.chars() {
                        let next = format!("{piece}{c}");
                        if !piece.is_empty() && measure(&next, size) > width {
                            lines.push(std::mem::take(&mut piece));
                            piece.push(c);
                        } else {
                            piece = next;
                        }
                    }
                    line = piece;
                }
            } else {
                lines.push(line.trim_end().to_string());
                line = word.to_string();
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every character 0.5 em wide.
    fn measure(text: &str, size: f32) -> f32 {
        text.chars().count() as f32 * size * 0.5
    }

    fn widget(w: f32, h: f32) -> Widget {
        Widget {
            id: (1, 0),
            page: 0,
            rect: [0., 0., w, h],
            size: (w, h),
            rotation: 0,
            upright: true,
            on_state: None,
            font_size: 10.,
            align: Align::Left,
            text_color: [0.; 3],
            background: None,
            border: None,
            border_width: 0.,
            symbol: Symbol::Check,
        }
    }

    fn text_field(multiline: bool, comb: bool, max_len: Option<usize>) -> Field {
        Field {
            id: (1, 0),
            name: "f".into(),
            kind: FieldKind::Text { multiline, comb },
            widgets: vec![],
            value: FieldValue::Text(String::new()),
            max_len,
        }
    }

    #[test]
    fn default_appearance_strings_give_size_and_colour() {
        assert_eq!(parse_da("/Helv 0 Tf 0 g"), (0., [0., 0., 0.]));
        assert_eq!(parse_da("/TiRo 11.5 Tf 1 0 0 rg"), (11.5, [1., 0., 0.]));
        assert_eq!(parse_da("0 0 0 1 k /F1 9 Tf"), (9., [0., 0., 0.]));
        assert_eq!(parse_da(""), (0., [0., 0., 0.]));
    }

    #[test]
    fn single_line_text_is_centred_vertically_and_aligned() {
        let field = text_field(false, false, None);
        let mut w = widget(100., 20.);
        let value = FieldValue::Text("abcd".into());
        let l = layout(&field, &w, &value, 0.87, &mut measure);
        assert_eq!(l.runs.len(), 1);
        assert_eq!(l.runs[0].x, 2.);
        assert!((l.runs[0].baseline - ((20. - 12.) / 2. + 8.7)).abs() < 1e-4);
        w.align = Align::Right;
        let l = layout(&field, &w, &value, 0.87, &mut measure);
        assert_eq!(l.runs[0].x, 100. - 2. - 20.);
    }

    #[test]
    fn auto_size_shrinks_long_text_to_fit() {
        let field = text_field(false, false, None);
        let mut w = widget(50., 20.);
        w.font_size = 0.;
        let l = layout(&field, &w, &FieldValue::Text("a fairly long value".into()), 0.87, &mut measure);
        assert!(l.size < AUTO_MAX);
        assert!(measure(&l.runs[0].text, l.size) <= 46. + 1e-3);
    }

    #[test]
    fn multiline_text_wraps_at_spaces() {
        let field = text_field(true, false, None);
        let w = widget(54., 100.); // 50pt inside: 10 characters at 10pt
        let l = layout(&field, &w, &FieldValue::Text("one two three four\nfive".into()), 0.87, &mut measure);
        let lines: Vec<&str> = l.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(lines, ["one two", "three four", "five"]);
        assert!(l.runs[1].baseline > l.runs[0].baseline);
    }

    #[test]
    fn comb_fields_put_one_character_per_cell() {
        let field = text_field(false, true, Some(4));
        let w = widget(80., 20.);
        let l = layout(&field, &w, &FieldValue::Text("1234".into()), 0.87, &mut measure);
        let xs: Vec<f32> = l.runs.iter().map(|r| r.x).collect();
        assert_eq!(xs, [7.5, 27.5, 47.5, 67.5]);
    }

    #[test]
    fn values_are_cut_to_what_the_field_takes() {
        assert_eq!(text_field(false, false, Some(3)).accept("abcdef"), "abc");
        assert_eq!(text_field(false, false, None).accept("a\nb"), "a b");
        assert_eq!(text_field(true, false, None).accept("a\nb"), "a\nb");
    }
}
