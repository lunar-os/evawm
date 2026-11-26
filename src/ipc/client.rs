//! `evawm msg`: one-shot queries, actions, and the event stream.

use std::{
    fmt,
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub fn run(args: &[String]) -> Result<()> {
    match run_inner(args) {
        // `evawm msg ... | head` closes stdout early. That is a normal way to
        // stop reading, not an error, and must never panic the client.
        Err(error) if is_broken_pipe(&error) => Ok(()),
        result => result,
    }
}

const USAGE: &str = "usage: evawm msg <outputs|workspaces|windows|focused|version|action|subscribe> [--json|-j] [--yes]";

fn run_inner(args: &[String]) -> Result<()> {
    let mut json_output = false;
    let mut yes = false;
    let mut positional = Vec::new();
    let mut end_of_flags = false;
    for arg in args {
        let in_spawn_args = positional.first().map(String::as_str) == Some("action")
            && matches!(
                positional.get(1).map(String::as_str),
                Some("spawn" | "spawn-sh")
            );
        if end_of_flags || in_spawn_args {
            positional.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end_of_flags = true,
            "--json" | "-j" => json_output = true,
            "--yes" => yes = true,
            "--help" | "-h" => {
                write_out(format_args!("{USAGE}"))?;
                return Ok(());
            }
            other => positional.push(other.to_string()),
        }
    }

    let Some(command) = positional.first().cloned() else {
        bail!("{USAGE}");
    };

    let mut request = json!({"id": 1, "cmd": command});
    match command.as_str() {
        "action" => {
            let Some(name) = positional.get(1).cloned() else {
                bail!("usage: evawm msg action <name> [args...]");
            };
            let mut action_args = positional[2..]
                .iter()
                .map(|arg| Value::String(arg.clone()))
                .collect::<Vec<_>>();
            if matches!(
                name.as_str(),
                "focus-workspace"
                    | "focus-workspace-id"
                    | "move-window-to-workspace"
                    | "move-column-to-workspace"
                    | "focus-window"
            ) {
                if let Some(value) = positional
                    .get(2)
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    action_args[0] = json!(value);
                }
            } else if name == "pan-camera"
                && let Some(raw) = positional.get(3)
            {
                let amount = raw.parse::<i64>().with_context(|| {
                    format!("pan-camera amount must be an integer, got {raw:?}")
                })?;
                action_args[1] = json!(amount);
            }
            request = json!({
                "id": 1,
                "cmd": "action",
                "action": name,
                "args": action_args,
                "yes": yes,
            });
        }
        "subscribe" | "outputs" | "workspaces" | "windows" | "focused" | "version" => {
            if let Some(extra) = positional.get(1) {
                bail!("unexpected argument {extra:?} for `{command}`");
            }
        }
        other => bail!("unknown command {other:?}"),
    }

    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("failed to connect to {}", path.display()))?;
    writeln!(stream, "{request}")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line).context("invalid IPC response")?;
    if response["ok"] != true {
        bail!(
            "{}",
            response["error"].as_str().unwrap_or("IPC request failed")
        );
    }

    if command == "subscribe" {
        return subscribe(reader, response["data"].clone(), json_output);
    }

    if json_output {
        write_out(format_args!(
            "{}",
            serde_json::to_string_pretty(&response["data"])?
        ))?;
    } else {
        print_human(&command, &response["data"])?;
    }
    Ok(())
}

