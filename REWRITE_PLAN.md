# MouseBorder native rewrite

## Why replace Electron

The current Electron build embeds Chromium and Node.js. The native rewrite uses Rust for input, networking and clipboard work, with a small Tauri UI using the operating system's WebView. Windows uses WebView2 and macOS uses WKWebView. This should cut the app payload substantially; the 10 MB target is an optimization target, not guaranteed until both release artifacts are measured. On Windows, a missing WebView2 runtime may need a one-time runtime install.

## Feature architecture

- No LAN broadcast. The user types the peer IP and current six-digit pairing code.
- A pairing session opens a direct WebSocket connection; clipboard and input are forwarded only after pairing.
- A native global input hook captures keyboard/mouse events. While the pointer crosses the selected edge, the local event is suppressed and sent to the peer. The peer injects the event locally. A dedicated escape hotkey returns control and releases held keys/buttons.
- macOS uses a CoreGraphics event tap and requires Accessibility/Input Monitoring permissions. Windows uses low-level keyboard/mouse hooks and `SendInput`.
- Text clipboard updates are sent over the paired connection with loop prevention.
- Release builds: signed/notarized macOS DMG/ZIP and x64 Windows portable EXE. Windows portable output should be built on Windows; this Apple Silicon host cannot reliably run the required NSIS tools.

## Implementation order

1. Native input capture/suppression and event serialization.
2. Direct-IP pairing and reconnection, no discovery broadcasts.
3. Native input injection and return-to-host handling.
4. Clipboard sync and keyboard layout/shortcut handling.
5. Small Tauri settings UI, permissions guide, panic-safe hook behavior.
6. Build and measure both targets on real Mac and Windows machines; optimize release profiles and package resources.

## Current state

The Electron client has been deleted; this repository now builds one native target only. The old sources live in `归档.zip` and its wire protocol is not compatible with the native one, so a mixed pair of machines cannot pair.

Done and verified on this Mac:

- Manual-IP pairing, six-digit code refresh, HMAC-SHA256 sealed session messages, and silent drop of tampered packets — proven by `native/tests/clipboard_link.rs`, a mock peer that runs the real handshake against the shipped bundle.
- Bidirectional text clipboard sync with loop prevention — same test.
- Settings persistence (`<appData>/settings.json`) and capped-backoff reconnection.
- Edge handoff geometry extracted into pure functions with unit tests, plus automatic re-anchoring of the hidden local cursor so a clamped border no longer zeroes the derived delta. That was the recorded blocker in the previous draft of this plan.
- `cargo check --target x86_64-pc-windows-msvc` passes, so the Windows code paths type-check. Linking a Windows binary still has to happen on Windows: `tauri-build` compiles the icon into a Win32 resource and needs the SDK resource compiler.
- Apple Silicon bundle: 2.7 MiB, ZIP 1.5 MiB.

- macOS capture no longer goes through `rdev::grab`. Its converter calls `TISGetInputSourceProperty` on every key press, and that HIToolbox API asserts the main queue; from the tap thread the first physical key press aborted the process (`dispatch_assert_queue_fail`, SIGTRAP). `native/src/tap.rs` creates the filtering tap directly, reads only scalar event fields, and takes device-relative mouse deltas, which also removes the pointer-clamping stall at the source instead of only papering over it with re-anchoring. Injection still uses `rdev::simulate`.
- Modifier keys are now forwarded while driving the peer. The old arms consumed Ctrl/Alt for hotkey bookkeeping and let them pass locally, so shortcuts like Ctrl+C never reached the other machine.

Still open:

- A real Mac-to-Windows session. Nothing has proved that capture, suppression and injection feel right across two machines, and the hand-back ergonomics (peer-edge release versus `Ctrl+Alt+Esc`) are untested.
- macOS input permissions. `kTCCServicePostEvent` (Accessibility) is currently denied for this build, and because the bundle is ad-hoc linker-signed its code-signing identifier changes on every rebuild, so grants expire — measured directly: `kTCCServiceListenEvent` went from allowed to denied after one rebuild. Fix is a stable self-signed signing identity, see README.
- Multi-monitor mapping, resolution changes mid-link, keyboard layout and system-shortcut handling, heartbeat-based dead-link detection, and signing/notarization.
