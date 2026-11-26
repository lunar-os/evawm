mod animation;
mod borders;
mod config;
mod config_watcher;
mod cursor;
mod dnd;
mod drag;
mod gamma;
mod handlers;
mod input;
mod ipc;
mod layout;
mod reload;
mod render;
mod screencopy;
mod session;
mod session_env;
mod signals;
mod snapshot;
mod spawn;
mod state;
mod window_state;
mod workspace;
mod workspaces;
mod xwayland;

#[cfg(feature = "nested")]
use anyhow::anyhow;
use anyhow::{Context, Result, bail};
#[cfg(feature = "nested")]
use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{calloop::EventLoop, wayland_server::Display, winit::window::WindowAttributes},
    utils::Transform,
};
use std::path::PathBuf;

use crate::config::Config;
#[cfg(feature = "nested")]
use crate::state::{EvaWM, NestedBackend, OutputState};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "evawm=info".into()),
        )
        .init();

    if std::env::args().nth(1).as_deref() == Some("msg") {
        let rest = std::env::args().skip(2).collect::<Vec<_>>();
        return ipc::client::run(&rest);
    }

    let args = Args::parse()?;

    if args.nested && args.session {
        bail!("--nested and --session cannot be combined");
    }
    if !args.nested && !args.session && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        bail!("no Wayland session detected; use --nested or --session");
    }

    let (config, config_path) = Config::load(args.config.as_deref())?;

    if args.session {
        return session::run_session(args.run.as_deref(), config, config_path);
    }

    run_nested(
        args.run.as_deref(),
        args.nested_mod_alt,
        config,
        config_path,
    )
}

#[cfg(feature = "nested")]
fn run_nested(
    run: Option<&str>,
    nested_mod_alt: bool,
    config: Config,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let mut event_loop: EventLoop<EvaWM> = EventLoop::try_new()?;
    signals::install(&event_loop)?;
    let display: Display<EvaWM> = Display::new()?;

    let attributes = WindowAttributes::default()
        .with_title("EvaWM")
        .with_inner_size(smithay::reexports::winit::dpi::LogicalSize::new(
            1280.0, 800.0,
        ));
    let (backend, winit_source) = winit::init_from_attributes::<GlesRenderer>(attributes)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let size = backend.window_size();

    let output = Output::new(
        state::OUTPUT_NAME.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "EvaWM".into(),
            model: "Nested".into(),
            serial_number: "unknown".into(),
        },
    );

    let mode = Mode {
        size,
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let damage_tracker = OutputDamageTracker::from_output(&output);
    let _ = output.create_global::<EvaWM>(&display.handle());
    backend.window().set_cursor_visible(false);

    let nested = NestedBackend {
        graphics: backend,
        damage_tracker,
    };
    let mut state = EvaWM::new(
        display,
        &mut event_loop,
        state::Backend::Nested(Box::new(nested)),
        vec![OutputState::new(output, &config)],
        nested_mod_alt,
        config,
        config_path,
    );
    state.init_dmabuf();
    state.start_xwayland();
    let x11_display = state.x11_display().map(str::to_owned);
    state.run_startup();

    state.request_redraw();

    let socket = state.socket_name.to_string_lossy().into_owned();
    let mod_name = if nested_mod_alt { "Alt" } else { "Super" };
    tracing::info!(
        wayland_display = %socket,
        x11_display = x11_display.as_deref().unwrap_or("disabled"),
        mod_key = mod_name,
        "EvaWM nested ready"
    );

    event_loop
        .handle()
        .insert_source(winit_source, |event, _, state: &mut EvaWM| match event {
            WinitEvent::Resized { size, .. } => state.resize_output(size),
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                if let Err(err) = render::render(state) {
                    tracing::error!("render failed: {err:?}");
                }
            }
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            _ => {}
        })
        .map_err(|err| anyhow!("failed to insert winit source: {err}"))?;

    if let Some(run_command) = run {
        state.spawn_shell_command(run_command)?;
    }

    event_loop.run(None, &mut state, |state| {
        let _ = state.display_handle.flush_clients();
    })?;
    Ok(())
}

#[cfg(not(feature = "nested"))]
fn run_nested(
    _run: Option<&str>,
    _nested_mod_alt: bool,
    _config: Config,
    _config_path: Option<PathBuf>,
) -> Result<()> {
    bail!(
        "nested backend is disabled in this build (built without the `nested` feature); use --session"
    );
}

struct Args {
    nested: bool,
    session: bool,
    config: Option<PathBuf>,
    run: Option<String>,
    nested_mod_alt: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Self {
            nested: false,
            session: false,
            config: None,
            run: None,
            nested_mod_alt: false,
        };

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--nested" | "--winit" => args.nested = true,
                "--session" => args.session = true,
                "--config" | "-c" => {
                    args.config = Some(argv.next().context("--config needs a path")?.into());
                }
                "--run" => {
                    args.run = Some(argv.next().context("--run needs a command")?);
                }
                "--nested-mod" => {
                    let value = argv.next().context("--nested-mod needs alt or super")?;
                    args.nested_mod_alt = match value.as_str() {
                        "alt" => true,
                        "super" => false,
                        other => bail!("unknown nested modifier: {other}"),
                    };
                }
                "--version" | "-V" => {
                    println!("evawm {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--help" | "-h" => {
                    println!(
                        "evawm [--nested | --session] [--nested-mod alt|super]\n\
                         \x20     [--config | -c <path>] [--run <command>]\n\
                         \x20     [--version | -V] [--help | -h]\n\
                         evawm msg <outputs|workspaces|windows|focused|version|action|subscribe> [--json|-j] [--yes]\n\
                         \n\
                         --nested runs as a window inside the current Wayland session.\n\
                         --session takes over a free TTY via libseat/DRM.\n\
                         Use --nested-mod alt inside a host compositor that owns Super."
                    );
                    std::process::exit(0);
                }
                other => bail!("unknown argument: {other}"),
            }
        }

        Ok(args)
    }
}
