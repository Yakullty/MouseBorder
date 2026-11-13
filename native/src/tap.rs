//! macOS capture layer.
//!
//! rdev's `grab` converts every key press through HIToolbox's input-source APIs
//! (`TISGetInputSourceProperty`), which assert they run on the main queue. A CGEventTap callback
//! runs on the tap thread, so the first physical key press aborted the process with
//! `dispatch_assert_queue_fail`. This tap only reads scalar event fields, so it stays inside the
//! thread-safe subset of CoreGraphics, and it reports device deltas instead of pointer positions.
//! Injection still goes through `rdev::simulate`.
//!
//! # Why the local cursor has to be pinned here
//!
//! Swallowing a mouse event (`return nil`) does **not** stop the cursor: the pointer still follows
//! the hardware, and only the apps stop seeing the event. Neither does zeroing the delta on its
//! own. The measured combination that actually holds the pointer still is
//!
//!   1. `CGAssociateMouseAndMouseCursorPosition(0)` — re-asserted on every event, because the
//!      association is global state that the system quietly restores;
//!   2. rewriting `event.location` to the border point the cursor left through and **passing the
//!      event on** — the rewrite only takes effect because the event is delivered;
//!   3. zeroing `kCGEventMouseDeltaX/Y` as well, otherwise WindowServer keeps accumulating the
//!      device delta internally and the cursor jumps back once a real device event arrives.
//!
//! `CGDisplayHideCursor` also needs `SetsCursorInBackground` on the WindowServer connection: as a
//! background app the hide call returns success and does nothing, so the cursor stays visible on
//! the Mac while the peer moves. It is a counter, so every hide is flushed with a matching show.

use crate::input::{self, Captured, HookState};
use rdev::{Button, Key};
use std::{
    cell::Cell,
    ffi::{c_char, c_void},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::Sender,
        Arc, OnceLock,
    },
};

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
const FIELD_EVENT_SOURCE_PID: u32 = 41;
const FIELD_WHEEL_POINT_Y: u32 = 96;
const FIELD_WHEEL_POINT_X: u32 = 97;

// CGEventFlags bits for the modifiers reported through flagsChanged.
const FLAG_CAPS_LOCK: u64 = 0x0001_0000;
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

/// `kCFStringEncodingUTF8`.
const UTF8: u32 = 0x0800_0100;
/// `RTLD_DEFAULT`: search the images already loaded, where CoreGraphics certainly is.
const RTLD_DEFAULT: isize = -2;

#[repr(C)]
struct CGPoint { x: f64, y: f64 }

struct Ctx {
    tx: Arc<Sender<Captured>>,
    state: HookState,
    memory: input::HookMemory,
    tap: Cell<Ref>,
    /// How often the system has switched the tap off underneath us; any of those silences the pin.
    disabled: Cell<u64>,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(loc: u32, place: u32, options: u32, mask: u64, callback: extern "C" fn(u32, u32, Ref, Ref) -> Ref, refcon: Ref) -> Ref;
    fn CGEventTapEnable(tap: Ref, enable: bool);
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
    fn CGEventGetLocation(event: Ref) -> CGPoint;
    fn CGEventSetLocation(event: Ref, point: CGPoint);
    fn CGEventGetFlags(event: Ref) -> u64;
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    fn CGGetActiveDisplayList(max: u32, ids: *mut u32, count: *mut u32) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(alloc: Ref, port: Ref, order: i64) -> Ref;
    fn CFRunLoopGetCurrent() -> Ref;
    fn CFRunLoopAddSource(loop_: Ref, source: Ref, mode: Ref);
    fn CFRunLoopRun();
    fn CFRelease(obj: Ref);
    fn CFStringCreateWithCString(alloc: Ref, s: *const c_char, encoding: u32) -> Ref;
    static kCFRunLoopCommonModes: Ref;
    static kCFBooleanTrue: Ref;
}

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn getpid() -> i32;
}

fn mask() -> u64 {
    [LEFT_DOWN, LEFT_UP, RIGHT_DOWN, RIGHT_UP, MOUSE_MOVED, LEFT_DRAGGED, RIGHT_DRAGGED, KEY_DOWN, KEY_UP, FLAGS_CHANGED, SCROLL_WHEEL, OTHER_DOWN, OTHER_UP, OTHER_DRAGGED]
        .into_iter().fold(0u64, |m, t| m | (1u64 << t))
}

fn is_motion(event_type: u32) -> bool {
    matches!(event_type, MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED)
}

// MARK: - Background cursor control

/// Looks a symbol up in the already-loaded images. `CGCursorIsVisible` and the CGS pair are not
/// declared in a public header, so binding them lazily keeps the tap loadable if one disappears.
unsafe fn sym<T: Copy>(name: &[u8]) -> Option<T> {
    debug_assert_eq!(name.last(), Some(&0), "symbol names must be NUL-terminated");
    let found = dlsym(RTLD_DEFAULT as *mut c_void, name.as_ptr() as *const c_char);
    if found.is_null() { None } else { Some(std::mem::transmute_copy(&found)) }
}

