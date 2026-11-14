//! Theme colours and small reusable UI components.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    AnyView, App, BoxShadow, Context, CursorStyle, Div, ElementId, FontWeight, Hsla, IntoElement, MouseButton, Render,
    SharedString, Stateful, Svg, Window, WindowAppearance, div, point, prelude::*, px, rgb, rgba, svg,
};

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------
//
// Colours follow the window's appearance (System Settings → Appearance). The
// editor calls `set_appearance` at the top of every render and re-renders when
// the appearance changes, so every colour helper below always reflects the
// current mode. (A process-wide flag is fine: the app has a single window.)

static DARK: AtomicBool = AtomicBool::new(false);

pub fn set_appearance(appearance: WindowAppearance) {
    let dark = matches!(appearance, WindowAppearance::Dark | WindowAppearance::VibrantDark);
    DARK.store(dark, Ordering::Relaxed);
}

pub fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

/// Pick a `0xRRGGBBAA` colour for the current mode.
fn pick(light: u32, dark: u32) -> Hsla {
    rgba(if is_dark() { dark } else { light }).into()
}

pub fn accent() -> Hsla {
    pick(0x2563ebff, 0x3b82f6ff)
}
pub fn accent_hover() -> Hsla {
    pick(0x1d4ed8ff, 0x5b95f8ff)
}
pub fn accent_soft() -> Hsla {
    pick(0xe8efffff, 0x1e2b47ff)
}
pub fn danger() -> Hsla {
    pick(0xe5484dff, 0xff6369ff)
}
pub fn warning() -> Hsla {
    pick(0xf5a524ff, 0xf7b955ff)
}
pub fn text_primary() -> Hsla {
    pick(0x1d1d1fff, 0xf2f2f7ff)
}
pub fn text_secondary() -> Hsla {
    pick(0x6e6e73ff, 0xa8a8b0ff)
}
pub fn text_tertiary() -> Hsla {
    pick(0xa1a1a6ff, 0x6c6c74ff)
}
pub fn chrome_bg() -> Hsla {
    pick(0xf5f5f7ff, 0x242428ff)
}
pub fn sidebar_bg() -> Hsla {
    pick(0xfafafbff, 0x1d1d20ff)
}
pub fn canvas_bg() -> Hsla {
    pick(0xe6e7ebff, 0x131315ff)
}
/// Raised controls: buttons, cards, chips.
pub fn surface() -> Hsla {
    pick(0xffffffff, 0x2e2e33ff)
}
/// Neutral fill for badges and the segmented control track.
pub fn muted_bg() -> Hsla {
    pick(0xe8e8edff, 0x2e2e33ff)
}
/// The "paper" behind signature thumbnails. Ink is dark, so this stays light in dark mode.
pub fn paper() -> Hsla {
    pick(0xffffffff, 0xf4f4f5ff)
}
pub fn border() -> Hsla {
    pick(0xe2e2e7ff, 0x34343aff)
}
pub fn border_strong() -> Hsla {
    pick(0xd2d2d8ff, 0x46464dff)
}
pub fn hover_bg() -> Hsla {
    pick(0x0000000d, 0xffffff14)
}
pub fn pressed_bg() -> Hsla {
    pick(0x00000017, 0xffffff24)
}
/// `surface` under the pointer, and pressed. Opaque, unlike `hover_bg`, for
/// controls that float over other content.
pub fn surface_hover() -> Hsla {
    pick(0xf0f0f3ff, 0x3a3a41ff)
}
pub fn surface_pressed() -> Hsla {
    pick(0xe4e4e9ff, 0x46464dff)
}
/// Floating dark HUDs (tooltips, field toolbar, tool hint).
pub fn hud_bg() -> Hsla {
    pick(0x1f1f23ff, 0x3a3a41ff)
}
pub fn hud_text() -> Hsla {
    rgb(0xd8d8de).into()
}
pub fn hud_muted() -> Hsla {
    rgb(0x9a9aa2).into()
}

fn shadow(alpha_light: u32, alpha_dark: u32, y: f32, blur: f32, spread: f32) -> BoxShadow {
    BoxShadow {
        color: pick(alpha_light, alpha_dark),
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(spread),
    }
}

pub fn page_shadow() -> Vec<BoxShadow> {
    vec![
        shadow(0x0000001a, 0x00000066, 1., 3., 0.),
        shadow(0x0000001f, 0x00000080, 10., 30., -6.),
    ]
}