fn subscribe(reader: BufReader<UnixStream>, initial: Value, json_output: bool) -> Result<()> {
    if json_output {
        write_out(format_args!(
            "{}",
            json!({"event": "state", "data": initial})
        ))?;
    } else {
        print_human("workspaces", &initial)?;
    }

    for line in reader.lines() {
        let line = line?;
        if json_output {
            write_out(format_args!("{line}"))?;
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(data) = event.get("data") {
            print_human("workspaces", data)?;
        }
    }
    Ok(())
}

/// Write one line of command output without panicking when the reader is gone.
fn write_out(arguments: fmt::Arguments<'_>) -> Result<()> {
    writeln!(io::stdout().lock(), "{arguments}")?;
    Ok(())
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}

fn print_human(command: &str, data: &Value) -> Result<()> {
    match command {
        "outputs" => {
            for output in data.as_array().into_iter().flatten() {
                let refresh = output["refresh_mhz"]
                    .as_i64()
                    .filter(|&mhz| mhz > 0)
                    .map(|mhz| format!("@{}", (mhz + 500) / 1000))
                    .unwrap_or_default();
                write_out(format_args!(
                    "{}: {}x{}{} at {},{}{}{} workspace {}",
                    output["name"].as_str().unwrap_or("?"),
                    output["width"].as_i64().unwrap_or(0),
                    output["height"].as_i64().unwrap_or(0),
                    refresh,
                    output["x"].as_i64().unwrap_or(0),
                    output["y"].as_i64().unwrap_or(0),
                    if output["transform"].as_str() == Some("normal") {
                        ""
                    } else {
                        " transformed"
                    },
                    if output["active"].as_bool() == Some(true) {
                        " focused"
                    } else {
                        ""
                    },
                    output["active_workspace_index"].as_u64().unwrap_or(0),
                ))?;
            }
        }
        "workspaces" => {
            let workspaces = data["workspaces"]
                .as_array()
                .or_else(|| data.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            for workspace in workspaces {
                write_out(format_args!(
                    "{} workspace {}{}{} {} ({} windows)",
                    workspace["output"].as_str().unwrap_or("?"),
                    workspace["index"].as_u64().unwrap_or(0),
                    if workspace["active"].as_bool() == Some(true) {
                        " active"
                    } else {
                        ""
                    },
                    if workspace["focused"].as_bool() == Some(true) {
                        " focused"
                    } else {
                        ""
                    },
                    workspace["mode"].as_str().unwrap_or("?"),
                    workspace["window_count"].as_u64().unwrap_or(0),
                ))?;
            }
        }
        "windows" => {
            for window in data.as_array().into_iter().flatten() {
                write_out(format_args!(
                    "{} {} \"{}\"{}",
                    window["id"].as_u64().unwrap_or(0),
                    window["app_id"].as_str().unwrap_or("?"),
                    window["title"].as_str().unwrap_or(""),
                    if window["focused"].as_bool() == Some(true) {
                        " focused"
                    } else {
                        ""
                    },
                ))?;
            }
        }
        "focused" => {
            if data.is_null() {
                write_out(format_args!("No focused window"))?;
            } else {
                write_out(format_args!(
                    "{} {} \"{}\" on {} workspace {}",
                    data["id"].as_u64().unwrap_or(0),
                    data["app_id"].as_str().unwrap_or("?"),
                    data["title"].as_str().unwrap_or(""),
                    data["output"].as_str().unwrap_or("?"),
                    data["workspace_index"].as_u64().unwrap_or(0),
                ))?;
            }
        }
        "version" => {
            write_out(format_args!(
                "EvaWM {} protocol {} pid {}",
                data["version"].as_str().unwrap_or("?"),
                data["protocol"].as_u64().unwrap_or(0),
                data["pid"].as_u64().unwrap_or(0),
            ))?;
        }
        other => write_out(format_args!("{other}: {}", data))?,
    }
    Ok(())
}

fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("EVAWM_SOCKET").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is unset")?;
    let display = std::env::var_os("WAYLAND_DISPLAY")
        .context("EVAWM_SOCKET is unset and WAYLAND_DISPLAY is not set")?;
    Ok(runtime.join(format!(
        "evawm-{}.sock",
        display.to_string_lossy().replace('/', "_")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_pipe_is_treated_as_a_clean_exit() {
        let error = anyhow::Error::from(io::Error::from(io::ErrorKind::BrokenPipe));
        assert!(is_broken_pipe(&error));
        let wrapped = error.context("writing output");
        assert!(is_broken_pipe(&wrapped));
    }

    #[test]
    fn other_output_errors_stay_errors() {
        let error = anyhow::Error::from(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(!is_broken_pipe(&error));
    }
}
