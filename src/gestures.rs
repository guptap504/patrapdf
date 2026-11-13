//! Trackpad pinch-to-zoom.
//!
//! GPUI 0.2 has no gesture events: its macOS view (`GPUIView`) doesn't
//! implement `magnifyWithEvent:`, so AppKit drops pinches. We add that method
//! to the class through the Objective-C runtime and forward each pinch step
//! over a channel, which the editor drains on the main thread.

use std::sync::Mutex;

use futures::channel::mpsc;

/// One step of a pinch gesture.
pub struct Pinch {
    /// Multiply the zoom by this (> 1 when spreading the fingers).
    pub factor: f32,
    /// Pointer position in window coordinates (points, top-left origin).
    pub position: (f32, f32),
}

static SENDER: Mutex<Option<mpsc::UnboundedSender<Pinch>>> = Mutex::new(None);

/// Start receiving pinch gestures. A later call replaces the earlier receiver
/// (the app has a single window).
pub fn subscribe() -> mpsc::UnboundedReceiver<Pinch> {
    let (tx, rx) = mpsc::unbounded();
    *SENDER.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    #[cfg(target_os = "macos")]
    mac::install();
    rx
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn send(pinch: Pinch) {
    if let Some(tx) = SENDER.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.unbounded_send(pinch);
    }
}

// objc 0.2's macros check a `cargo-clippy` feature that this crate doesn't declare.
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
mod mac {
    use std::sync::Once;

    use objc::runtime::{Class, Imp, Object, Sel, class_addMethod};
    use objc::{msg_send, sel, sel_impl};

    use super::{Pinch, send};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSRect {
        origin: NSPoint,
        size: NSPoint,
    }

    extern "C" fn magnify_with_event(this: &Object, _: Sel, event: *mut Object) {
        // SAFETY: AppKit calls this with a live NSView (`this`) and NSEvent of type magnify.
        unsafe {
            let magnification: f64 = msg_send![event, magnification];
            let location: NSPoint = msg_send![event, locationInWindow];
            let frame: NSRect = msg_send![this, frame];
            if magnification.is_finite() {
                send(Pinch {
                    factor: (1.0 + magnification).max(0.1) as f32,
                    // AppKit's origin is bottom-left; GPUI's is top-left.
                    position: (location.x as f32, (frame.size.y - location.y) as f32),
                });
            }
        }
    }

    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let Some(class) = Class::get("GPUIView") else { return };
            // SAFETY: the type encoding "v@:@" matches `magnify_with_event`'s signature
            // (void return; self, _cmd, one object argument). class_addMethod leaves the
            // class untouched if GPUI ever implements the method itself.
            unsafe {
                let imp: Imp = std::mem::transmute(magnify_with_event as extern "C" fn(&Object, Sel, *mut Object));
                class_addMethod(
                    class as *const Class as *mut Class,
                    sel!(magnifyWithEvent:),
                    imp,
                    c"v@:@".as_ptr(),
                );
            }
        });
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use objc::runtime::{Class, Sel};

    #[test]
    fn pinch_handler_is_installed_on_gpuis_view() {
        let _rx = super::subscribe();
        let class = Class::get("GPUIView").expect("GPUI registers its view class at load time");
        assert!(class.instance_method(Sel::register("magnifyWithEvent:")).is_some());
    }
}
