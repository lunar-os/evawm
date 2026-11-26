use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use anyhow::{Context, Result, bail};
use knuffel::{
    ast::{Literal, SpannedNode},
    decode::{Context as KdlContext, Kind as DecodeKind},
    errors::DecodeError,
    span::{LineSpan, Spanned},
    traits::{Decode, DecodeChildren, DecodeScalar, ErrorSpan},
};
use smithay::{
    backend::renderer::Color32F,
    utils::{Point, Transform},
};
use tracing::{info, warn};

use crate::animation::{AnimationConfig, AnimationsConfig, Curve, Kind, OpenAnimationConfig};
use crate::workspace::Mode;

use super::{
    AccelProfile, BorderConfig, Config, CursorConfig, DEFAULT_CONFIG, MouseButton,
    NewWindowPosition, OutputConfig, OutputModeConfig, XwaylandConfig, binds::BindsPart,
};

// KDL v1 config parser. Section structs are partial overrides: omitted fields
// keep the value from the embedded defaults or previously merged includes.

#[derive(Debug, Clone, Copy)]
struct Flag(bool);

impl<S: ErrorSpan> Decode<S> for Flag {
    fn decode_node(
        node: &SpannedNode<S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        if !node.properties.is_empty() || node.children.is_some() || node.arguments.len() > 1 {
            return Err(DecodeError::unexpected(
                &node.node_name,
                "option",
                "this option takes at most one boolean argument",
            ));
        }
        match node.arguments.first() {
            Some(value) => match &*value.literal {
                Literal::Bool(value) => Ok(Self(*value)),
                _ => Err(DecodeError::scalar_kind(DecodeKind::Bool, &value.literal)),
            },
            None => Ok(Self(true)),
        }
    }
}

#[derive(Debug)]
struct UnknownChild;

impl<S: ErrorSpan> Decode<S> for UnknownChild {
    fn decode_node(
        node: &SpannedNode<S>,
        ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        if strict(ctx) {
            return Err(DecodeError::unexpected(
                &node.node_name,
                "option",
                format!("unknown config option `{}`", &**node.node_name),
            ));
        }
        warn!(
            span = ?node.span(),
            option = %&**node.node_name,
            "ignoring unknown config option"
        );
        Ok(Self)
    }
}

pub(super) fn strict<S: ErrorSpan>(ctx: &KdlContext<S>) -> bool {
    ctx.get::<ParseState>()
        .is_some_and(|state| state.strict.get())
}

fn unknown_section<S: ErrorSpan>(node: &SpannedNode<S>, ctx: &mut KdlContext<S>) {
    if strict(ctx) {
        ctx.emit_error(DecodeError::unexpected(
            &node.node_name,
            "section",
            format!("unknown config section `{}`", &**node.node_name),
        ));
    } else {
        warn!(
            span = ?node.span(),
            section = %&**node.node_name,
            "ignoring unknown config section"
        );
    }
}

#[derive(Clone)]
struct ParseState {
    base: PathBuf,
    config: Rc<RefCell<Config>>,
    strict: Rc<Cell<bool>>,
    sources: Rc<RefCell<Vec<PathBuf>>>,
    include_stack: Vec<PathBuf>,
    record_sources: bool,
}

struct ConfigPart;

impl<S: ErrorSpan> DecodeChildren<S> for ConfigPart {
    fn decode_children(
        nodes: &[SpannedNode<S>],
        ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        let state = ctx
            .get::<ParseState>()
            .expect("parse state context")
            .clone();

        // `strict` is a property of the whole file, so read it before
        // processing sections (and before includes).
        for node in nodes {
            if &**node.node_name == "strict" {
                state.strict.set(Flag::decode_node(node, ctx)?.0);
            }
        }

        for node in nodes {
            match &**node.node_name {
                "strict" => {}
                "include" => handle_include(node, ctx)?,
                "startup" => {
                    let part = StartupPart::decode_node(node, ctx)?;
                    part.apply(&mut state.config.borrow_mut());
                }
                "input" => {
                    let part = InputPart::decode_node(node, ctx)?;
                    part.apply(&mut state.config.borrow_mut());
                }
                "layout" => {
                    let part = LayoutPart::decode_node(node, ctx)?;
                    part.apply(&mut state.config.borrow_mut());
                }
                "animations" => {
                    let part = AnimationsPart::decode_node(node, ctx)?;
                    part.apply(&mut state.config.borrow_mut().animations);
                }
                "xwayland" => {
                    let part = XwaylandPart::decode_node(node, ctx)?;
                    part.apply(&mut state.config.borrow_mut().xwayland);
                }
                "output" => {
                    let part = OutputPart::decode_node(node, ctx)?;
                    part.apply(&mut state.config.borrow_mut().outputs);
                }
                "binds" => {
                    let part = BindsPart::decode_node(node, ctx)?;
                    state.config.borrow_mut().binds = part.binds;
                }
                _ => unknown_section(node, ctx),
            }
        }

        Ok(Self)
    }
}