pub fn card_shadow() -> Vec<BoxShadow> {
    vec![shadow(0x0000000f, 0x00000040, 1., 2., 0.)]
}

pub fn lifted_shadow() -> Vec<BoxShadow> {
    vec![
        shadow(0x00000014, 0x00000059, 2., 4., 0.),
        shadow(0x0000001a, 0x00000073, 8., 20., -4.),
    ]
}

/// A 3px accent ring drawn with a shadow, so it doesn't affect layout.
pub fn ring() -> Vec<BoxShadow> {
    let mut shadows = page_shadow();
    shadows.push(BoxShadow {
        color: accent(),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(3.),
    });
    shadows
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

/// An icon from `assets.rs`. GPUI SVGs don't inherit the parent's text colour,
/// so the colour must be given here.
pub fn icon(name: &str, color: impl Into<Hsla>) -> Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(16.))
        .flex_none()
        .text_color(color.into())
}

struct Tooltip {
    text: SharedString,
    shortcut: Option<SharedString>,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .font_family(".SystemUIFont")
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(hud_bg())
            .text_color(gpui::white())
            .text_xs()
            .shadow(lifted_shadow())
            .child(self.text.clone())
            .when_some(self.shortcut.clone(), |d, s| d.child(div().text_color(hud_muted()).child(s)))
    }
}

pub fn tooltip(
    text: impl Into<SharedString>,
    shortcut: Option<&'static str>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| {
        cx.new(|_| Tooltip {
            text: text.clone(),
            shortcut: shortcut.map(keys),
        })
        .into()
    }
}

