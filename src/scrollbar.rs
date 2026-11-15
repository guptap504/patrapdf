//! Scroll bars. GPUI scrolls a `div` that overflows, but draws no bar for it:
//! these are drawn over the edge of one, from its `ScrollHandle`.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Along, Axis, BorderStyle, Bounds, CursorStyle, DispatchPhase, EntityId, HitboxBehavior, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, ScrollHandle, Styled, canvas, point, px, quad, size,
};

use crate::ui;

/// The strip along the edge that takes the pointer, and the bar drawn in the
/// middle of it.
pub const STRIP: f32 = 12.;
const BAR: f32 = 6.;
/// A bar is never shorter than this, however much it scrolls.
const MIN_LENGTH: f32 = 28.;

/// The bar being dragged, if one is. Shared by all the bars of a window.
pub type Held = Rc<Cell<Option<Grip>>>;

#[derive(Clone, Copy)]
pub struct Grip {
    bar: (&'static str, Axis),
    /// How far along the bar the pointer has hold of it.
    at: Pixels,
}

/// Where a bar is in the `room` it has to move in: how far from the start, and
/// how long. `viewport` is how much can be seen of what is scrolled, `max` how
/// far that scrolls, and `scrolled` how far it has (0 to `max`).
fn span(room: f32, viewport: f32, max: f32, scrolled: f32) -> (f32, f32) {
    let length = (room * viewport / (viewport + max)).max(MIN_LENGTH).min(room);
    ((room - length) * (scrolled / max).clamp(0., 1.), length)
}

/// A bar as it is drawn.
#[derive(Clone, Copy)]
struct Bar {
    axis: Axis,
    /// The part of the strip that takes the pointer.
    track: Bounds<Pixels>,
    bounds: Bounds<Pixels>,
    /// Where the bar starts when nothing is scrolled, and how far it moves.
    start: Pixels,
    travel: Pixels,
    /// How far there is to scroll.
    max: Pixels,
}

impl Bar {
    /// The bar for what `handle` scrolls, in `strip`. None when there is
    /// nothing to scroll. `crossed`: the other axis may have a bar too, which
    /// this one then stops short of.
    fn new(strip: Bounds<Pixels>, axis: Axis, crossed: bool, handle: &ScrollHandle) -> Option<Self> {
        let (max, viewport) = (handle.max_offset(), handle.bounds().size);
        if max.along(axis) < px(1.) || viewport.along(axis) <= px(0.) {
            return None;
        }
        let inset = px((STRIP - BAR) / 2.);
        let corner = if crossed && max.along(axis.invert()) >= px(1.) { px(STRIP) } else { px(0.) };
        let start = strip.origin.along(axis) + inset;
        let room = strip.size.along(axis) - corner - inset * 2.;
        if room <= px(0.) {
            return None;
        }
        let (from, length) =
            span(room / px(1.), viewport.along(axis) / px(1.), max.along(axis) / px(1.), -handle.offset().along(axis) / px(1.));
        let across = strip.origin.along(axis.invert()) + inset;
        let (along, length) = (start + px(from), px(length));
        let (bounds, track) = match axis {
            Axis::Vertical => (
                Bounds::new(point(across, along), size(px(BAR), length)),
                Bounds::new(strip.origin, size(strip.size.width, strip.size.height - corner)),
            ),
            Axis::Horizontal => (
                Bounds::new(point(along, across), size(length, px(BAR))),
                Bounds::new(strip.origin, size(strip.size.width - corner, strip.size.height)),
            ),
        };
        Some(Self { axis, track, bounds, start, travel: room - length, max: max.along(axis) })
    }

