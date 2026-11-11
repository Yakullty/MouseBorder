//! macOS capture layer.
//!
//! rdev's `grab` converts every key press through HIToolbox's input-source APIs
//! (`TISGetInputSourceProperty`), which assert they run on the main queue. A CGEventTap callback
//! runs on the tap thread, so the first physical key press aborted the process with
//! `dispatch_assert_queue_fail`. This tap only reads scalar event fields, so it stays inside the
//! thread-safe subset of CoreGraphics, and it reports device deltas instead of pointer positions.
//! Injection still goes through `rdev::simulate`.
//!
//! The keycode table mirrors rdev's `macos/keycodes.rs` (plus right control and the navigation
//! keys it omits) so both platforms put identical names on the wire.

use crate::input::{self, Captured, HookState};
use rdev::{Button, Key};
use std::{cell::Cell, ffi::c_void, sync::{mpsc::Sender, Arc}};

type Ref = *mut c_void;

const LOCATION_HID: u32 = 0;
const PLACE_HEAD: u32 = 0;
const OPTION_FILTER: u32 = 0;

// CGEventType values subscribed to, plus the two tap notifications.
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOUSE_MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const KEY_DOWN: u32 = 8;
const KEY_UP: u32 = 9;
const FLAGS_CHANGED: u32 = 10;
const SCROLL_WHEEL: u32 = 12;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
const DISABLED_BY_TIMEOUT: u32 = 14;
const DISABLED_BY_USER: u32 = 15;

// CGEventField values, per CGEventFields.h (mirrored by core-graphics' EventField).
// 6/7 are InstantMouser/Subtype and read as zero for a plain mouse: using them silently forwards
// (0,0) moves, which looks like a working handoff whose peer cursor never moves.
const FIELD_MOUSE_DELTA_X: u32 = 4;
const FIELD_MOUSE_DELTA_Y: u32 = 5;
const FIELD_KEYCODE: u32 = 9;
const FIELD_WHEEL_POINT_Y: u32 = 96;
const FIELD_WHEEL_POINT_X: u32 = 97;

// CGEventFlags bits for the modifiers reported through flagsChanged.
const FLAG_CAPS_LOCK: u64 = 0x0001_0000;
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

#[repr(C)]
struct CGPoint { x: f64, y: f64 }

struct Ctx {
    tx: Arc<Sender<Captured>>,
    state: HookState,
    memory: input::HookMemory,
    tap: Cell<Ref>,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(loc: u32, place: u32, options: u32, mask: u64, callback: extern "C" fn(u32, u32, Ref, Ref) -> Ref, refcon: Ref) -> Ref;
    fn CGEventTapEnable(tap: Ref, enable: bool);
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    fn CGEventGetLocation(event: Ref) -> CGPoint;
    fn CGEventGetFlags(event: Ref) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(alloc: Ref, port: Ref, order: i64) -> Ref;
    fn CFRunLoopGetCurrent() -> Ref;
    fn CFRunLoopAddSource(loop_: Ref, source: Ref, mode: Ref);
    fn CFRunLoopRun();
    fn CFRelease(obj: Ref);
    static kCFRunLoopCommonModes: Ref;
}

fn mask() -> u64 {
    [LEFT_DOWN, LEFT_UP, RIGHT_DOWN, RIGHT_UP, MOUSE_MOVED, LEFT_DRAGGED, RIGHT_DRAGGED, KEY_DOWN, KEY_UP, FLAGS_CHANGED, SCROLL_WHEEL, OTHER_DOWN, OTHER_UP, OTHER_DRAGGED]
        .into_iter().fold(0u64, |m, t| m | (1u64 << t))
}

