//! Socket listener and per-connection readers. The listener deliberately runs
//! off the event-loop thread; requests are handed to the compositor through a
//! calloop channel.

use std::{
    ffi::OsStr,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use smithay::reexports::{
    calloop::channel::Sender,
    rustix::event::{PollFd, PollFlags, Timespec, poll},
};

use super::protocol::{PROTOCOL_VERSION, Request};

pub struct IpcRequest {
    pub request: Request,
    pub respond: mpsc::Sender<IpcReply>,
    pub events: Option<mpsc::SyncSender<String>>,
}

pub struct IpcReply {
    pub line: String,
    pub subscribe: bool,
}

const MAX_CONNECTIONS: usize = 64;
const MAX_LINE_BYTES: usize = 64 * 1024;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(5);
const SUBSCRIBER_POLL_INTERVAL: Duration = Duration::from_secs(1);
static CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

struct ConnectionGuard;

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct IpcServer {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl IpcServer {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

pub fn start(socket_name: &OsStr, sender: Sender<IpcRequest>) -> std::io::Result<IpcServer> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "XDG_RUNTIME_DIR is unset; refusing to put the IPC socket in /tmp",
        )
    })?;
    let path = PathBuf::from(runtime).join(format!(
        "evawm-{}.sock",
        socket_name.to_string_lossy().replace('/', "_")
    ));
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_socket() {
            let _ = fs::remove_file(&path);
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
    }

    let listener = UnixListener::bind(&path)?;
    if let Err(err) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
        let _ = fs::remove_file(&path);
        return Err(err);
    }
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) => {
            let _ = fs::remove_file(&path);
            return Err(err);
        }
    };
    if metadata.mode() & 0o777 != 0o600 {
        let _ = fs::remove_file(&path);
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "IPC socket permissions are not 0600",
        ));
    }

    if let Err(err) = thread::Builder::new()
        .name("evawm ipc".into())
        .spawn(move || accept_loop(listener, sender))
    {
        let _ = fs::remove_file(&path);
        return Err(std::io::Error::other(err));
    }

    Ok(IpcServer {
        path,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn accept_loop(listener: UnixListener, sender: Sender<IpcRequest>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if CONNECTIONS.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                    CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
                    tracing::warn!("IPC connection limit reached; rejecting client");
                    drop(stream);
                    continue;
                }
                let sender = sender.clone();
                let guard = ConnectionGuard;
                let spawned = thread::Builder::new()
                    .name("evawm ipc client".into())
                    .spawn(move || {
                        let _guard = guard;
                        handle_connection(stream, sender);
                    });
                if spawned.is_err() {
                    tracing::warn!("failed to spawn IPC client thread");
                }
            }
            Err(err) if accept_error_is_transient(&err) => {
                tracing::warn!("IPC accept transient error: {err}; retrying");
                thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                tracing::error!("IPC accept failed permanently: {err}");
                break;
            }
        }
    }
}

fn accept_error_is_transient(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::TimedOut
    ) || matches!(
        err.raw_os_error(),
        // Linux EMFILE/ENFILE/ENOBUFS/ENOMEM: resource exhaustion is
        // transient, unlike an invalid listener.
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

fn handle_connection(stream: UnixStream, sender: Sender<IpcRequest>) {
    let _ = stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT));
    let mut reader = match stream.try_clone() {
        Ok(stream) => BufReader::new(stream),
        Err(err) => {
            tracing::warn!("IPC connection clone failed: {err}");
            return;
        }
    };
    let mut writer = stream;

    loop {
        let mut line = String::new();
        let read = (&mut reader)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_line(&mut line);
        match read {
            Ok(0) => return,
            Ok(_) if line.len() > MAX_LINE_BYTES => {
                tracing::warn!("IPC request exceeded {MAX_LINE_BYTES} bytes; closing");
                return;
            }
            Ok(_) if !line.ends_with('\n') => {
                tracing::debug!("IPC client closed connection without trailing newline");
                return;
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!("IPC read ended: {err}");
                return;
            }
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                let reply = serde_json::json!({
                    "ok": false,
                    "error": format!("invalid request: {err}"),
                });
                if write_line(&mut writer, &reply.to_string()).is_err() {
                    return;
                }
                continue;
            }
        };

        let (respond_tx, respond_rx) = mpsc::channel();
        let (events_tx, events_rx) = if request.cmd == "subscribe" {
            let (tx, rx) = mpsc::sync_channel(256);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        if sender
            .send(IpcRequest {
                request,
                respond: respond_tx,
                events: events_tx,
            })
            .is_err()
        {
            return;
        }

        let reply = match respond_rx.recv() {
            Ok(reply) => reply,
            Err(_) => return,
        };
        if write_line(&mut writer, &reply.line).is_err() {
            return;
        }
        if reply.subscribe
            && let Some(events_rx) = events_rx
        {
            loop {
                match events_rx.recv_timeout(SUBSCRIBER_POLL_INTERVAL) {
                    Ok(event) => {
                        if write_line(&mut writer, &event).is_err() {
                            return;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if peer_disconnected(&writer) {
                            return;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }
}

fn peer_disconnected(stream: &UnixStream) -> bool {
    let mut fds = [PollFd::new(stream, PollFlags::RDHUP)];
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    match poll(&mut fds, Some(&zero)) {
        Ok(0) => false,
        Ok(_) => !fds[0]
            .revents()
            .intersection(PollFlags::RDHUP | PollFlags::HUP | PollFlags::ERR)
            .is_empty(),
        Err(_) => false,
    }
}

fn write_line(writer: &mut UnixStream, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub fn version_value(socket: Option<&std::path::Path>) -> serde_json::Value {
    serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "socket": socket.map(|path| path.to_string_lossy().into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::accept_error_is_transient;
    use std::io::{Error, ErrorKind};

    #[test]
    fn transient_accept_errors_are_retried() {
        assert!(accept_error_is_transient(&Error::from(
            ErrorKind::Interrupted
        )));
        assert!(accept_error_is_transient(&Error::from(
            ErrorKind::ConnectionAborted
        )));
        assert!(accept_error_is_transient(&Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(!accept_error_is_transient(&Error::from(
            ErrorKind::InvalidInput
        )));
    }
}