/// Lifts the foreground-only restriction on cursor hiding for this process's WindowServer
/// connection. Without it every hide below reports success and does nothing.
fn enable_background_cursor() -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| unsafe {
        let connection: Option<unsafe extern "C" fn() -> u32> = sym(b"_CGSDefaultConnection\0");
        let set_property: Option<unsafe extern "C" fn(u32, u32, Ref, Ref) -> i32> = sym(b"CGSSetConnectionProperty\0");
        let (Some(connection), Some(set_property)) = (connection, set_property) else {
            eprintln!("mouseborder: cursor-hiding symbols are missing; the pointer stays visible while the peer is driven");
            return false;
        };
        let key = CFStringCreateWithCString(std::ptr::null_mut(), b"SetsCursorInBackground\0".as_ptr() as *const c_char, UTF8);
        if key.is_null() { return false; }
        let status = set_property(connection(), connection(), key, kCFBooleanTrue);
        CFRelease(key);
        if status != 0 {
            eprintln!("mouseborder: could not unlock background cursor control ({status}); the pointer stays visible while the peer is driven");
        }
        status == 0
    })
}

fn cursor_visible() -> bool {
    static PROBE: OnceLock<Option<unsafe extern "C" fn() -> u32>> = OnceLock::new();
    PROBE.get_or_init(|| unsafe { sym(b"CGCursorIsVisible\0") }).map_or(true, |probe| unsafe { probe() != 0 })
}

fn displays() -> Vec<u32> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 || count == 0 {
        return vec![unsafe { CGMainDisplayID() }];
    }
    ids[..count as usize].to_vec()
}

/// One hide round. The hide is a counter per display, so every call here is matched on the way out.
static HIDDEN: AtomicU64 = AtomicU64::new(0);
static LAST_HIDE_MS: AtomicU64 = AtomicU64::new(0);

fn hide_round() {
    for id in displays() { unsafe { let _ = CGDisplayHideCursor(id); } }
    HIDDEN.fetch_add(1, Ordering::Relaxed);
    LAST_HIDE_MS.store(input::now_ms(), Ordering::Relaxed);
}

/// Hides the cursor and decouples it from the hardware. Called once when control leaves this
/// machine; the per-event re-assert below keeps it that way.
pub(crate) fn lock_cursor() {
    let unlocked = enable_background_cursor();
    unsafe { CGAssociateMouseAndMouseCursorPosition(0) };
    hide_round();
    // One line per hand-off, and the only objective record of whether the hide actually took
    // effect: `CGDisplayHideCursor` reports success even when it does nothing.
    eprintln!("mouseborder: pointer pinned for the peer (background control {}; cursor {})",
        if unlocked { "unlocked" } else { "NOT unlocked" },
        if cursor_visible() { "STILL VISIBLE - the hide did not take effect" } else { "hidden" });
}

/// Puts the pointer back under the user's control. The show count is over-flushed on purpose: a
/// drifted counter must never leave the user without a pointer after the app is done.
pub(crate) fn unlock_cursor() {
    unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
    let count = HIDDEN.swap(0, Ordering::Relaxed);
    if count == 0 { return; }
    for _ in 0..(count + 3) {
        for id in displays() { unsafe { let _ = CGDisplayShowCursor(id); } }
    }
}

/// Re-asserted on every pointer event: the system re-shows the cursor on real device input and can
/// re-associate it at any point, and either one unpins the pointer.
fn reassert_lock() {
    enable_background_cursor();
    unsafe { CGAssociateMouseAndMouseCursorPosition(0) };
    let now = input::now_ms();
    let since = now.saturating_sub(LAST_HIDE_MS.load(Ordering::Relaxed));
    // The hide itself is throttled: a 1 kHz mouse would otherwise push the counter into the tens
    // of thousands. Only re-hide when the cursor is actually visible again, with a one-second
    // fallback in case the visibility probe is unavailable.
    if since > 10 && (cursor_visible() || since > 1000) {
        hide_round();
    }
}

// MARK: - Event translation

