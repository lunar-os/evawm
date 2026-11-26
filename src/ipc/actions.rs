//! Parse the IPC `action` payload into the existing keyboard action enum.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::Action;

pub enum IpcAction {
    Action(Action),
    FocusWindow(u64),
    FocusWorkspaceId(u64),
}

pub fn parse(name: &str, args: &[Value], yes: bool) -> Result<IpcAction> {
    if let Some(action) = crate::config::binds::parse_simple_action(name) {
        expect_args(name, args, 0)?;
        return Ok(IpcAction::Action(action));
    }
    let action = match name {
        "spawn" => {
            if args.is_empty() {
                bail!("spawn needs a program name");
            }
            Action::Spawn(string_args(args)?)
        }
        "spawn-sh" => {
            expect_args(name, args, 1)?;
            let command = string_arg(args, 0)?;
            if command.trim().is_empty() {
                bail!("spawn-sh needs a non-empty command");
            }
            Action::SpawnSh(command)
        }
        "quit" => {
            expect_args(name, args, 0)?;
            if !yes {
                bail!("quit requires --yes");
            }
            Action::Quit
        }
        "resize-width" | "set-column-width" => {
            expect_args(name, args, 1)?;
            Action::ResizeWidth(percent_arg(args, "resize-width")?)
        }
        "resize-height" | "set-window-height" => {
            expect_args(name, args, 1)?;
            Action::ResizeHeight(percent_arg(args, "resize-height")?)
        }
        "focus-workspace" => {
            expect_args(name, args, 1)?;
            Action::FocusWorkspace(workspace_arg(args, name)?)
        }
        "move-window-to-workspace" => {
            expect_args(name, args, 1)?;
            Action::MoveWindowToWorkspace(workspace_arg(args, name)?)
        }
        "move-column-to-workspace" => {
            expect_args(name, args, 1)?;
            Action::MoveColumnToWorkspace(workspace_arg(args, name)?)
        }
        "consume-or-expel" => {
            expect_args(name, args, 1)?;
            match string_arg(args, 0)?.as_str() {
                "left" => Action::ConsumeOrExpelLeft,
                "right" => Action::ConsumeOrExpelRight,
                other => bail!("consume-or-expel expects left or right, got {other:?}"),
            }
        }
        "cycle-window" => {
            expect_args(name, args, 1)?;
            match string_arg(args, 0)?.as_str() {
                "next" => Action::CycleWindowNext,
                "previous" | "prev" => Action::CycleWindowPrevious,
                other => bail!("cycle-window expects next or previous, got {other:?}"),
            }
        }
        "pan-camera" => {
            if args.is_empty() || args.len() > 2 {
                bail!("pan-camera expects 1 or 2 arguments");
            }
            let direction = string_arg(args, 0)?;
            let amount = match args.get(1) {
                Some(value) => {
                    let raw = value
                        .as_i64()
                        .context("pan-camera amount must be an integer")?;
                    let amount = i32::try_from(raw)
                        .map_err(|_| anyhow::anyhow!("pan-camera amount out of i32 range"))?;
                    Some(amount)
                }
                None => None,
            };
            match direction.as_str() {
                "left" => Action::PanCameraLeft(amount),
                "right" => Action::PanCameraRight(amount),
                "up" => Action::PanCameraUp(amount),
                "down" => Action::PanCameraDown(amount),
                other => bail!("unknown pan direction {other:?}"),
            }
        }
        "focus-window" => {
            expect_args(name, args, 1)?;
            let id = args
                .first()
                .and_then(Value::as_u64)
                .context("focus-window needs a numeric window id")?;
            return Ok(IpcAction::FocusWindow(id));
        }
        "focus-workspace-id" => {
            expect_args(name, args, 1)?;
            let id = args
                .first()
                .and_then(Value::as_u64)
                .context("focus-workspace-id needs a numeric workspace id")?;
            return Ok(IpcAction::FocusWorkspaceId(id));
        }
        other => bail!("unknown action {other:?}"),
    };
    Ok(IpcAction::Action(action))
}

fn expect_args(name: &str, args: &[Value], expected: usize) -> Result<()> {
    if args.len() != expected {
        bail!("{name} expects {expected} argument(s), got {}", args.len());
    }
    Ok(())
}

fn string_arg(args: &[Value], index: usize) -> Result<String> {
    args.get(index)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("missing string argument {index}"))
}

