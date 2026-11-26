//! IPC wire types. The socket uses newline-delimited JSON and is versioned by
//! `PROTOCOL_VERSION`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current newline-delimited JSON IPC protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Wire snapshot of a single output returned by `outputs` and `subscribe`.
#[derive(Debug, Clone, Serialize)]
pub struct OutputInfo {
    pub name: String,
    pub width: i32,
    pub height: i32,
    /// Current mode refresh rate in millihertz (e.g. `60000` for 60 Hz).
    pub refresh_mhz: Option<i32>,
    pub x: i32,
    pub y: i32,
    pub transform: String,
    /// Whether this output currently holds keyboard focus.
    pub active: bool,
    pub active_workspace_id: u64,
    /// Live 1-based index of the active workspace on this output.
    pub active_workspace_index: u32,
}

/// Wire snapshot of a workspace returned by `workspaces` and `subscribe`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInfo {
    /// Stable monotonic per-session workspace identifier.
    pub id: u64,
    /// Live 1-based position on `output` (shifts as dynamic workspaces come and go).
    pub index: u32,
    /// Optional workspace name (currently always `None`).
    pub name: Option<String>,
    pub output: String,
    /// `true` for the visible workspace on each output.
    pub active: bool,
    /// `true` only for the active workspace on the currently focused output.
    pub focused: bool,
    pub mode: String,
    pub window_count: usize,
    pub active_window_id: Option<u64>,
}

/// Window content rectangle in workspace logical coordinates.
#[derive(Debug, Clone, Serialize)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Wire snapshot of a mapped window returned by `windows`, `focused`, and `subscribe`.
#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    /// Stable monotonic per-session window identifier.
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub output: String,
    pub workspace_id: u64,
    pub workspace_index: u32,
    pub focused: bool,
    pub fullscreen: bool,
    pub mode: String,
    pub geometry: WindowRect,
}

/// Full compositor snapshot sent on `subscribe` and state-change events.
#[derive(Debug, Clone, Serialize)]
pub struct StateSnapshot {
    pub outputs: Vec<OutputInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub windows: Vec<WindowInfo>,
    pub focused: Option<WindowInfo>,
}

/// Incoming client IPC request line.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<u64>,
    pub cmd: String,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub args: Vec<Value>,
    /// Confirmation flag required by destructive actions (`quit`).
    #[serde(default)]
    pub yes: bool,
}
