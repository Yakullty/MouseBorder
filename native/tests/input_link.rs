//! End-to-end check that verified input packets actually move this machine's pointer.
//!
//! Start one app with a known code and the edge hook switched off, then:
//!   MOUSEBORDER_TEST_PIN=246810 cargo test --test input_link -- --ignored --nocapture
//!
//! The pointer is parked near the centre of the whole desktop by the app's injector, so the test
//! moves it there and back. Keep `edge_enabled` false in the app's settings.json: a live edge hook
//! would swallow the physical input of whoever is sitting at this machine.

#![cfg(target_os = "macos")]

use display_info::DisplayInfo;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::ffi::c_void;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const PORT: u16 = 39876;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CGPoint { x: f64, y: f64 }

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *mut c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(obj: *mut c_void);
}

fn cursor() -> CGPoint {
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        let point = CGEventGetLocation(event);
        CFRelease(event);
        point
    }
}
/// Centre of the union of every display, which is where the app starts the injected cursor.
fn desktop_center() -> CGPoint {
    let displays = DisplayInfo::all().expect("display list");
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for d in &displays {
        x0 = x0.min(d.x); y0 = y0.min(d.y);
        x1 = x1.max(d.x + d.width as i32); y1 = y1.max(d.y + d.height as i32);
    }
    CGPoint { x: (x0 + x1) as f64 / 2.0, y: (y0 + y1) as f64 / 2.0 }
}

fn mac_of(key: &str, body: &Value) -> String {
    let mut h = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    h.update(&serde_json::to_vec(body).expect("serialize body"));
    h.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
fn sealed(key: &str, body: Value) -> Message {
    Message::Text(json!({"mac": mac_of(key, &body), "body": body}).to_string().into())
}
fn near(a: CGPoint, b: CGPoint) -> bool { (a.x - b.x).abs() < 6.0 && (a.y - b.y).abs() < 6.0 }

#[tokio::test]
#[ignore = "requires a running MouseBorder instance started with MOUSEBORDER_PIN and edge_enabled=false"]
async fn verified_input_packets_move_the_pointer() {
    let pin = std::env::var("MOUSEBORDER_TEST_PIN").expect("set MOUSEBORDER_TEST_PIN to the app's MOUSEBORDER_PIN");
    let home = cursor();
    let center = desktop_center();
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{PORT}")).await.expect("connect to the app");

    ws.send(Message::Text(json!({"type":"pair","pin":pin,"label":"mock-peer"}).to_string().into())).await.unwrap();
    let reply: Value = match tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap() {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected frame: {other:?}"),
    };
    assert_eq!(reply["ok"].as_bool(), Some(true), "pairing was rejected: {reply}");
    let key = reply["key"].as_str().expect("session key").to_string();

    ws.send(sealed(&key, json!({"type":"control","enabled":true}))).await.unwrap();
    // The injector accumulates from the desktop centre, so the destination is predictable.
    let target = CGPoint { x: center.x + 150.0, y: center.y + 90.0 };
    for _ in 0..6 {
        ws.send(sealed(&key, json!({"type":"input","event":{"kind":"move","dx":25.0,"dy":15.0}}))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    let moved = cursor();
    assert!(near(moved, target), "verified moves must reach the pointer: expected {target:?}, got {moved:?}");

    // A whole notch each way: scrolling must reach the peer without dragging the pointer along.
    for (dx, dy) in [(0, 120), (0, -120), (120, 0)] {
        ws.send(sealed(&key, json!({"type":"input","event":{"kind":"wheel","dx":dx,"dy":dy}}))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(near(cursor(), moved), "a wheel packet must not move the pointer");

    ws.send(Message::Text(json!({"mac":"0".repeat(64),"body":{"type":"input","event":{"kind":"move","dx":400.0,"dy":400.0}}}).to_string().into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let after_bad = cursor();
    assert!((after_bad.x - moved.x - 400.0).abs() > 50.0 || (after_bad.y - moved.y - 400.0).abs() > 50.0, "a packet with a bad MAC must not move the pointer");

    ws.send(sealed(&key, json!({"type":"input","event":{"kind":"move","dx":home.x-moved.x,"dy":home.y-moved.y}}))).await.unwrap();
    ws.send(sealed(&key, json!({"type":"return_control"}))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(near(cursor(), home), "the pointer must be parked back where the test found it");
    ws.close(None).await.ok();
}
