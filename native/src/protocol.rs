use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// `token` re-authenticates a previously paired device without asking for the current code.
    Pair {
        pin: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// `trust` is the shared secret both ends keep for this pair, so either side can dial later.
    PairResult {
        ok: bool,
        key: Option<String>,
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trust: Option<String>,
    },
    Input { event: InputEvent },
    Clipboard { text: String },
    /// Base64 PNG. A screenshot is pixels, not a path the peer could resolve.
    ClipboardImage { png: String },
    /// Announces a file copy; the bytes follow as `FileChunk` messages in list order.
    ClipboardFiles { transfer: u64, files: Vec<FileEntry> },
    FileChunk { transfer: u64, data: String },
    ReturnControl,
    Control { enabled: bool },
    Ping,
    Disconnect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry { pub name: String, pub size: u64 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    Move { dx: f64, dy: f64 },
    /// Absolute peer-cursor position in normalised 0..65535 units, sent once on hand-off so the
    /// cursor appears on the border it entered by instead of wherever the last session left it.
    MoveTo { x: f64, y: f64 },
    Button { button: MouseButton, down: bool },
    Wheel { dx: i64, dy: i64 },
    Key { code: String, down: bool },
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton { Left, Right, Middle }