extern "C" fn on_event(_proxy: u32, event_type: u32, event: Ref, refcon: Ref) -> Ref {
    if event.is_null() || refcon.is_null() { return event; }
    let ctx = unsafe { &mut *(refcon as *mut Ctx) };
    if event_type == DISABLED_BY_TIMEOUT || event_type == DISABLED_BY_USER {
        // A slow callback disables the tap; without re-enabling it input stops forwarding silently.
        unsafe { CGEventTapEnable(ctx.tap.get(), true) };
        return event;
    }
    let point = unsafe { CGEventGetLocation(event) };
    let raw = match event_type {
        MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED => input::Raw::Move {
            x: point.x, y: point.y,
            dx: unsafe { CGEventGetIntegerValueField(event, FIELD_MOUSE_DELTA_X) } as f64,
            dy: unsafe { CGEventGetIntegerValueField(event, FIELD_MOUSE_DELTA_Y) } as f64,
            relative: true,
        },
        LEFT_DOWN => input::Raw::Button { button: Button::Left, down: true },
        LEFT_UP => input::Raw::Button { button: Button::Left, down: false },
        RIGHT_DOWN => input::Raw::Button { button: Button::Right, down: true },
        RIGHT_UP => input::Raw::Button { button: Button::Right, down: false },
        OTHER_DOWN => input::Raw::Button { button: Button::Middle, down: true },
        OTHER_UP => input::Raw::Button { button: Button::Middle, down: false },
        SCROLL_WHEEL => input::Raw::Wheel {
            dx: unsafe { CGEventGetIntegerValueField(event, FIELD_WHEEL_POINT_X) } * input::WHEEL_TICKS_PER_UNIT,
            dy: unsafe { CGEventGetIntegerValueField(event, FIELD_WHEEL_POINT_Y) } * input::WHEEL_TICKS_PER_UNIT,
        },
        KEY_DOWN => input::Raw::Key { code: key_from_code(unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) }), down: true },
        KEY_UP => input::Raw::Key { code: key_from_code(unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) }), down: false },
        FLAGS_CHANGED => {
            let code = key_from_code(unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) });
            let flags = unsafe { CGEventGetFlags(event) };
            let down = match code {
                Key::ShiftLeft | Key::ShiftRight => flags & FLAG_SHIFT != 0,
                Key::ControlLeft | Key::ControlRight => flags & FLAG_CONTROL != 0,
                Key::Alt | Key::AltGr => flags & FLAG_OPTION != 0,
                Key::MetaLeft | Key::MetaRight => flags & FLAG_COMMAND != 0,
                Key::CapsLock => flags & FLAG_CAPS_LOCK != 0,
                _ => false,
            };
            input::Raw::Key { code, down }
        }
        _ => input::Raw::Other,
    };
    if input::handle(raw, &ctx.state, &mut ctx.memory, &ctx.tx) { event } else { std::ptr::null_mut() }
}

/// Creates the tap and blocks in its run loop, like `rdev::grab`. Call on a dedicated thread.
pub fn start(tx: Sender<Captured>, state: HookState) -> Result<(), String> {
    let ctx = Box::new(Ctx { tx: Arc::new(tx), state, memory: input::HookMemory::default(), tap: Cell::new(std::ptr::null_mut()) });
    let refcon = Box::into_raw(ctx) as Ref;
    let tap = unsafe { CGEventTapCreate(LOCATION_HID, PLACE_HEAD, OPTION_FILTER, mask(), on_event, refcon) };
    if tap.is_null() {
        unsafe { drop(Box::from_raw(refcon as *mut Ctx)) };
        eprintln!("mouseborder: input tap refused - grant Accessibility and Input Monitoring, then retry");
        return Err("global input hook failed: EventTapError".into());
    }
    let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null_mut(), tap, 0) };
    if source.is_null() {
        unsafe { CFRelease(tap); drop(Box::from_raw(refcon as *mut Ctx)) };
        return Err("global input hook failed: LoopSourceError".into());
    }
    // The callback is not reachable until the source joins the run loop, so this write cannot race.
    unsafe { (*refcon.cast::<Ctx>()).tap.set(tap) };
    // LaunchServices discards a GUI app's stdout, but the unified log keeps it; this line is how a
    // permission failure is told apart from a hook that is armed but idle.
    eprintln!("mouseborder: global input tap ready");
    unsafe {
        CGEventTapEnable(tap, true);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CFRelease(source);
        CFRunLoopRun();
    }
    Ok(())
}