/// A clickable element whose presses stay with it: without this, a mouse-down
/// also reaches the parents, so e.g. two quick clicks on a title-bar button
/// would count as a title-bar double-click (which zooms the window).
pub fn button_base(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// A square, borderless icon button (toolbar style).
pub fn icon_button(id: impl Into<ElementId>, icon_name: &str) -> Stateful<Div> {
    icon_button_tinted(id, icon_name, text_primary())
}

/// Like [`icon_button`], with a custom icon colour on hover (e.g. red for delete).
pub fn icon_button_tinted(id: impl Into<ElementId>, icon_name: &str, hover_color: Hsla) -> Stateful<Div> {
    icon_button_on(id, icon_name, hover_color, hover_bg(), pressed_bg())
}

/// An icon button floating over other content (e.g. on a thumbnail). Its
/// background stays opaque on hover, so it never fades into what's behind it.
pub fn floating_icon_button(id: impl Into<ElementId>, icon_name: &str, hover_color: Hsla) -> Stateful<Div> {
    icon_button_on(id, icon_name, hover_color, surface_hover(), surface_pressed())
        .bg(surface())
        .border_1()
        .border_color(border())
        .shadow(card_shadow())
}

fn icon_button_on(id: impl Into<ElementId>, icon_name: &str, hover_color: Hsla, hover: Hsla, pressed: Hsla) -> Stateful<Div> {
    button_base(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(30.))
        .rounded_md()
        .group("icon-btn")
        .cursor(CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .active(move |s| s.bg(pressed))
        .child(icon(icon_name, text_secondary()).group_hover("icon-btn", move |s| s.text_color(hover_color)))
}

/// A filled accent button.
pub fn primary_button(id: impl Into<ElementId>, icon_name: &str, label: &'static str) -> Stateful<Div> {
    button_base(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_1p5()
        .h(px(30.))
        .px_3()
        .rounded_md()
        .bg(accent())
        .text_color(gpui::white())
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .shadow(card_shadow())
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(accent_hover()))
        .child(icon(icon_name, gpui::white()).size(px(15.)))
        .child(label)
}

/// A white, bordered button.
pub fn secondary_button(id: impl Into<ElementId>, icon_name: &str, label: &'static str) -> Stateful<Div> {
    button_base(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_1p5()
        .h(px(30.))
        .px_3()
        .rounded_md()
        .bg(surface())
        .border_1()
        .border_color(border_strong())
        .text_color(text_primary())
        .text_sm()
        .shadow(card_shadow())
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(hover_bg()))
        .child(icon(icon_name, text_secondary()).size(px(15.)))
        .child(label)
}

/// A keyboard shortcut chip, e.g. "⌘O".
/// A shortcut label written the macOS way ("⇧⌘Z", "⌘C  ⌘V") in the running
/// platform's convention ("Ctrl+Shift+Z" on Windows and Linux).
pub fn keys(mac: &str) -> SharedString {
    keys_for(mac, cfg!(target_os = "macos"))
}

fn keys_for(mac: &str, is_mac: bool) -> SharedString {
    if is_mac {
        return SharedString::from(mac.to_string());
    }
    mac.split(' ')
        .map(|token| {
            let mut mods = Vec::new();
            let mut key = String::new();
            for c in token.chars() {
                match c {
                    '⌘' => mods.push((0, "Ctrl")),
                    '⌥' => mods.push((1, "Alt")),
                    '⇧' => mods.push((2, "Shift")),
                    '⌫' => key.push_str("Delete"),
                    c => key.push(c),
                }
            }
            mods.sort();
            mods.into_iter().map(|(_, m)| m).chain((!key.is_empty()).then_some(key.as_str())).collect::<Vec<_>>().join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
        .into()
}

pub fn kbd(keys: &'static str) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .rounded(px(4.))
        .border_1()
        .border_color(border_strong())
        .bg(surface())
        .text_color(text_secondary())
        .text_xs()
        .child(self::keys(keys))
}

pub fn vertical_divider() -> Div {
    div().flex_none().w(px(1.)).h(px(18.)).mx_1().bg(border_strong())
}

/// A button inside the dark floating HUD shown above a selected field.
pub fn hud_button(id: impl Into<ElementId>, icon_name: &str) -> Stateful<Div> {
    hud_button_tinted(id, icon_name, gpui::white())
}

pub fn hud_button_tinted(id: impl Into<ElementId>, icon_name: &str, hover_color: Hsla) -> Stateful<Div> {
    button_base(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(26.))
        .rounded(px(5.))
        .group("hud-btn")
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(rgba(0xffffff1f)))
        .child(
            icon(icon_name, hud_text())
                .size(px(15.))
                .group_hover("hud-btn", move |s| s.text_color(hover_color)),
        )
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

/// The floating panel of a dropdown menu.
pub fn menu_panel() -> Div {
    div()
        .font_family(".SystemUIFont")
        .flex()
        .flex_col()
        .min_w(px(190.))
        .p(px(4.))
        .rounded_lg()
        .bg(surface())
        .border_1()
        .border_color(border())
        .shadow(lifted_shadow())
        .text_color(text_primary())
}

pub fn menu_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    shortcut: Option<&'static str>,
) -> Stateful<Div> {
    button_base(id)
        .flex()
        .items_center()
        .gap_2()
        .h(px(28.))
        .px_2()
        .rounded(px(5.))
        .text_sm()
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(hover_bg()))
        .child(
            div()
                .flex()
                .flex_none()
                .w(px(14.))
                .when(checked, |d| d.child(icon("check", accent()).size(px(14.)))),
        )
        .child(div().flex_1().child(label.into()))
        .when_some(shortcut, |d, s| d.child(div().text_xs().text_color(text_tertiary()).child(keys(s))))
}

pub fn menu_separator() -> Div {
    div().h(px(1.)).mx_1().my_1().bg(border())
}

/// One option in a small row of choices (e.g. inside a menu).
pub fn chip(id: impl Into<ElementId>, label: &'static str, active: bool) -> Stateful<Div> {
    button_base(id)
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .h(px(24.))
        .px_2()
        .rounded(px(5.))
        .text_xs()
        .cursor(CursorStyle::PointingHand)
        .when(active, |d| d.bg(accent_soft()).text_color(accent()).font_weight(FontWeight::MEDIUM))
        .when(!active, |d| d.text_color(text_secondary()).hover(|s| s.bg(hover_bg()).text_color(text_primary())))
        .child(label)
}

/// A small caption above a group of options in a menu.
pub fn menu_caption(text: &'static str) -> Div {
    div().px_2().pt_1p5().pb_1().text_xs().text_color(text_tertiary()).child(text)
}

#[cfg(test)]
mod tests {
    use super::keys_for;

    #[test]
    fn shortcut_labels_follow_the_platform() {
        assert_eq!(keys_for("⇧⌘Z", true).as_ref(), "⇧⌘Z");
        assert_eq!(keys_for("⇧⌘Z", false).as_ref(), "Ctrl+Shift+Z");
        assert_eq!(keys_for("⌘C  ⌘V", false).as_ref(), "Ctrl+C  Ctrl+V");
        assert_eq!(keys_for("⌘−", false).as_ref(), "Ctrl+−");
        assert_eq!(keys_for("⌫", false).as_ref(), "Delete");
        assert_eq!(keys_for("←↑→↓", false).as_ref(), "←↑→↓");
    }
}
