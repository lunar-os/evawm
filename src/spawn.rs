//! Spawn children and reap them without leaking zombie processes or threads.
//!
//! Dropping `std::process::Child` does not wait for it, so short-lived
//! commands launched by binds or IPC would otherwise stay as zombies for the
//! rest of the session. One shared background reaper thread watches every
//! spawned child via Linux `pidfd_open` + `poll`, sleeping at 0% CPU until a
//! child exits or a new one is queued.
//!
//! Child processes receive compositor session variables via
//! `EvaWM::apply_child_env` instead of mutating process-global `environ`.

use std::{
    io,
    os::fd::OwnedFd,
    process::{Child, Command},
    sync::{Arc, Mutex, OnceLock},
    thread,
};

use smithay::reexports::rustix::{
    event::{EventfdFlags, PollFd, PollFlags, eventfd, poll},
    io::{read, write},
    process::{Pid, PidfdFlags, pidfd_open},
};

struct Reaper {
    pending: Arc<Mutex<Vec<(Child, OwnedFd)>>>,
    wake_fd: Arc<OwnedFd>,
}

static REAPER: OnceLock<Result<Reaper, io::ErrorKind>> = OnceLock::new();

fn reaper() -> io::Result<&'static Reaper> {
    let result = REAPER.get_or_init(|| init_reaper().map_err(|err| err.kind()));
    result
        .as_ref()
        .map_err(|kind| io::Error::new(*kind, "child reaper unavailable"))
}

fn init_reaper() -> io::Result<Reaper> {
    let wake_fd = Arc::new(
        eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).map_err(io::Error::from)?,
    );
    let pending: Arc<Mutex<Vec<(Child, OwnedFd)>>> = Arc::new(Mutex::new(Vec::new()));

    let thread_wake = Arc::clone(&wake_fd);
    let thread_pending = Arc::clone(&pending);
    thread::Builder::new()
        .name("evawm child reaper".into())
        .spawn(move || reaper_loop(thread_wake, thread_pending))?;

    Ok(Reaper { pending, wake_fd })
}

fn reaper_loop(wake_fd: Arc<OwnedFd>, pending: Arc<Mutex<Vec<(Child, OwnedFd)>>>) {
    let mut children: Vec<(Child, OwnedFd)> = Vec::new();
    let mut wake_buf = [0u8; 8];

    loop {
        if let Ok(mut guard) = pending.lock() {
            children.append(&mut guard);
        }

        // Reap any child that already exited before we block in poll().
        children.retain_mut(|(child, _)| matches!(child.try_wait(), Ok(None)));

        let (woke, ready_children) = {
            let mut poll_fds = Vec::with_capacity(children.len() + 1);
            poll_fds.push(PollFd::new(&wake_fd, PollFlags::IN));
            for (_, pidfd) in &children {
                poll_fds.push(PollFd::new(pidfd, PollFlags::IN));
            }

            if let Err(err) = poll(&mut poll_fds, None)
                && err != smithay::reexports::rustix::io::Errno::INTR
            {
                tracing::warn!("child reaper poll failed: {err}");
                thread::sleep(std::time::Duration::from_millis(50));
                (false, None)
            } else {
                let woke = !poll_fds[0].revents().is_empty();
                let ready: Vec<bool> = poll_fds[1..]
                    .iter()
                    .map(|fd| !fd.revents().is_empty())
                    .collect();
                (woke, Some(ready))
            }
        };

        if woke {
            while read(&wake_fd, &mut wake_buf).is_ok() {}
        }

        if let Some(ready_children) = ready_children {
            let mut idx = 0;
            children.retain_mut(|(child, _)| {
                let ready = ready_children[idx];
                idx += 1;
                if ready {
                    let _ = child.wait();
                    false
                } else {
                    true
                }
            });
        } else {
            children.retain_mut(|(child, _)| matches!(child.try_wait(), Ok(None)));
        }
    }
}

/// Spawn `command` and register it with the shared background reaper. `what`
/// describes the child in warning logs on fallback; the caller decides whether
/// to log or propagate the returned error.
pub fn reap(mut command: Command, what: &str) -> io::Result<()> {
    let mut child = command.spawn()?;
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Ok(());
    }

    let pid = i32::try_from(child.id())
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other(format!("invalid child pid for {what}")))?;

    let pidfd = match pidfd_open(pid, PidfdFlags::NONBLOCK) {
        Ok(fd) => fd,
        Err(_) => {
            // If the child already exited between `spawn` and `pidfd_open`,
            // reap it immediately instead of leaking a zombie.
            if matches!(child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
            // Fallback for kernels/sandboxes without pidfd_open.
            let _ = thread::Builder::new()
                .name("evawm child reaper fallback".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            return Ok(());
        }
    };

    let reaper = match reaper() {
        Ok(reaper) => reaper,
        Err(err) => {
            tracing::warn!("shared reaper unavailable for {what}: {err}; using fallback thread");
            let _ = thread::Builder::new()
                .name("evawm child reaper fallback".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            return Ok(());
        }
    };

    if let Ok(mut guard) = reaper.pending.lock() {
        guard.push((child, pidfd));
    }
    let _ = write(&reaper.wake_fd, &1u64.to_ne_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::reap;

    #[test]
    fn missing_program_returns_error() {
        let command = Command::new("/nonexistent/evawm-test-program");
        assert!(reap(command, "missing program").is_err());
    }

    #[test]
    fn short_command_is_accepted() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 0");
        assert!(reap(command, "short command").is_ok());
    }
}
