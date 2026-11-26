//! SIGTERM/SIGINT handling.
//!
//! A signal handler cannot touch compositor state, so it writes one byte to a
//! non-blocking self-pipe. The event loop reads that byte and stops normally,
//! which runs the session, gamma, IPC-socket, and environment cleanup paths.

use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    sync::atomic::{AtomicI32, Ordering},
};

use anyhow::{Context, Result};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic};

use crate::state::EvaWM;

static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_signal(_signal: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    // Preserve errno across the signal handler so an interrupted syscall's
    // errno is never clobbered if write() returns EAGAIN/EPIPE.
    // SAFETY: `__errno_location` returns a valid thread-local pointer.
    let saved_errno = unsafe { *libc::__errno_location() };
    let byte = 1u8;
    // SAFETY: `write` is async-signal-safe. A full or closed pipe is harmless;
    // a queued byte already guarantees the event loop wakes up.
    unsafe {
        libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        *libc::__errno_location() = saved_errno;
    }
}

pub fn install(event_loop: &EventLoop<'_, EvaWM>) -> Result<()> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a valid two-element array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error()).context("create signal self-pipe");
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // SAFETY: `read_fd` was just created above and is owned by this `File`,
    // so early error returns close it automatically.
    let pipe = unsafe { File::from_raw_fd(read_fd) };

    // Register the event-loop source before installing signal handlers so a
    // registration failure never leaves a handler pointing at a closed pipe.
    let token = match event_loop.handle().insert_source(
        Generic::new(pipe, Interest::READ, Mode::Level),
        |_, pipe, state: &mut EvaWM| {
            let fd = pipe.as_raw_fd();
            let mut buffer = [0u8; 64];
            loop {
                // SAFETY: `fd` is the non-blocking read end owned by the
                // event-loop source, and `buffer` is valid for its length.
                let read = unsafe {
                    libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len())
                };
                if read <= 0 {
                    break;
                }
            }
            tracing::info!("termination signal received; stopping event loop");
            state.loop_signal.stop();
            Ok(PostAction::Continue)
        },
    ) {
        Ok(token) => token,
        Err(err) => {
            // SAFETY: `write_fd` is owned here and was never published.
            unsafe {
                libc::close(write_fd);
            }
            return Err(anyhow::anyhow!("insert signal source: {err}"));
        }
    };

    SIGNAL_WRITE_FD.store(write_fd, Ordering::Release);

    // SAFETY: zeroed sigaction initialized with `sigemptyset`, `SA_RESTART`,
    // and an async-signal-safe handler.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handle_signal as *const () as usize;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: `action.sa_mask` is a valid aligned `sigset_t`.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }

    let signals = [libc::SIGTERM, libc::SIGINT];
    for (installed, &signal) in signals.iter().enumerate() {
        // SAFETY: `action` is initialized with an async-signal-safe handler.
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            let err = std::io::Error::last_os_error();
            let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
            default.sa_sigaction = libc::SIG_DFL;
            // SAFETY: restoring `SIG_DFL` on any signals already installed above.
            unsafe {
                libc::sigemptyset(&mut default.sa_mask);
                for &prev in &signals[..installed] {
                    libc::sigaction(prev, &default, std::ptr::null_mut());
                }
            }
            SIGNAL_WRITE_FD.store(-1, Ordering::Release);
            event_loop.handle().remove(token);
            // SAFETY: `write_fd` is owned here and no longer published.
            unsafe {
                libc::close(write_fd);
            }
            return Err(err).context("install signal handler");
        }
    }

    // The write end stays open for the process lifetime; the read end is owned
    // by the event-loop source.
    Ok(())
}
