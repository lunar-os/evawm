use anyhow::{Context, Result, bail};
use knuffel::{
    ast::SpannedNode,
    decode::Context as KdlContext,
    errors::DecodeError,
    traits::{Decode, ErrorSpan},
};
use smithay::input::keyboard::{
    Keysym,
    xkb::{KEYSYM_CASE_INSENSITIVE, keysym_from_name},
};
use tracing::warn;

use super::{
    Action, Bind, BindTrigger, Modifiers, WheelDirection,
    parse::{
        arg_i64, arg_string, arg_strings, conversion_error, required_i64, required_string, strict,
    },
};

/// Parse no-argument action names shared between KDL binds and IPC actions.
/// `"quit"` is handled by callers because IPC gates it on `--yes`.
pub(crate) fn parse_simple_action(name: &str) -> Option<Action> {
    Some(match name {
        "close-window" => Action::CloseWindow,
        "focus-left" | "focus-column-left" => Action::FocusLeft,
        "focus-right" | "focus-column-right" => Action::FocusRight,
        "focus-up" | "focus-window-up" => Action::FocusUp,
        "focus-down" | "focus-window-down" => Action::FocusDown,
        "focus-column-first" => Action::FocusColumnFirst,
        "focus-column-last" => Action::FocusColumnLast,
        "move-left" | "move-column-left" => Action::MoveLeft,
        "move-right" | "move-column-right" => Action::MoveRight,
        "move-window-left" => Action::MoveWindowLeft,
        "move-window-right" => Action::MoveWindowRight,
        "move-up" | "move-window-up" => Action::MoveUp,
        "move-down" | "move-window-down" => Action::MoveDown,
        "move-column-to-first" => Action::MoveColumnToFirst,
        "move-column-to-last" => Action::MoveColumnToLast,
        "focus-monitor-left" => Action::FocusMonitorLeft,
        "focus-monitor-right" => Action::FocusMonitorRight,
        "focus-monitor-up" => Action::FocusMonitorUp,
        "focus-monitor-down" => Action::FocusMonitorDown,
        "move-to-monitor-left" => Action::MoveToMonitorLeft,
        "move-to-monitor-right" => Action::MoveToMonitorRight,
        "move-to-monitor-up" => Action::MoveToMonitorUp,
        "move-to-monitor-down" => Action::MoveToMonitorDown,
        "maximize-column" => Action::MaximizeColumn,
        "expand-column-to-available-width" => Action::ExpandColumnToAvailableWidth,
        "center-focused" | "center-column" => Action::CenterFocused,
        "fullscreen-window" => Action::FullscreenWindow,
        "consume-or-expel-window-left" => Action::ConsumeOrExpelLeft,
        "consume-or-expel-window-right" => Action::ConsumeOrExpelRight,
        "toggle-canvas" => Action::ToggleCanvas,
        "toggle-overview" => Action::ToggleOverview,
        "open-overview" => Action::OpenOverview,
        "close-overview" => Action::CloseOverview,
        "home-canvas" => Action::HomeCanvas,
        "reload-config" => Action::ReloadConfig,
        "power-off-monitors" => Action::PowerOffMonitors,
        _ => return None,
    })
}

pub(super) struct BindsPart {
    pub(super) binds: Vec<Bind>,
}

impl<S: ErrorSpan> Decode<S> for BindsPart {
    fn decode_node(
        node: &SpannedNode<S>,
        ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        if !node.arguments.is_empty() || !node.properties.is_empty() {
            let err = DecodeError::unexpected(
                &node.node_name,
                "binds",
                "binds does not take arguments or properties",
            );
            if strict(ctx) {
                return Err(err);
            }
            warn!(error = ?err, "ignoring invalid binds header");
        }
        let mut binds = Vec::new();
        for child in node.children() {
            match Bind::decode_node(child, ctx) {
                Ok(bind) => binds.push(bind),
                Err(err) => {
                    if strict(ctx) {
                        ctx.emit_error(err);
                    } else {
                        warn!(error = ?err, "ignoring invalid bind");
                    }
                }
            }
        }
        Ok(Self { binds })
    }
}