    /// Scroll so that the bar starts at `to` (window coordinates), or as near as it goes.
    fn drag_to(&self, to: Pixels, handle: &ScrollHandle) {
        if self.travel <= px(0.) {
            return;
        }
        let scrolled = ((to - self.start) / self.travel).clamp(0., 1.);
        handle.set_offset(handle.offset().apply_along(self.axis, |_| -self.max * scrolled));
    }
}

/// A scroll bar for what `handle` scrolls along `axis`, there only while there
/// is something to scroll. It lies over the right (or bottom) edge of its
/// parent, which is to be `relative` and the size of the scrolling element,
/// and comes after that element among the parent's children. Dragging the bar
/// scrolls; pressing the strip beside it brings the bar to the pointer.
///
/// `id` tells the bars of a window apart, `held` is shared by them, and `view`
/// is drawn again as the bar is dragged. `crossed`: see `Bar::new`.
pub fn scrollbar(id: &'static str, axis: Axis, crossed: bool, handle: &ScrollHandle, held: &Held, view: EntityId) -> impl IntoElement {
    let (measured, handle, held) = (handle.clone(), handle.clone(), held.clone());
    let strip = canvas(
        // What is scrolled has been laid out by now (it comes first), so this
        // is where the bar belongs in the frame being drawn.
        move |strip, window, _| {
            let bar = Bar::new(strip, axis, crossed, &measured)?;
            Some((bar, window.insert_hitbox(bar.track, HitboxBehavior::Normal)))
        },
        move |_, bar, window, _| {
            let Some((bar, hitbox)) = bar else { return };
            let this = (id, axis);
            let hovered = hitbox.is_hovered(window);
            let active = hovered || held.get().is_some_and(|grip| grip.bar == this);
            let edge = px(1.);
            window.paint_quad(quad(
                bar.bounds.dilate(edge),
                px(BAR / 2.) + edge,
                ui::scrollbar(active),
                edge,
                ui::scrollbar_edge(),
                BorderStyle::default(),
            ));
            window.set_cursor_style(CursorStyle::Arrow, &hitbox);

            // Bubbling, so that anything drawn over the strip (a menu, the
            // find bar) gets the press first, and what's under it never does.
            window.on_mouse_event({
                let (handle, held, hitbox) = (handle.clone(), held.clone(), hitbox.clone());
                move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || event.button != MouseButton::Left || !hitbox.is_hovered(window) {
                        return;
                    }
                    let pointer = event.position.along(axis);
                    let (start, length) = (bar.bounds.origin.along(axis), bar.bounds.size.along(axis));
                    let at = if (start..start + length).contains(&pointer) {
                        pointer - start
                    } else {
                        // Beside the bar: its middle comes to the pointer.
                        bar.drag_to(pointer - length / 2., &handle);
                        length / 2.
                    };
                    held.set(Some(Grip { bar: this, at }));
                    cx.stop_propagation();
                    cx.notify(view);
                }
            });
            window.on_mouse_event({
                let (handle, held, hitbox) = (handle.clone(), held.clone(), hitbox.clone());
                move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    match held.get() {
                        // The button was let go where that wasn't seen (outside the window).
                        Some(_) if event.pressed_button != Some(MouseButton::Left) => {
                            held.set(None);
                            cx.notify(view);
                        }
                        Some(grip) if grip.bar == this => {
                            bar.drag_to(event.position.along(axis) - grip.at, &handle);
                            cx.notify(view);
                        }
                        Some(_) => {}
                        None if hitbox.is_hovered(window) != hovered => window.refresh(),
                        None => {}
                    }
                }
            });
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left && held.take().is_some() {
                    cx.notify(view);
                }
            });
        },
    )
    .absolute();
    match axis {
        Axis::Vertical => strip.top_0().bottom_0().right_0().w(px(STRIP)),
        Axis::Horizontal => strip.left_0().right_0().bottom_0().h(px(STRIP)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_shows_how_much_is_in_view_and_where() {
        // A quarter in view: a bar a quarter of the room, moving through the rest.
        assert_eq!(span(400., 500., 1500., 0.), (0., 100.));
        assert_eq!(span(400., 500., 1500., 750.), (150., 100.));
        assert_eq!(span(400., 500., 1500., 1500.), (300., 100.));
        // Never too short to take hold of, nor past the ends of its room.
        assert_eq!(span(400., 500., 99_500., 99_500.), (400. - MIN_LENGTH, MIN_LENGTH));
        assert_eq!(span(400., 500., 1500., 1600.), (300., 100.));
        assert_eq!(span(400., 500., 1500., -20.), (0., 100.));
        assert_eq!(span(20., 500., 1500., 1500.), (0., 20.));
    }
}