fn string_args(args: &[Value]) -> Result<Vec<String>> {
    args.iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .context("spawn arguments must be strings")
        })
        .collect()
}

fn workspace_arg(args: &[Value], name: &str) -> Result<u8> {
    let value = args
        .first()
        .and_then(Value::as_u64)
        .with_context(|| format!("{name} needs a workspace number"))?;
    if !(1..=9).contains(&value) {
        bail!("{name} expects a workspace from 1 to 9");
    }
    Ok(value as u8)
}

fn percent_arg(args: &[Value], name: &str) -> Result<f64> {
    let text = string_arg(args, 0)?;
    let value = text.trim_end_matches('%');
    let parsed: f64 = value
        .parse()
        .with_context(|| format!("{name} needs a percentage, got {text:?}"))?;
    if !parsed.is_finite() {
        bail!("{name} needs a finite percentage, got {text:?}");
    }
    Ok(parsed / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_and_focus_actions_parse() {
        assert!(matches!(
            parse("focus-workspace", &[json!(2)], false).unwrap(),
            IpcAction::Action(Action::FocusWorkspace(2))
        ));
        assert!(matches!(
            parse("focus-window", &[json!(7)], false).unwrap(),
            IpcAction::FocusWindow(7)
        ));
        assert!(matches!(
            parse("focus-workspace-id", &[json!(9)], false).unwrap(),
            IpcAction::FocusWorkspaceId(9)
        ));
        assert!(matches!(
            parse("move-window-to-workspace", &[json!(3)], false).unwrap(),
            IpcAction::Action(Action::MoveWindowToWorkspace(3))
        ));
        assert!(matches!(
            parse("expand-column-to-available-width", &[], false).unwrap(),
            IpcAction::Action(Action::ExpandColumnToAvailableWidth)
        ));
        assert!(matches!(
            parse("toggle-overview", &[], false).unwrap(),
            IpcAction::Action(Action::ToggleOverview)
        ));
        assert!(parse("overview", &[], false).is_err());
        assert!(matches!(
            parse("open-overview", &[], false).unwrap(),
            IpcAction::Action(Action::OpenOverview)
        ));
        assert!(matches!(
            parse("close-overview", &[], false).unwrap(),
            IpcAction::Action(Action::CloseOverview)
        ));
    }

    #[test]
    fn quit_requires_confirmation() {
        assert!(parse("quit", &[], false).is_err());
        assert!(matches!(
            parse("quit", &[], true).unwrap(),
            IpcAction::Action(Action::Quit)
        ));
    }

    #[test]
    fn resize_and_power_actions_parse() {
        assert!(matches!(
            parse("resize-width", &[json!("-10%")], false).unwrap(),
            IpcAction::Action(Action::ResizeWidth(value)) if (value + 0.1).abs() < 1e-9
        ));
        assert!(matches!(
            parse("power-off-monitors", &[], false).unwrap(),
            IpcAction::Action(Action::PowerOffMonitors)
        ));
        assert!(matches!(
            parse("spawn", &[json!("foot")], false).unwrap(),
            IpcAction::Action(Action::Spawn(args)) if args == vec!["foot".to_string()]
        ));
    }

    #[test]
    fn bad_action_names_fail() {
        assert!(parse("no-such-action", &[], false).is_err());
    }

    #[test]
    fn non_finite_resize_percentages_are_rejected() {
        assert!(parse("resize-width", &[json!("NaN")], false).is_err());
        assert!(parse("resize-width", &[json!("inf%")], false).is_err());
        assert!(parse("resize-height", &[json!("-inf")], false).is_err());
    }

    #[test]
    fn invalid_and_extra_arguments_are_rejected() {
        assert!(parse("close-window", &[json!("extra")], false).is_err());
        assert!(parse("spawn", &[], false).is_err());
        assert!(parse("spawn-sh", &[json!("   ")], false).is_err());
        assert!(parse("spawn-sh", &[json!("echo"), json!("hi")], false).is_err());
        assert!(parse("pan-camera", &[json!("left"), json!("abc")], false).is_err());
        assert!(parse("pan-camera", &[json!("left"), json!(i64::MAX)], false).is_err());
        assert!(matches!(
            parse("pan-camera", &[json!("left"), json!(120)], false).unwrap(),
            IpcAction::Action(Action::PanCameraLeft(Some(120)))
        ));
    }
}