fn handle_include<S: ErrorSpan>(
    node: &SpannedNode<S>,
    ctx: &mut KdlContext<S>,
) -> std::result::Result<(), DecodeError<S>> {
    if !node.properties.is_empty() || node.children.is_some() || node.arguments.len() > 1 {
        let err = DecodeError::unexpected(
            &node.node_name,
            "include",
            "include takes a single file path string",
        );
        if strict(ctx) {
            return Err(err);
        }
        warn!("{err:?}");
        return Ok(());
    }
    let Some(raw) = arg_string(node, 0) else {
        let err = DecodeError::missing(node, "include needs a file path string");
        if strict(ctx) {
            return Err(err);
        }
        warn!("{err:?}");
        return Ok(());
    };

    let state = ctx
        .get::<ParseState>()
        .expect("parse state context")
        .clone();
    let parent_strict = state.strict.get();
    let path = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        state.base.join(raw)
    };
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());

    if state.include_stack.contains(&canonical) {
        let message = format!("include cycle involving {}", path.display());
        if parent_strict {
            return Err(DecodeError::missing(node, message));
        }
        warn!("{message}");
        return Ok(());
    }
    if state.include_stack.len() > 16 {
        let message = format!("include nesting is too deep at {}", path.display());
        if parent_strict {
            return Err(DecodeError::missing(node, message));
        }
        warn!("{message}");
        return Ok(());
    }

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            let message = format!("failed to read include {}: {err}", path.display());
            if parent_strict {
                return Err(DecodeError::missing(node, message));
            }
            warn!("{message}");
            return Ok(());
        }
    };

    // Keep the unresolved path so the watcher notices when a symlinked include
    // is repointed to a different target file.
    if state.record_sources {
        let mut sources = state.sources.borrow_mut();
        if !sources.contains(&path) {
            sources.push(path.clone());
        }
    }

    let config_before = state.config.borrow().clone();
    let mut include_stack = state.include_stack;
    include_stack.push(canonical);
    let display_path = path.display().to_string();
    // `strict` is strictly per file: each included file starts at default
    // lenient (`false`) and its own `strict true|false` never leaks into the
    // including file or overrides how the parent handles a failed include.
    let child_state = ParseState {
        base: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        config: state.config.clone(),
        strict: Rc::new(Cell::new(false)),
        sources: state.sources,
        include_stack,
        record_sources: state.record_sources,
    };

    let result =
        knuffel::parse_with_context::<ConfigPart, LineSpan, _>(&display_path, &text, |ctx| {
            ctx.set(child_state);
        });

    match result {
        Ok(_) => Ok(()),
        Err(err) => {
            warn!(include = %display_path, error = ?err, "failed to parse included config");
            if parent_strict {
                Err(DecodeError::missing(
                    node,
                    format!("failed to parse include {display_path}"),
                ))
            } else {
                // A failed include must not leave the sections parsed before
                // the error applied to the running configuration.
                *state.config.borrow_mut() = config_before;
                Ok(())
            }
        }
    }
}

fn parse_text(path: &Path, text: &str, config: Config, record_sources: bool) -> Result<Config> {
    let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let canonical = record_sources.then(|| path.canonicalize().ok()).flatten();
    let strict = Rc::new(Cell::new(false));
    let sources = Rc::new(RefCell::new(Vec::new()));
    if record_sources {
        sources.borrow_mut().push(path.to_path_buf());
    }
    let include_stack = canonical.into_iter().collect::<Vec<_>>();
    let config_cell = Rc::new(RefCell::new(config));
    let display_path = path.display().to_string();
    let state = ParseState {
        base,
        config: config_cell.clone(),
        strict,
        sources: sources.clone(),
        include_stack,
        record_sources,
    };

    knuffel::parse_with_context::<ConfigPart, LineSpan, _>(&display_path, text, |ctx| {
        ctx.set(state);
    })
    .map_err(|err| anyhow::anyhow!("{err:?}"))?;

    let mut config = Rc::try_unwrap(config_cell)
        .map_err(|_| anyhow::anyhow!("config parser left aliases"))?
        .into_inner();
    if record_sources {
        config.sources = sources.borrow().clone();
    } else {
        config.sources.clear();
    }
    Ok(config)
}

impl Config {
    /// Load the embedded defaults plus the user's config. Returns the path
    /// that was actually read so the caller can watch it for live reload.
    pub fn load(path: Option<&Path>) -> Result<(Self, Option<PathBuf>)> {
        let config = parse_text(
            Path::new("config.kdl"),
            DEFAULT_CONFIG,
            Self::default(),
            false,
        )?;

        let path = match path {
            Some(path) => Some(path.to_path_buf()),
            None => default_path(),
        };

        match path {
            Some(path) => {
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read config {}", path.display()))?;
                let loaded = parse_text(&path, &text, config, true)?;
                info!(
                    config = %path.display(),
                    binds = loaded.binds.len(),
                    startup = loaded.startup.len(),
                    includes = loaded.sources.len().saturating_sub(1),
                    "config loaded"
                );
                Ok((loaded, Some(path)))
            }
            None => {
                info!(
                    binds = config.binds.len(),
                    startup = config.startup.len(),
                    "embedded config loaded"
                );
                Ok((config, None))
            }
        }
    }

    #[cfg(test)]
    pub fn parse(text: &str) -> Result<Self> {
        parse_text(Path::new("config.kdl"), text, Self::default(), false)
    }

    #[cfg(test)]
    pub(super) fn apply(&mut self, text: &str) -> Result<()> {
        *self = parse_text(Path::new("config.kdl"), text, std::mem::take(self), true)?;
        Ok(())
    }
}
pub(super) fn arg_string<S>(node: &SpannedNode<S>, index: usize) -> Option<&str> {
    match &*node.arguments.get(index)?.literal {
        Literal::String(value) => Some(value),
        _ => None,
    }
}