impl<S: ErrorSpan> Decode<S> for Bind {
    fn decode_node(
        node: &SpannedNode<S>,
        ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        if !node.arguments.is_empty() || !node.properties.is_empty() {
            return Err(DecodeError::unexpected(
                &node.node_name,
                "bind",
                "a bind key cannot have arguments or properties",
            ));
        }

        let (mods, trigger) = parse_trigger(&node.node_name)
            .map_err(|err| DecodeError::unexpected(&node.node_name, "key", err.to_string()))?;

        let mut children = node.children();
        let Some(action_node) = children.next() else {
            return Err(DecodeError::missing(node, "bind needs an action"));
        };
        if children.next().is_some() {
            return Err(DecodeError::unexpected(
                &node.node_name,
                "bind",
                "only one action is allowed per bind",
            ));
        }
        let action = Action::decode_node(action_node, ctx)?;
        Ok(Self {
            mods,
            trigger,
            action,
        })
    }
}

impl<S: ErrorSpan> Decode<S> for Action {
    fn decode_node(
        node: &SpannedNode<S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        if !node.properties.is_empty() || node.children.is_some() {
            return Err(DecodeError::unexpected(
                &node.node_name,
                "action",
                "actions do not take properties or child blocks",
            ));
        }
        let name = &**node.node_name;
        if let Some(action) = parse_simple_action(name) {
            expect_arity(node, 0)?;
            return Ok(action);
        }
        let action = match name {
            "spawn" => Action::Spawn(arg_strings(node)?),
            "spawn-sh" => {
                expect_arity(node, 1)?;
                Action::SpawnSh(required_string(node, 0, "spawn-sh needs a command")?)
            }
            "quit" => {
                expect_arity(node, 0)?;
                Action::Quit
            }
            "resize-width" | "set-column-width" => {
                expect_arity(node, 1)?;
                Action::ResizeWidth(parse_percent(
                    arg_string(node, 0).ok_or_else(|| {
                        DecodeError::missing(node, "resize-width needs a percentage")
                    })?,
                    node,
                )?)
            }
            "resize-height" | "set-window-height" => {
                expect_arity(node, 1)?;
                Action::ResizeHeight(parse_percent(
                    arg_string(node, 0).ok_or_else(|| {
                        DecodeError::missing(node, "resize-height needs a percentage")
                    })?,
                    node,
                )?)
            }
            "focus-workspace" => {
                expect_arity(node, 1)?;
                Action::FocusWorkspace(workspace_arg(node, "focus-workspace")?)
            }
            "move-window-to-workspace" => {
                expect_arity(node, 1)?;
                Action::MoveWindowToWorkspace(workspace_arg(node, "move-window-to-workspace")?)
            }
            "move-column-to-workspace" => {
                expect_arity(node, 1)?;
                Action::MoveColumnToWorkspace(workspace_arg(node, "move-column-to-workspace")?)
            }
            "consume-or-expel" => {
                expect_arity(node, 1)?;
                match arg_string(node, 0) {
                    Some("left") => Action::ConsumeOrExpelLeft,
                    Some("right") => Action::ConsumeOrExpelRight,
                    other => {
                        return Err(DecodeError::unexpected(
                            &node.node_name,
                            "action",
                            format!("consume-or-expel expects left or right, got {other:?}"),
                        ));
                    }
                }
            }
            "cycle-window" => {
                expect_arity(node, 1)?;
                match arg_string(node, 0) {
                    Some("next") => Action::CycleWindowNext,
                    Some("previous") | Some("prev") => Action::CycleWindowPrevious,
                    other => {
                        return Err(DecodeError::unexpected(
                            &node.node_name,
                            "action",
                            format!("cycle-window expects next or previous, got {other:?}"),
                        ));
                    }
                }
            }
            "pan-camera" => {
                if node.arguments.len() > 2 {
                    return Err(DecodeError::unexpected(
                        &node.arguments[2].literal,
                        "argument",
                        "pan-camera takes at most 2 arguments",
                    ));
                }
                let direction = required_string(node, 0, "pan-camera needs a direction")?;
                let amount = if node.arguments.len() == 2 {
                    let raw = arg_i64(node, 1).ok_or_else(|| {
                        conversion_error(
                            &node.arguments[1].literal,
                            "pan-camera amount must be an integer",
                        )
                    })?;
                    let amount = i32::try_from(raw).map_err(|_| {
                        conversion_error(
                            &node.arguments[1].literal,
                            "pan-camera amount out of i32 range",
                        )
                    })?;
                    Some(amount)
                } else {
                    None
                };
                match direction.as_str() {
                    "left" => Action::PanCameraLeft(amount),
                    "right" => Action::PanCameraRight(amount),
                    "up" => Action::PanCameraUp(amount),
                    "down" => Action::PanCameraDown(amount),
                    other => {
                        return Err(DecodeError::unexpected(
                            &node.node_name,
                            "action",
                            format!("unknown pan direction {other:?}"),
                        ));
                    }
                }
            }
            other => {
                return Err(DecodeError::unexpected(
                    &node.node_name,
                    "action",
                    format!("unknown action {other:?}"),
                ));
            }
        };
        Ok(action)
    }
}