fn key_from_code(code: i64) -> Key {
    match code {
        0 => Key::KeyA, 1 => Key::KeyS, 2 => Key::KeyD, 3 => Key::KeyF, 4 => Key::KeyH, 5 => Key::KeyG,
        6 => Key::KeyZ, 7 => Key::KeyX, 8 => Key::KeyC, 9 => Key::KeyV, 11 => Key::KeyB, 12 => Key::KeyQ,
        13 => Key::KeyW, 14 => Key::KeyE, 15 => Key::KeyR, 16 => Key::KeyY, 17 => Key::KeyT, 18 => Key::Num1,
        19 => Key::Num2, 20 => Key::Num3, 21 => Key::Num4, 22 => Key::Num6, 23 => Key::Num5, 24 => Key::Equal,
        25 => Key::Num9, 26 => Key::Num7, 27 => Key::Minus, 28 => Key::Num8, 29 => Key::Num0, 30 => Key::RightBracket,
        31 => Key::KeyO, 32 => Key::KeyU, 33 => Key::LeftBracket, 34 => Key::KeyI, 35 => Key::KeyP, 36 => Key::Return,
        37 => Key::KeyL, 38 => Key::KeyJ, 39 => Key::Quote, 40 => Key::KeyK, 41 => Key::SemiColon, 42 => Key::BackSlash,
        43 => Key::Comma, 44 => Key::Slash, 45 => Key::KeyN, 46 => Key::KeyM, 47 => Key::Dot, 48 => Key::Tab,
        49 => Key::Space, 50 => Key::BackQuote, 51 => Key::Backspace, 53 => Key::Escape, 54 => Key::MetaRight,
        55 => Key::MetaLeft, 56 => Key::ShiftLeft, 57 => Key::CapsLock, 58 => Key::Alt, 59 => Key::ControlLeft,
        60 => Key::ShiftRight, 61 => Key::AltGr, 62 => Key::ControlRight, 63 => Key::Function,
        96 => Key::F5, 97 => Key::F6, 98 => Key::F7, 99 => Key::F3, 100 => Key::F8, 101 => Key::F9,
        103 => Key::F11, 109 => Key::F10, 111 => Key::F12, 115 => Key::Home, 116 => Key::PageUp, 117 => Key::Delete,
        118 => Key::F4, 119 => Key::End, 120 => Key::F2, 121 => Key::PageDown, 122 => Key::F1, 123 => Key::LeftArrow,
        124 => Key::RightArrow, 125 => Key::DownArrow, 126 => Key::UpArrow,
        other => Key::Unknown(other.clamp(0, u32::MAX as i64) as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_covers_every_forwarded_event_type() {
        for t in [LEFT_DOWN, LEFT_UP, RIGHT_DOWN, RIGHT_UP, MOUSE_MOVED, LEFT_DRAGGED, RIGHT_DRAGGED, KEY_DOWN, KEY_UP, FLAGS_CHANGED, SCROLL_WHEEL, OTHER_DOWN, OTHER_UP] {
            assert!(mask() & (1u64 << t) != 0, "type {t} is not subscribed");
        }
        assert_eq!(mask() & (1u64 << DISABLED_BY_TIMEOUT), 0, "tap notifications are not events");
    }

    #[test]
    fn keycodes_map_to_names_the_peer_parses() {
        for (code, key) in [(0i64, Key::KeyA), (11, Key::KeyB), (59, Key::ControlLeft), (62, Key::ControlRight), (58, Key::Alt), (61, Key::AltGr), (55, Key::MetaLeft), (53, Key::Escape), (51, Key::Backspace), (36, Key::Return), (49, Key::Space), (115, Key::Home), (119, Key::End), (116, Key::PageUp), (121, Key::PageDown), (117, Key::Delete), (123, Key::LeftArrow), (126, Key::UpArrow), (122, Key::F1), (111, Key::F12)] {
            let got = key_from_code(code);
            assert_eq!(got, key, "code {code} mapped to {got:?}");
            let name = format!("{got:?}");
            assert!(name.chars().next().unwrap().is_ascii_uppercase() && name.chars().all(|c| c.is_alphanumeric()), "{name} is not a plain wire name");
        }
    }

    #[test]
    fn out_of_range_keycodes_stay_unknown() {
        assert_eq!(key_from_code(-1), Key::Unknown(0));
        assert_eq!(key_from_code(70000), Key::Unknown(70000));
    }
}