pub(super) fn arg_i64<S>(node: &SpannedNode<S>, index: usize) -> Option<i64> {
    match &*node.arguments.get(index)?.literal {
        Literal::Int(value) => i64::try_from(value).ok(),
        _ => None,
    }
}

fn arg_f64<S>(node: &SpannedNode<S>, index: usize) -> Option<f64> {
    let value = match &*node.arguments.get(index)?.literal {
        Literal::Int(value) => i64::try_from(value).ok().map(|value| value as f64),
        Literal::Decimal(value) => f64::try_from(value).ok(),
        _ => None,
    }?;
    value.is_finite().then_some(value)
}

pub(super) fn arg_strings<S: ErrorSpan>(
    node: &SpannedNode<S>,
) -> std::result::Result<Vec<String>, DecodeError<S>> {
    if node.arguments.is_empty() {
        return Err(DecodeError::missing(node, "spawn needs a program name"));
    }
    let mut out = Vec::with_capacity(node.arguments.len());
    for value in &node.arguments {
        match &*value.literal {
            Literal::String(text) => out.push(text.to_string()),
            _ => return Err(DecodeError::scalar_kind(DecodeKind::String, &value.literal)),
        }
    }
    Ok(out)
}

pub(super) fn required_string<S: ErrorSpan>(
    node: &SpannedNode<S>,
    index: usize,
    message: &'static str,
) -> std::result::Result<String, DecodeError<S>> {
    arg_string(node, index)
        .map(str::to_string)
        .ok_or_else(|| DecodeError::missing(node, message))
}

pub(super) fn required_i64<S: ErrorSpan>(
    node: &SpannedNode<S>,
    index: usize,
    message: &'static str,
) -> std::result::Result<i64, DecodeError<S>> {
    arg_i64(node, index).ok_or_else(|| DecodeError::missing(node, message))
}

