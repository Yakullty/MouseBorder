use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Pair { pin: String, label: String },
    PairResult { ok: bool, key: Option<String>, label: Option<String> },
    Input { event: InputEvent },
    Clipboard { text: String },
    ReturnControl,
    Control { enabled: bool },
    Ping,
    Disconnect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