unsafe fn read_raw(event_type: u32, event: Ref) -> Option<input::Raw> {
    let point = CGEventGetLocation(event);
    Some(match event_type {
        MOUSE_MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED => input::Raw::Move {
            x: point.x, y: point.y,
            dx: CGEventGetIntegerValueField(event, FIELD_MOUSE_DELTA_X) as f64,
            dy: CGEventGetIntegerValueField(event, FIELD_MOUSE_DELTA_Y) as f64,
            relative: true,
        },
        LEFT_DOWN => input::Raw::Button { button: Button::Left, down: true },
        LEFT_UP => input::Raw::Button { button: Button::Left, down: false },
        RIGHT_DOWN => input::Raw::Button { button: Button::Right, down: true },
        RIGHT_UP => input::Raw::Button { button: Button::Right, down: false },
        OTHER_DOWN => input::Raw::Button { button: Button::Middle, down: true },
        OTHER_UP => input::Raw::Button { button: Button::Middle, down: false },
        SCROLL_WHEEL => input::Raw::Wheel {
            dx: CGEventGetIntegerValueField(event, FIELD_WHEEL_POINT_X) * input::WHEEL_TICKS_PER_UNIT,
            dy: CGEventGetIntegerValueField(event, FIELD_WHEEL_POINT_Y) * input::WHEEL_TICKS_PER_UNIT,
        },
        KEY_DOWN => input::Raw::Key { code: key_from_code(CGEventGetIntegerValueField(event, FIELD_KEYCODE)), down: true },
        KEY_UP => input::Raw::Key { code: key_from_code(CGEventGetIntegerValueField(event, FIELD_KEYCODE)), down: false },
        FLAGS_CHANGED => {
            let code = key_from_code(CGEventGetIntegerValueField(event, FIELD_KEYCODE));
            let flags = CGEventGetFlags(event);
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
        _ => return None,
    })
}

extern "C" fn on_event(_proxy: u32, event_type: u32, event: Ref, refcon: Ref) -> Ref {
    if event.is_null() || refcon.is_null() { return event; }
    let ctx = unsafe { &mut *(refcon as *mut Ctx) };
    if event_type == DISABLED_BY_TIMEOUT || event_type == DISABLED_BY_USER {
        // A disabled tap runs none of the logic below, so the pin and the handoff are both off
        // until it is switched back on. That has to happen first and it has to be visible.
        let count = ctx.disabled.get() + 1;
        ctx.disabled.set(count);
        if count <= 5 || count % 20 == 0 {
            eprintln!("mouseborder: input tap disabled by {} ({count} so far) - re-enabled; while it is down the cursor is not pinned",
                if event_type == DISABLED_BY_TIMEOUT { "timeout" } else { "the system" });
        }
        unsafe { CGEventTapEnable(ctx.tap.get(), true) };
        return event;
    }
    let Some(raw) = (unsafe { read_raw(event_type, event) }) else { return event };
    // Our own injections come back through this tap. The source process id is the only marker a
    // race cannot lose, so these bypass every local rule instead of feeding the handoff logic.
    if unsafe { CGEventGetIntegerValueField(event, FIELD_EVENT_SOURCE_PID) } == unsafe { getpid() } as i64 {
        input::handle_injected(&raw, &ctx.state, &mut ctx.memory, &ctx.tx);
        return event;
    }
    let was_remote = ctx.state.remote.load(Ordering::Relaxed);
    let pass = input::handle(raw, &ctx.state, &mut ctx.memory, &ctx.tx);
    if ctx.state.remote.load(Ordering::Relaxed) {
        if is_motion(event_type) {
            let (x, y) = ctx.memory.pin();
            unsafe {
                CGEventSetLocation(event, CGPoint { x, y });
                CGEventSetIntegerValueField(event, FIELD_MOUSE_DELTA_X, 0);
                CGEventSetIntegerValueField(event, FIELD_MOUSE_DELTA_Y, 0);
            }
        }
        reassert_lock();
    } else if was_remote {
        unlock_cursor();
    }
    // Never swallow a motion event: the rewrite above only holds the pointer because the event is
    // delivered. Swallowing hands the cursor back to the hardware.
    if pass { event } else { std::ptr::null_mut() }
}

/// Creates the tap and blocks in its run loop, like `rdev::grab`. Call on a dedicated thread.
pub fn start(tx: Sender<Captured>, state: HookState) -> Result<(), String> {
    let ctx = Box::new(Ctx {
        tx: Arc::new(tx),
        state,
        memory: input::HookMemory::default(),
        tap: Cell::new(std::ptr::null_mut()),
        disabled: Cell::new(0),
    });
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

/// The cursor association is session-global state. Leaving it off would freeze the user's pointer
/// after this process is gone, so every exit path — including the window closing — puts it back.
pub fn emergency_restore() {
    unlock_cursor();
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
    fn only_pointer_travel_is_rewritten_to_the_pin() {
        for t in [MOUSE_MOVED, LEFT_DRAGGED, RIGHT_DRAGGED, OTHER_DRAGGED] {
            assert!(is_motion(t), "type {t} moves the pointer");
        }
        for t in [LEFT_DOWN, LEFT_UP, RIGHT_DOWN, KEY_DOWN, SCROLL_WHEEL, FLAGS_CHANGED] {
            assert!(!is_motion(t), "type {t} must not be rewritten");
        }
    }

    #[test]
    fn the_echo_field_is_the_source_process_id() {
        // CGEventFields.h: kCGEventSourceUnixProcessID = 41. 40 is the *target* pid, which is the
        // window server's rather than ours and would let every injection through as user input.
        assert_eq!(FIELD_EVENT_SOURCE_PID, 41);
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
