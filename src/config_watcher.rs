//! Polling config-file watcher, modeled on niri's watcher.
//!
//! Polling `stat` twice per second is effectively free and avoids an inotify
//! dependency. Parsing happens on the watcher thread so a large config never
//! stalls rendering.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

use smithay::reexports::calloop::channel::Sender;

use crate::config::Config;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Props {
    mtime: SystemTime,
    canonical: PathBuf,
}

impl Props {
    fn from_path(path: &Path) -> std::io::Result<Self> {
        let canonical = path.canonicalize()?;
        let mtime = canonical.metadata()?.modified()?;
        Ok(Self { mtime, canonical })
    }
}

/// Includes named in the main file. Used when parsing fails so a newly added
/// include is still watched and a fix to it triggers a reload.
fn include_paths(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("include") else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        let raw = &rest[..end];
        let joined = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            base.join(raw)
        };
        if !paths.contains(&joined) {
            paths.push(joined);
        }
    }
    paths
}

fn snapshot(paths: &[PathBuf]) -> Vec<Option<Props>> {
    paths
        .iter()
        .map(|path| Props::from_path(path).ok())
        .collect()
}

pub struct ConfigWatcher {
    load: mpsc::Sender<()>,
}

impl ConfigWatcher {
    pub fn new(
        path: PathBuf,
        initial_sources: Vec<PathBuf>,
        changed: Sender<Result<Config, String>>,
    ) -> Option<Self> {
        let (load, load_rx) = mpsc::channel();

        let spawned = thread::Builder::new()
            .name("evawm config watcher".into())
            .spawn(move || {
                let mut watched = if initial_sources.is_empty() {
                    vec![path.clone()]
                } else {
                    initial_sources
                };
                if !watched.contains(&path) {
                    watched.insert(0, path.clone());
                }
                let mut last = snapshot(&watched);
                let mut in_error = false;

                loop {
                    let forced = match load_rx.recv_timeout(POLL_INTERVAL) {
                        Ok(()) => true,
                        Err(mpsc::RecvTimeoutError::Timeout) => false,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };

                    // Drop included files that no longer exist when healthy so a
                    // removal produces a change, but keep missing includes
                    // watched while in error state so creating them reloads.
                    if !in_error {
                        watched
                            .retain(|watched_path| watched_path == &path || watched_path.exists());
                    }
                    if !watched.contains(&path) {
                        watched.insert(0, path.clone());
                    }
                    let current = snapshot(&watched);
                    let reload = forced || current != last;
                    if !reload {
                        continue;
                    }

                    match Config::load(Some(&path)) {
                        Ok((config, _)) => {
                            in_error = false;
                            watched = if config.sources.is_empty() {
                                vec![path.clone()]
                            } else {
                                config.sources.clone()
                            };
                            if !watched.contains(&path) {
                                watched.insert(0, path.clone());
                            }
                            last = snapshot(&watched);
                            if changed.send(Ok(config)).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            in_error = true;
                            // A failed parse may be caused by a newly added or
                            // missing include. Watch it so fixing or creating
                            // that file reloads without touching the main config.
                            if !watched.contains(&path) {
                                watched.insert(0, path.clone());
                            }
                            for include in include_paths(&path) {
                                if !watched.contains(&include) {
                                    watched.push(include);
                                }
                            }
                            last = snapshot(&watched);
                            if changed.send(Err(format!("{err:#}"))).is_err() {
                                break;
                            }
                        }
                    }
                }
            });

        match spawned {
            Ok(_) => Some(Self { load }),
            Err(err) => {
                tracing::warn!("failed to spawn config watcher: {err}; automatic reload disabled");
                None
            }
        }
    }

    /// Force a reload even if the file's mtime did not change.
    pub fn reload(&self) {
        let _ = self.load.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::include_paths;

    #[test]
    fn include_paths_finds_quoted_includes() {
        let dir =
            std::env::temp_dir().join(format!("evawm-config-watcher-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let include = dir.join("extra.kdl");
        std::fs::write(&include, "").expect("write include");
        let main = dir.join("config.kdl");
        std::fs::write(
            &main,
            "include \"extra.kdl\"\nincluded \"ignored.kdl\"\ninclude \"missing.kdl\"\n",
        )
        .expect("write main");

        let paths = include_paths(&main);
        assert!(paths.contains(&include));
        assert!(paths.contains(&dir.join("missing.kdl")));
        assert!(!paths.contains(&dir.join("ignored.kdl")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