pub(super) fn conversion_error<S: ErrorSpan>(
    value: &Spanned<Literal, S>,
    message: impl Into<String>,
) -> DecodeError<S> {
    DecodeError::conversion(
        value,
        std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FloatValue(f64);

impl<S: ErrorSpan> DecodeScalar<S> for FloatValue {
    fn type_check(_: &Option<Spanned<knuffel::ast::TypeName, S>>, _: &mut KdlContext<S>) {}

    fn raw_decode(
        literal: &Spanned<Literal, S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match &**literal {
            Literal::Int(value) => i64::try_from(value)
                .map(|value| Self(value as f64))
                .map_err(|err| conversion_error(literal, err.to_string())),
            Literal::Decimal(value) => {
                let value = f64::try_from(value)
                    .map_err(|err| conversion_error(literal, err.to_string()))?;
                if !value.is_finite() {
                    return Err(conversion_error(literal, "value must be finite"));
                }
                Ok(Self(value))
            }
            _ => Err(DecodeError::scalar_kind(DecodeKind::Decimal, literal)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ColorValue(Color32F);

impl<S: ErrorSpan> DecodeScalar<S> for ColorValue {
    fn type_check(type_name: &Option<Spanned<knuffel::ast::TypeName, S>>, ctx: &mut KdlContext<S>) {
        if let Some(type_name) = type_name {
            ctx.emit_error(DecodeError::unexpected(
                type_name,
                "type name",
                "no type name expected for a color",
            ));
        }
    }

    fn raw_decode(
        value: &Spanned<Literal, S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match &**value {
            Literal::String(text) => parse_color(text)
                .map(ColorValue)
                .map_err(|err| conversion_error(value, err.to_string())),
            _ => Err(DecodeError::scalar_kind(DecodeKind::String, value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OutputModeValue(OutputModeConfig);

impl<S: ErrorSpan> DecodeScalar<S> for OutputModeValue {
    fn type_check(type_name: &Option<Spanned<knuffel::ast::TypeName, S>>, ctx: &mut KdlContext<S>) {
        if let Some(type_name) = type_name {
            ctx.emit_error(DecodeError::unexpected(
                type_name,
                "type name",
                "no type name expected for an output mode",
            ));
        }
    }

    fn raw_decode(
        value: &Spanned<Literal, S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match &**value {
            Literal::String(text) => parse_output_mode(text)
                .map(OutputModeValue)
                .map_err(|err| conversion_error(value, err.to_string())),
            _ => Err(DecodeError::scalar_kind(DecodeKind::String, value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransformValue(Transform);

impl<S: ErrorSpan> DecodeScalar<S> for TransformValue {
    fn type_check(type_name: &Option<Spanned<knuffel::ast::TypeName, S>>, ctx: &mut KdlContext<S>) {
        if let Some(type_name) = type_name {
            ctx.emit_error(DecodeError::unexpected(
                type_name,
                "type name",
                "no type name expected for an output transform",
            ));
        }
    }

    fn raw_decode(
        value: &Spanned<Literal, S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match &**value {
            Literal::String(text) => parse_output_transform(text)
                .map(TransformValue)
                .ok_or_else(|| conversion_error(value, "unknown output transform")),
            _ => Err(DecodeError::scalar_kind(DecodeKind::String, value)),
        }
    }
}

impl<S: ErrorSpan> DecodeScalar<S> for AccelProfile {
    fn type_check(_: &Option<Spanned<knuffel::ast::TypeName, S>>, _: &mut KdlContext<S>) {}

    fn raw_decode(
        value: &Spanned<Literal, S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match &**value {
            Literal::String(text) if text.eq_ignore_ascii_case("adaptive") => Ok(Self::Adaptive),
            Literal::String(text) if text.eq_ignore_ascii_case("flat") => Ok(Self::Flat),
            Literal::String(_) => Err(conversion_error(value, "expected adaptive or flat")),
            _ => Err(DecodeError::scalar_kind(DecodeKind::String, value)),
        }
    }
}

impl<S: ErrorSpan> DecodeScalar<S> for Mode {
    fn type_check(_: &Option<Spanned<knuffel::ast::TypeName, S>>, _: &mut KdlContext<S>) {}

    fn raw_decode(
        value: &Spanned<Literal, S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match &**value {
            Literal::String(text) if text.eq_ignore_ascii_case("scroll") => Ok(Mode::Scroll),
            Literal::String(text) if text.eq_ignore_ascii_case("canvas") => Ok(Mode::Canvas),
            Literal::String(_) => Err(conversion_error(value, "expected scroll or canvas")),
            _ => Err(DecodeError::scalar_kind(DecodeKind::String, value)),
        }
    }
}

impl<S: ErrorSpan> DecodeScalar<S> for NewWindowPosition {
    fn type_check(_: &Option<Spanned<knuffel::ast::TypeName, S>>, _: &mut KdlContext<S>) {}

    fn raw_decode(
        value: &Spanned<Literal, S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match &**value {
            Literal::String(text) if text.eq_ignore_ascii_case("center") => Ok(Self::Center),
            Literal::String(text)
                if text.eq_ignore_ascii_case("cursor-center")
                    || text.eq_ignore_ascii_case("cursor") =>
            {
                Ok(Self::CursorCenter)
            }
            Literal::String(text) if text.eq_ignore_ascii_case("cursor-top-left") => {
                Ok(Self::CursorTopLeft)
            }
            Literal::String(_) => Err(conversion_error(
                value,
                "expected center, cursor-center, or cursor-top-left",
            )),
            _ => Err(DecodeError::scalar_kind(DecodeKind::String, value)),
        }
    }
}

impl<S: ErrorSpan> DecodeScalar<S> for MouseButton {
    fn type_check(_: &Option<Spanned<knuffel::ast::TypeName, S>>, _: &mut KdlContext<S>) {}

    fn raw_decode(
        value: &Spanned<Literal, S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match &**value {
            Literal::String(text) if text.eq_ignore_ascii_case("none") => Ok(Self::None),
            Literal::String(text) if text.eq_ignore_ascii_case("back") => Ok(Self::Back),
            Literal::String(text) if text.eq_ignore_ascii_case("forward") => Ok(Self::Forward),
            Literal::String(text) if text.eq_ignore_ascii_case("middle") => Ok(Self::Middle),
            Literal::String(_) => Err(conversion_error(
                value,
                "expected none, back, forward, or middle",
            )),
            _ => Err(DecodeError::scalar_kind(DecodeKind::String, value)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CurveSpec(Curve);

impl<S: ErrorSpan> Decode<S> for CurveSpec {
    fn decode_node(
        node: &SpannedNode<S>,
        _ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        if !node.properties.is_empty() || node.children.is_some() {
            return Err(DecodeError::unexpected(
                &node.node_name,
                "curve",
                "curve does not take properties or child blocks",
            ));
        }
        let Some(name) = arg_string(node, 0) else {
            return Err(DecodeError::missing(node, "curve needs a name"));
        };
        let curve = match name {
            "linear" => Curve::Linear,
            "ease" | "ease-in-out" => Curve::EaseInOut,
            "ease-out-quad" => Curve::EaseOutQuad,
            "ease-out-cubic" => Curve::EaseOutCubic,
            "ease-out-expo" => Curve::EaseOutExpo,
            "ease-out-back" => Curve::EaseOutBack,
            "ease-in-out-back" => Curve::EaseInOutBack,
            "cubic-bezier" => {
                if node.arguments.len() > 5 {
                    return Err(DecodeError::unexpected(
                        &node.arguments[5].literal,
                        "argument",
                        "cubic-bezier expects 4 control point coordinates",
                    ));
                }
                let x1 = arg_f64(node, 1)
                    .ok_or_else(|| DecodeError::missing(node, "cubic-bezier needs x1"))?;
                let y1 = arg_f64(node, 2)
                    .ok_or_else(|| DecodeError::missing(node, "cubic-bezier needs y1"))?;
                let x2 = arg_f64(node, 3)
                    .ok_or_else(|| DecodeError::missing(node, "cubic-bezier needs x2"))?;
                let y2 = arg_f64(node, 4)
                    .ok_or_else(|| DecodeError::missing(node, "cubic-bezier needs y2"))?;
                if !(0.0..=1.0).contains(&x1) {
                    return Err(conversion_error(
                        &node.arguments[1].literal,
                        "cubic-bezier x1 must be in 0.0..=1.0",
                    ));
                }
                if !(0.0..=1.0).contains(&x2) {
                    return Err(conversion_error(
                        &node.arguments[3].literal,
                        "cubic-bezier x2 must be in 0.0..=1.0",
                    ));
                }
                return Ok(Self(Curve::CubicBezier { x1, y1, x2, y2 }));
            }
            other => {
                return Err(conversion_error(
                    &node.arguments[0].literal,
                    format!("unknown curve {other:?}"),
                ));
            }
        };
        if let Some(extra) = node.arguments.get(1) {
            return Err(DecodeError::unexpected(
                &extra.literal,
                "argument",
                "named curve takes only 1 argument",
            ));
        }
        Ok(Self(curve))
    }
}

#[derive(knuffel::Decode, Default)]
struct StartupPart {
    #[knuffel(children)]
    entries: Vec<StartupEntry>,
}

enum StartupEntry {
    Spawn(Vec<String>),
    SpawnSh(String),
    Unknown,
}

impl<S: ErrorSpan> knuffel::Decode<S> for StartupEntry {
    fn decode_node(
        node: &SpannedNode<S>,
        ctx: &mut KdlContext<S>,
    ) -> std::result::Result<Self, DecodeError<S>> {
        match node.node_name.as_ref() {
            "spawn" => {
                let part = SpawnPart::decode_node(node, ctx)?;
                Ok(Self::Spawn(part.args))
            }
            "spawn-sh" => {
                let part = ShellPart::decode_node(node, ctx)?;
                Ok(Self::SpawnSh(part.command))
            }
            _ => {
                let _ = UnknownChild::decode_node(node, ctx)?;
                Ok(Self::Unknown)
            }
        }
    }
}

#[derive(knuffel::Decode)]
struct SpawnPart {
    #[knuffel(arguments)]
    args: Vec<String>,
}

#[derive(knuffel::Decode)]
struct ShellPart {
    #[knuffel(argument)]
    command: String,
}

impl StartupPart {
    fn apply(&self, config: &mut Config) {
        for entry in &self.entries {
            match entry {
                StartupEntry::Spawn(args) if !args.is_empty() => {
                    config.startup.push(args.clone());
                }
                StartupEntry::SpawnSh(command) => {
                    config
                        .startup
                        .push(vec!["sh".to_string(), "-c".to_string(), command.clone()]);
                }
                StartupEntry::Spawn(_) | StartupEntry::Unknown => {}
            }
        }
    }
}

#[derive(knuffel::Decode, Default)]
struct InputPart {
    #[knuffel(child)]
    keyboard: Option<KeyboardPart>,
    #[knuffel(child)]
    cursor: Option<CursorPart>,
    #[knuffel(child)]
    mouse: Option<MousePart>,
    #[knuffel(child)]
    touchpad: Option<TouchpadPart>,
    #[knuffel(child)]
    focus_follows_mouse: Option<FocusPart>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct XkbPart {
    #[knuffel(child, unwrap(argument))]
    rules: Option<String>,
    #[knuffel(child, unwrap(argument))]
    model: Option<String>,
    #[knuffel(child, unwrap(argument))]
    layout: Option<String>,
    #[knuffel(child, unwrap(argument))]
    variant: Option<String>,
    #[knuffel(child, unwrap(argument))]
    options: Option<String>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct KeyboardPart {
    #[knuffel(child)]
    xkb: Option<XkbPart>,
    #[knuffel(child, unwrap(argument))]
    repeat_delay: Option<u32>,
    #[knuffel(child, unwrap(argument))]
    repeat_rate: Option<u32>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct CursorPart {
    #[knuffel(child, unwrap(argument))]
    theme: Option<String>,
    #[knuffel(child, unwrap(argument))]
    size: Option<u32>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct MousePart {
    #[knuffel(child, unwrap(argument))]
    accel_profile: Option<AccelProfile>,
    #[knuffel(child, unwrap(argument))]
    accel_speed: Option<FloatValue>,
    #[knuffel(child)]
    natural_scroll: Option<Flag>,
    #[knuffel(child)]
    left_handed: Option<Flag>,
    #[knuffel(child)]
    middle_emulation: Option<Flag>,
    #[knuffel(child, unwrap(argument))]
    pan_button: Option<MouseButton>,
    #[knuffel(child, unwrap(argument))]
    mod_button: Option<MouseButton>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct TouchpadPart {
    #[knuffel(child)]
    tap: Option<Flag>,
    #[knuffel(child)]
    natural_scroll: Option<Flag>,
    #[knuffel(child)]
    left_handed: Option<Flag>,
    #[knuffel(child, unwrap(argument))]
    accel_speed: Option<FloatValue>,
    #[knuffel(child, unwrap(argument))]
    accel_profile: Option<AccelProfile>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct FocusPart {
    #[knuffel(child)]
    scroll: Option<Flag>,
    #[knuffel(child)]
    canvas: Option<Flag>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

impl CursorPart {
    fn apply(&self, cursor: &mut CursorConfig) {
        if let Some(theme) = &self.theme {
            cursor.theme = theme.clone();
        }
        if let Some(size) = self.size {
            cursor.size = size.clamp(8, 256);
        }
    }
}

impl InputPart {
    fn apply(&self, config: &mut Config) {
        if let Some(keyboard) = &self.keyboard {
            if let Some(xkb) = &keyboard.xkb {
                if let Some(rules) = &xkb.rules {
                    config.input.xkb.rules = rules.clone();
                }
                if let Some(model) = &xkb.model {
                    config.input.xkb.model = model.clone();
                }
                if let Some(layout) = &xkb.layout {
                    config.input.xkb.layout = layout.clone();
                }
                if let Some(variant) = &xkb.variant {
                    config.input.xkb.variant = variant.clone();
                }
                if let Some(options) = &xkb.options {
                    config.input.xkb.options = (!options.is_empty()).then(|| options.clone());
                }
            }
            if let Some(delay) = keyboard.repeat_delay {
                config.input.repeat_delay = delay.min(60_000);
            }
            if let Some(rate) = keyboard.repeat_rate {
                config.input.repeat_rate = rate.min(1_000);
            }
        }
        if let Some(cursor) = &self.cursor {
            cursor.apply(&mut config.cursor);
        }
        if let Some(mouse) = &self.mouse {
            if let Some(profile) = mouse.accel_profile {
                config.input.mouse.accel_profile = Some(profile);
            }
            if let Some(speed) = mouse.accel_speed {
                config.input.mouse.accel_speed = speed.0.clamp(-1.0, 1.0);
            }
            if let Some(flag) = mouse.natural_scroll {
                config.input.mouse.natural_scroll = flag.0;
            }
            if let Some(flag) = mouse.left_handed {
                config.input.mouse.left_handed = flag.0;
            }
            if let Some(flag) = mouse.middle_emulation {
                config.input.mouse.middle_emulation = flag.0;
            }
            if let Some(button) = mouse.pan_button {
                config.canvas.pan_button = button;
            }
            if let Some(button) = mouse.mod_button {
                config.canvas.mod_button = button;
            }
        }
        if let Some(touchpad) = &self.touchpad {
            if let Some(flag) = touchpad.tap {
                config.input.touchpad.tap = flag.0;
            }
            if let Some(flag) = touchpad.natural_scroll {
                config.input.touchpad.natural_scroll = flag.0;
            }
            if let Some(flag) = touchpad.left_handed {
                config.input.touchpad.left_handed = flag.0;
            }
            if let Some(speed) = touchpad.accel_speed {
                config.input.touchpad.accel_speed = speed.0.clamp(-1.0, 1.0);
            }
            if let Some(profile) = touchpad.accel_profile {
                config.input.touchpad.accel_profile = Some(profile);
            }
        }
        if let Some(focus) = &self.focus_follows_mouse {
            if let Some(flag) = focus.scroll {
                config.input.focus_follows_mouse.scroll = flag.0;
            }
            if let Some(flag) = focus.canvas {
                config.input.focus_follows_mouse.canvas = flag.0;
            }
        }
    }
}

#[derive(knuffel::Decode, Default)]
struct LayoutPart {
    #[knuffel(child, unwrap(argument))]
    gaps: Option<FloatValue>,
    #[knuffel(child)]
    inner_border: Option<BorderPart>,
    #[knuffel(child)]
    outer_border: Option<BorderPart>,
    #[knuffel(child, unwrap(argument))]
    background_color: Option<ColorValue>,
    #[knuffel(child)]
    default_column_width: Option<DefaultColumnPart>,
    #[knuffel(child, unwrap(argument))]
    default_mode: Option<Mode>,
    #[knuffel(child)]
    canvas: Option<CanvasPart>,
    #[knuffel(child)]
    overview: Option<OverviewPart>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct BorderPart {
    #[knuffel(child, unwrap(argument))]
    width: Option<FloatValue>,
    #[knuffel(child, unwrap(argument))]
    active_color: Option<ColorValue>,
    #[knuffel(child, unwrap(argument))]
    inactive_color: Option<ColorValue>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct DefaultColumnPart {
    #[knuffel(child, unwrap(argument))]
    proportion: Option<FloatValue>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct CanvasPart {
    #[knuffel(child, unwrap(argument))]
    pan_step: Option<FloatValue>,
    #[knuffel(child, unwrap(argument))]
    move_step: Option<FloatValue>,
    #[knuffel(child, unwrap(argument))]
    new_window_width: Option<FloatValue>,
    #[knuffel(child, unwrap(argument))]
    new_window_height: Option<FloatValue>,
    #[knuffel(child, unwrap(argument))]
    new_window_position: Option<NewWindowPosition>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct OverviewPart {
    #[knuffel(child, unwrap(argument))]
    zoom: Option<FloatValue>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

impl BorderPart {
    fn apply(&self, border: &mut BorderConfig) {
        if let Some(width) = self.width {
            border.width = width.0.max(0.0);
        }
        if let Some(color) = self.active_color {
            border.active = color.0;
        }
        if let Some(color) = self.inactive_color {
            border.inactive = color.0;
        }
    }
}

impl CanvasPart {
    fn apply(&self, config: &mut Config) {
        if let Some(step) = self.pan_step {
            config.canvas.pan_step = step.0.max(1.0);
        }
        if let Some(step) = self.move_step {
            config.canvas.move_step = step.0.max(1.0);
        }
        if let Some(value) = self.new_window_width {
            config.canvas.new_window_width = value.0.clamp(0.05, 1.0);
        }
        if let Some(value) = self.new_window_height {
            config.canvas.new_window_height = value.0.clamp(0.05, 1.0);
        }
        if let Some(position) = self.new_window_position {
            config.canvas.new_window_position = position;
        }
    }
}

impl LayoutPart {
    fn apply(&self, config: &mut Config) {
        if let Some(gaps) = self.gaps {
            config.layout.gaps = gaps.0.max(0.0);
        }
        if let Some(border) = &self.inner_border {
            border.apply(&mut config.layout.inner_border);
        }
        if let Some(border) = &self.outer_border {
            border.apply(&mut config.layout.outer_border);
        }
        if let Some(color) = self.background_color {
            config.layout.background = color.0;
        }
        if let Some(column) = &self.default_column_width
            && let Some(proportion) = column.proportion
        {
            config.layout.default_column_proportion = proportion.0.clamp(0.05, 1.0);
        }
        if let Some(mode) = self.default_mode {
            config.layout.default_mode = mode;
        }
        if let Some(canvas) = &self.canvas {
            canvas.apply(config);
        }
        if let Some(overview) = &self.overview
            && let Some(zoom) = overview.zoom
        {
            config.layout.overview.zoom = zoom.0.clamp(0.1, 0.95);
        }
    }
}

#[derive(knuffel::Decode, Default)]
struct XwaylandPart {
    #[knuffel(child)]
    enabled: Option<Flag>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

impl XwaylandPart {
    fn apply(&self, config: &mut XwaylandConfig) {
        if let Some(enabled) = self.enabled {
            config.enabled = enabled.0;
        }
    }
}

#[derive(knuffel::Decode, Default)]
struct AnimationsPart {
    #[knuffel(child)]
    off: Option<Flag>,
    #[knuffel(child, unwrap(argument))]
    slowdown: Option<FloatValue>,
    #[knuffel(child)]
    view_movement: Option<AnimationPart>,
    #[knuffel(child)]
    window_movement: Option<AnimationPart>,
    #[knuffel(child)]
    window_resize: Option<AnimationPart>,
    #[knuffel(child)]
    window_open: Option<OpenPart>,
    #[knuffel(child)]
    window_close: Option<ClosePart>,
    #[knuffel(child)]
    overview: Option<AnimationPart>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct SpringPart {
    #[knuffel(property)]
    damping_ratio: Option<FloatValue>,
    #[knuffel(property)]
    stiffness: Option<FloatValue>,
    #[knuffel(property)]
    epsilon: Option<FloatValue>,
}

#[derive(knuffel::Decode, Default)]
struct AnimationPart {
    #[knuffel(child)]
    off: Option<Flag>,
    #[knuffel(child)]
    spring: Option<SpringPart>,
    #[knuffel(child, unwrap(argument))]
    duration_ms: Option<u64>,
    #[knuffel(child)]
    curve: Option<CurveSpec>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct OpenPart {
    #[knuffel(child)]
    off: Option<Flag>,
    #[knuffel(child)]
    spring: Option<SpringPart>,
    #[knuffel(child, unwrap(argument))]
    duration_ms: Option<u64>,
    #[knuffel(child)]
    curve: Option<CurveSpec>,
    #[knuffel(child, unwrap(argument))]
    scale_from: Option<FloatValue>,
    #[knuffel(child, unwrap(argument))]
    opacity_from: Option<FloatValue>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode, Default)]
struct ClosePart {
    #[knuffel(child)]
    off: Option<Flag>,
    #[knuffel(child)]
    spring: Option<SpringPart>,
    #[knuffel(child, unwrap(argument))]
    duration_ms: Option<u64>,
    #[knuffel(child)]
    curve: Option<CurveSpec>,
    #[knuffel(child, unwrap(argument))]
    scale_to: Option<FloatValue>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

impl AnimationPart {
    fn apply(&self, current: &mut AnimationConfig) {
        apply_animation(
            current,
            self.off,
            self.spring.as_ref(),
            self.duration_ms,
            self.curve.as_ref(),
        );
    }
}

fn apply_animation(
    current: &mut AnimationConfig,
    off: Option<Flag>,
    spring: Option<&SpringPart>,
    duration_ms: Option<u64>,
    curve: Option<&CurveSpec>,
) {
    if let Some(off) = off {
        current.off = off.0;
    }

    if let Some(spring) = spring {
        let (default_damping, default_stiffness, default_epsilon) = match current.kind {
            Kind::Spring {
                damping_ratio,
                stiffness,
                epsilon,
            } => (damping_ratio, stiffness, epsilon),
            Kind::Easing { .. } => (0.9, 900.0, 0.0007),
        };
        current.kind = Kind::Spring {
            damping_ratio: spring
                .damping_ratio
                .map_or(default_damping, |v| v.0)
                .clamp(0.05, 10.0),
            stiffness: spring
                .stiffness
                .map_or(default_stiffness, |v| v.0)
                .clamp(1.0, 100_000.0),
            epsilon: spring
                .epsilon
                .map_or(default_epsilon, |v| v.0)
                .clamp(0.000001, 10.0),
        };
        return;
    }

    if duration_ms.is_some() || curve.is_some() {
        let (default_duration, default_curve) = match current.kind {
            Kind::Easing { duration_ms, curve } => (duration_ms, curve),
            Kind::Spring { .. } => (150, Curve::EaseOutQuad),
        };
        current.kind = Kind::Easing {
            duration_ms: duration_ms.unwrap_or(default_duration),
            curve: curve.map_or(default_curve, |curve| curve.0),
        };
    }
}

impl OpenPart {
    fn apply(&self, config: &mut OpenAnimationConfig) {
        apply_animation(
            &mut config.animation,
            self.off,
            self.spring.as_ref(),
            self.duration_ms,
            self.curve.as_ref(),
        );
        if let Some(scale) = self.scale_from {
            config.scale_from = scale.0.clamp(0.0, 1.0);
        }
        if let Some(opacity) = self.opacity_from {
            config.opacity_from = opacity.0.clamp(0.0, 1.0);
        }
    }
}

impl ClosePart {
    fn apply(&self, config: &mut AnimationConfig, target_scale: &mut f64) {
        apply_animation(
            config,
            self.off,
            self.spring.as_ref(),
            self.duration_ms,
            self.curve.as_ref(),
        );
        if let Some(scale) = self.scale_to {
            *target_scale = scale.0.clamp(0.0, 1.0);
        }
    }
}

impl AnimationsPart {
    fn apply(&self, config: &mut AnimationsConfig) {
        if let Some(off) = self.off {
            config.off = off.0;
        }
        if let Some(slowdown) = self.slowdown {
            config.slowdown = slowdown.0.clamp(0.05, 100.0);
        }
        if let Some(part) = &self.view_movement {
            part.apply(&mut config.view_movement);
        }
        if let Some(part) = &self.window_movement {
            part.apply(&mut config.window_movement);
        }
        if let Some(part) = &self.window_resize {
            part.apply(&mut config.window_resize);
        }
        if let Some(part) = &self.window_open {
            part.apply(&mut config.window_open);
        }
        if let Some(part) = &self.window_close {
            part.apply(&mut config.window_close, &mut config.window_close_scale);
        }
        if let Some(part) = &self.overview {
            part.apply(&mut config.overview);
        }
    }
}

#[derive(knuffel::Decode)]
struct OutputPart {
    #[knuffel(argument)]
    name: String,
    #[knuffel(child)]
    off: Option<Flag>,
    #[knuffel(child, unwrap(argument))]
    mode: Option<OutputModeValue>,
    #[knuffel(child)]
    position: Option<PositionPart>,
    #[knuffel(child, unwrap(argument))]
    transform: Option<TransformValue>,
    #[knuffel(children)]
    _unknown: Vec<UnknownChild>,
}

#[derive(knuffel::Decode)]
struct PositionPart {
    #[knuffel(property)]
    x: i32,
    #[knuffel(property)]
    y: i32,
}

impl OutputPart {
    fn apply(&self, outputs: &mut Vec<OutputConfig>) {
        let output =
            if let Some(output) = outputs.iter_mut().find(|output| output.name == self.name) {
                output
            } else {
                outputs.push(OutputConfig {
                    name: self.name.clone(),
                    off: false,
                    mode: None,
                    position: None,
                    transform: None,
                });
                outputs.last_mut().expect("just pushed output")
            };

        if let Some(off) = self.off {
            output.off = off.0;
        }
        if let Some(mode) = self.mode {
            output.mode = Some(mode.0);
        }
        if let Some(position) = &self.position {
            output.position = Some(Point::from((position.x, position.y)));
        }
        if let Some(transform) = self.transform {
            output.transform = Some(transform.0);
        }
    }
}

fn parse_output_mode(text: &str) -> Result<OutputModeConfig> {
    let (resolution, refresh) = match text.split_once('@') {
        Some((resolution, refresh)) => (resolution, Some(refresh)),
        None => (text, None),
    };
    let (width, height) = resolution
        .split_once('x')
        .with_context(|| format!("expected WIDTHxHEIGHT, got {text:?}"))?;
    let width = width.parse::<i32>().context("invalid output width")?;
    let height = height.parse::<i32>().context("invalid output height")?;
    if width <= 0 || height <= 0 {
        bail!("output width and height must be positive");
    }
    let refresh = match refresh {
        Some(refresh) => {
            let hz = refresh
                .parse::<f64>()
                .with_context(|| format!("invalid output refresh {refresh:?}"))?;
            if !hz.is_finite() || hz <= 0.0 {
                bail!("output refresh must be a positive finite number");
            }
            let millihertz = (hz * 1000.0).round();
            if millihertz > f64::from(i32::MAX) {
                bail!("output refresh is too large");
            }
            Some(millihertz as i32)
        }
        None => None,
    };
    Ok(OutputModeConfig {
        width,
        height,
        refresh,
    })
}

fn parse_output_transform(text: &str) -> Option<Transform> {
    match text.to_ascii_lowercase().as_str() {
        "normal" => Some(Transform::Normal),
        "90" => Some(Transform::_90),
        "180" => Some(Transform::_180),
        "270" => Some(Transform::_270),
        "flipped" => Some(Transform::Flipped),
        "flipped-90" => Some(Transform::Flipped90),
        "flipped-180" => Some(Transform::Flipped180),
        "flipped-270" => Some(Transform::Flipped270),
        _ => None,
    }
}

pub(super) fn parse_color(text: &str) -> Result<Color32F> {
    let hex = text
        .strip_prefix('#')
        .with_context(|| format!("color {text:?} must start with '#'"))?;
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("color {text:?} must contain only ASCII hex digits");
    }
    let (rgb, alpha) = match hex.len() {
        6 => (hex, 255),
        8 => (&hex[..6], u8::from_str_radix(&hex[6..], 16)?),
        _ => bail!("color {text:?} must be #rrggbb or #rrggbbaa"),
    };
    let r = u8::from_str_radix(&rgb[0..2], 16)? as f32 / 255.0;
    let g = u8::from_str_radix(&rgb[2..4], 16)? as f32 / 255.0;
    let b = u8::from_str_radix(&rgb[4..6], 16)? as f32 / 255.0;
    let a = alpha as f32 / 255.0;

    // Color32F is premultiplied, so scale RGB by alpha up front.
    Ok(Color32F::new(r * a, g * a, b * a, a))
}

pub(super) fn default_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
        && Path::new(&dir).is_absolute()
    {
        let path = PathBuf::from(dir).join("evawm/config.kdl");
        if path.exists() {
            return Some(path);
        }
    }
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        let path = PathBuf::from(home).join(".config/evawm/config.kdl");
        if path.exists() {
            return Some(path);
        }
    }
    None
}
