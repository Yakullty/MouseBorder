//! End-to-end check of pairing, HMAC sealing and clipboard sync against a running instance.
//!
//! Start one app with a known code and the edge hook switched off, then:
//!   MOUSEBORDER_TEST_PIN=246810 cargo test --test clipboard_link -- --ignored --nocapture
//!
//! Only the link is exercised here; a live edge hook would move the pointer on the machine under
//! test, so keep `edge_enabled` false in the app's settings.json while running this.

#![cfg(target_os = "macos")]

use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::io::Write;
use std::process::Command;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

const PORT: u16 = 39876;
type Peer = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

fn pbpaste() -> Option<String> {
    let out = Command::new("pbpaste").output().expect("pbpaste").stdout;
    // A non-UTF-8 flavor (UTF-16 rich text, for instance) must not be restored through a lossy
    // conversion: writing the replacement characters back would silently corrupt the user's copy.
    String::from_utf8(out).ok()
}
fn pbcopy(text: &str) {
    let mut child = Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn().expect("pbcopy");
    child.stdin.take().unwrap().write_all(text.as_bytes()).expect("write pbcopy");
    child.wait().expect("pbcopy exit");
}
/// The app hashes the compact serde_json form of the body, so the mock peer must sign the same bytes.
fn mac_of(key: &str, body: &Value) -> String {
    let mut h = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    h.update(&serde_json::to_vec(body).expect("serialize body"));
    h.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
fn sealed(key: &str, body: Value) -> Message {
    Message::Text(json!({"mac": mac_of(key, &body), "body": body}).to_string().into())
}
async fn next_frame(ws: &mut Peer, wait: Duration) -> Option<Value> {
    match tokio::time::timeout(wait, ws.next()).await.ok()??.ok()? {
        Message::Text(t) => Some(serde_json::from_str(&t).expect("frame is json")),
        _ => None,
    }
}
async fn wait_for_pasteboard(expected: &str) -> bool {
    for _ in 0..25 {
        if pbpaste().as_deref() == Some(expected) { return true }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

#[tokio::test]
#[ignore = "requires a running MouseBorder instance started with MOUSEBORDER_PIN"]
async fn pairs_seals_and_syncs_the_pasteboard() {
    let pin = std::env::var("MOUSEBORDER_TEST_PIN").expect("set MOUSEBORDER_TEST_PIN to the app's MOUSEBORDER_PIN");
    let original = match pbpaste() {
        Some(text) => text,
        None => panic!("the pasteboard does not hold UTF-8 text; copy something plain or clear it before running this test"),
    };
    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{PORT}")).await.expect("connect to the app");

    ws.send(Message::Text(json!({"type":"pair","pin":pin,"label":"mock-peer"}).to_string().into())).await.unwrap();
    let reply = next_frame(&mut ws, Duration::from_secs(5)).await.expect("pair result");
    assert_eq!(reply["type"].as_str(), Some("pair_result"), "unexpected reply: {reply}");
    assert_eq!(reply["ok"].as_bool(), Some(true), "pairing was rejected: {reply}");
    let key = reply["key"].as_str().expect("session key").to_string();
    assert_eq!(key.len(), 64, "session key should be 32 bytes of hex");
    assert!(reply["label"].is_string(), "the app should report its hostname");

    let inbound = format!("MOUSEBORDER-IN-{pin}");
    ws.send(sealed(&key, json!({"type":"clipboard","text":inbound.clone()}))).await.unwrap();
    assert!(wait_for_pasteboard(&inbound).await, "a verified clipboard packet must reach the pasteboard");

    ws.send(Message::Text(json!({"mac":"0".repeat(64),"body":{"type":"clipboard","text":"SHOULD-NOT-LAND"}}).to_string().into())).await.unwrap();
    assert_eq!(pbpaste().as_deref(), Some(inbound.as_str()), "a packet with a bad MAC must be dropped, not applied");

    let outbound = format!("MOUSEBORDER-OUT-{pin}");
    pbcopy(&outbound);
    let mut forwarded = None;
    for _ in 0..5 {
        if let Some(frame) = next_frame(&mut ws, Duration::from_secs(3)).await {
            if frame["body"]["type"].as_str() == Some("clipboard") { forwarded = Some(frame); break }
        }
    }
    let frame = forwarded.expect("the app must forward a local pasteboard change");
    assert_eq!(frame["body"]["text"].as_str(), Some(outbound.as_str()));
    assert_eq!(frame["mac"].as_str(), Some(mac_of(&key, &frame["body"]).as_str()), "the forwarded packet must be sealed with the session key");

    pbcopy(&original);
    ws.close(None).await.ok();
}
