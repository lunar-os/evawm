//! Native EvaWM IPC: a newline-delimited JSON socket plus subscriber stream.

mod actions;
pub mod client;
mod protocol;
mod server;

pub(crate) use actions::{IpcAction, parse as parse_action};
pub(crate) use protocol::{OutputInfo, StateSnapshot, WindowInfo, WindowRect, WorkspaceInfo};
pub(crate) use server::{IpcReply, IpcRequest, IpcServer, start as start_server, version_value};