fn expect_arity<S: ErrorSpan>(
    node: &SpannedNode<S>,
    expected: usize,
) -> std::result::Result<(), DecodeError<S>> {
    if let Some(extra) = node.arguments.get(expected) {
        return Err(DecodeError::unexpected(
            &extra.literal,
            "argument",
            format!("`{}` expects {expected} argument(s)", &**node.node_name),
        ));
    }
    Ok(())
}

fn workspace_arg<S: ErrorSpan>(
    node: &SpannedNode<S>,
    name: &'static str,
) -> std::result::Result<u8, DecodeError<S>> {
    let value = required_i64(node, 0, "missing workspace number")?;
    if (1..=9).contains(&value) {
        Ok(value as u8)
    } else {
        Err(DecodeError::unexpected(
            &node.node_name,
            "workspace",
            format!("{name} expects a workspace from 1 to 9"),
        ))
    }
}

fn parse_percent<S: ErrorSpan>(
    text: &str,
    node: &SpannedNode<S>,
) -> std::result::Result<f64, DecodeError<S>> {
    let value = text.trim_end_matches('%');
    let value = value.parse::<f64>().map_err(|err| {
        conversion_error(
            &node.arguments[0].literal,
            format!("invalid percentage: {err}"),
        )
    })?;
    if !value.is_finite() {
        return Err(conversion_error(
            &node.arguments[0].literal,
            "percentage must be finite",
        ));
    }
    Ok(value / 100.0)
}

fn parse_trigger(text: &str) -> Result<(Modifiers, BindTrigger)> {
    let mut parts = text.split('+').collect::<Vec<_>>();
    let key_name = parts.pop().context("empty key")?;
    if key_name.is_empty() {
        bail!("empty key name");
    }

    let mut mods = Modifiers::default();
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "mod" | "super" | "logo" | "win" => mods.logo = true,
            "ctrl" | "control" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            other => bail!("unknown modifier {other:?}"),
        }
    }

    let trigger = match key_name.to_ascii_lowercase().as_str() {
        "wheelscrollup" => BindTrigger::Wheel(WheelDirection::Up),
        "wheelscrolldown" => BindTrigger::Wheel(WheelDirection::Down),
        "wheelscrollleft" => BindTrigger::Wheel(WheelDirection::Left),
        "wheelscrollright" => BindTrigger::Wheel(WheelDirection::Right),
        _ => {
            let keysym = keysym_from_name(key_name, KEYSYM_CASE_INSENSITIVE);
            if keysym == Keysym::NoSymbol {
                bail!("unknown key {key_name:?}");
            }
            BindTrigger::Key(keysym)
        }
    };

    Ok((mods, trigger))
}
