use std::path::PathBuf;

use smithay::{
    backend::renderer::Color32F,
    input::keyboard::Keysym,
    utils::{Logical, Point, Transform},
};

use crate::{animation::AnimationsConfig, workspace::Mode};

use super::parse::parse_color;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub logo: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Action {
    /// Whether holding the bind key should repeat the action.
    pub fn is_repeatable(&self) -> bool {
        matches!(
            self,
            Self::FocusLeft
                | Self::FocusRight
                | Self::FocusUp
                | Self::FocusDown
                | Self::MoveLeft
                | Self::MoveRight
                | Self::MoveWindowLeft
                | Self::MoveWindowRight
                | Self::MoveUp
                | Self::MoveDown
                | Self::ResizeWidth(_)
                | Self::ResizeHeight(_)
                | Self::PanCameraLeft(_)
                | Self::PanCameraRight(_)
                | Self::PanCameraUp(_)
                | Self::PanCameraDown(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindTrigger {
    Key(Keysym),
    Wheel(WheelDirection),
}

#[derive(Debug, Clone)]
pub struct Bind {
    pub mods: Modifiers,
    pub trigger: BindTrigger,
    pub action: Action,
}

#[derive(Debug, Clone)]
pub enum Action {
    Spawn(Vec<String>),
    SpawnSh(String),
    CloseWindow,
    Quit,

    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    FocusColumnFirst,
    FocusColumnLast,
    MoveLeft,
    MoveRight,
    MoveWindowLeft,
    MoveWindowRight,
    MoveUp,
    MoveDown,
    MoveColumnToFirst,
    MoveColumnToLast,
    FocusMonitorLeft,
    FocusMonitorRight,
    FocusMonitorUp,
    FocusMonitorDown,
    MoveToMonitorLeft,
    MoveToMonitorRight,
    MoveToMonitorUp,
    MoveToMonitorDown,

    ResizeWidth(f64),
    ResizeHeight(f64),
    MaximizeColumn,
    ExpandColumnToAvailableWidth,
    CenterFocused,
    FullscreenWindow,

    FocusWorkspace(u8),
    MoveWindowToWorkspace(u8),
    MoveColumnToWorkspace(u8),
    ConsumeOrExpelLeft,
    ConsumeOrExpelRight,

    CycleWindowNext,
    CycleWindowPrevious,
    ToggleCanvas,
    ToggleOverview,
    OpenOverview,
    CloseOverview,
    PanCameraLeft(Option<i32>),
    PanCameraRight(Option<i32>),
    PanCameraUp(Option<i32>),
    PanCameraDown(Option<i32>),
    HomeCanvas,
    ReloadConfig,
    PowerOffMonitors,
}

#[derive(Debug, Clone)]
pub struct CursorConfig {
    pub theme: String,
    pub size: u32,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            theme: String::new(),
            size: 24,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderConfig {
    pub width: f64,
    pub active: Color32F,
    pub inactive: Color32F,
}

impl BorderConfig {
    /// The ring closest to the window content.
    pub fn inner() -> Self {
        Self {
            width: 3.0,
            active: parse_color("#ffffff").expect("valid default color"),
            inactive: parse_color("#505050").expect("valid default color"),
        }
    }

    /// The optional ring drawn outside the inner border.
    pub fn outer() -> Self {
        Self {
            width: 3.0,
            active: parse_color("#b4b4b4").expect("valid default color"),
            inactive: parse_color("#2a2a2a").expect("valid default color"),
        }
    }
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self::inner()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverviewConfig {
    /// Zoomed-out scale while Overview is open (`0.1..=0.95`).
    pub zoom: f64,
}

impl Default for OverviewConfig {
    fn default() -> Self {
        Self { zoom: 0.5 }
    }
}

#[derive(Debug, Clone)]
pub struct LayoutConfig {
    pub gaps: f64,
    pub default_column_proportion: f64,
    /// Mode used for newly created workspaces and the first workspace.
    pub default_mode: Mode,
    pub inner_border: BorderConfig,
    pub outer_border: BorderConfig,
    pub background: Color32F,
    pub overview: OverviewConfig,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            gaps: 8.0,
            default_column_proportion: 0.5,
            default_mode: Mode::Scroll,
            inner_border: BorderConfig::inner(),
            outer_border: BorderConfig::outer(),
            background: parse_color("#000000").expect("valid default color"),
            overview: OverviewConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NewWindowPosition {
    #[default]
    Center,
    CursorCenter,
    CursorTopLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseButton {
    #[default]
    None,
    Back,
    Forward,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasConfig {
    pub pan_step: f64,
    pub move_step: f64,
    /// New canvas window size as a fraction of the working area.
    pub new_window_width: f64,
    pub new_window_height: f64,
    /// Where new canvas windows open: view center, centered under the cursor,
    /// or with the top-left corner at the cursor.
    pub new_window_position: NewWindowPosition,
    pub pan_button: MouseButton,
    pub mod_button: MouseButton,
}

impl Default for CanvasConfig {
    fn default() -> Self {
        Self {
            pan_step: 200.0,
            move_step: 40.0,
            new_window_width: 0.55,
            new_window_height: 0.60,
            new_window_position: NewWindowPosition::Center,
            pan_button: MouseButton::None,
            mod_button: MouseButton::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XwaylandConfig {
    /// Pre-bind the X11 sockets and start `xwayland-satellite` on the first
    /// X11 client connection.
    pub enabled: bool,
}

impl Default for XwaylandConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelProfile {
    Adaptive,
    Flat,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseConfig {
    pub natural_scroll: bool,
    pub left_handed: bool,
    pub middle_emulation: bool,
    pub accel_speed: f64,
    pub accel_profile: Option<AccelProfile>,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            natural_scroll: false,
            left_handed: false,
            middle_emulation: false,
            accel_speed: 0.0,
            // The shipped config selects flat acceleration explicitly. A user
            // config can switch back to adaptive.
            accel_profile: Some(AccelProfile::Flat),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchpadConfig {
    pub tap: bool,
    pub natural_scroll: bool,
    pub left_handed: bool,
    pub accel_speed: f64,
    /// `None` keeps the device's libinput default profile.
    pub accel_profile: Option<AccelProfile>,
}

impl Default for TouchpadConfig {
    fn default() -> Self {
        Self {
            tap: true,
            natural_scroll: true,
            left_handed: false,
            accel_speed: 0.0,
            accel_profile: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusFollowsMouseConfig {
    pub scroll: bool,
    pub canvas: bool,
}

impl Default for FocusFollowsMouseConfig {
    fn default() -> Self {
        Self {
            scroll: true,
            canvas: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XkbConfig {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: Option<String>,
}

impl XkbConfig {
    pub fn to_smithay(&self) -> smithay::input::keyboard::XkbConfig<'_> {
        smithay::input::keyboard::XkbConfig {
            rules: &self.rules,
            model: &self.model,
            layout: &self.layout,
            variant: &self.variant,
            options: self.options.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InputConfig {
    /// XKB keymap configuration. Empty fields fall back to xkbcommon defaults
    /// / `XKB_DEFAULT_*` environment variables.
    pub xkb: XkbConfig,
    /// Delay before a held bind starts repeating, in milliseconds.
    pub repeat_delay: u32,
    /// Repeats per second. 0 disables bind repeat.
    pub repeat_rate: u32,
    pub mouse: MouseConfig,
    pub touchpad: TouchpadConfig,
    pub focus_follows_mouse: FocusFollowsMouseConfig,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            xkb: XkbConfig::default(),
            repeat_delay: 250,
            repeat_rate: 25,
            mouse: MouseConfig::default(),
            touchpad: TouchpadConfig::default(),
            focus_follows_mouse: FocusFollowsMouseConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputModeConfig {
    pub width: i32,
    pub height: i32,
    /// Refresh rate in mHz, when explicitly requested.
    pub refresh: Option<i32>,
}

// Output scale is intentionally fixed at 1 for now.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    pub name: String,
    pub off: bool,
    pub mode: Option<OutputModeConfig>,
    pub position: Option<Point<i32, Logical>>,
    pub transform: Option<Transform>,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub input: InputConfig,
    pub cursor: CursorConfig,
    pub layout: LayoutConfig,
    pub animations: AnimationsConfig,
    pub canvas: CanvasConfig,
    pub xwayland: XwaylandConfig,
    pub outputs: Vec<OutputConfig>,
    pub binds: Vec<Bind>,
    /// Commands run once when the compositor starts. Each entry is already
    /// split into program + args.
    pub startup: Vec<Vec<String>>,
    /// Files read for this config, including the main file and includes.
    /// Used by the config watcher. The embedded default is not listed.
    pub sources: Vec<PathBuf>,
}

impl Config {
    pub fn output_config(&self, name: &str) -> Option<&OutputConfig> {
        self.outputs.iter().find(|output| output.name == name)
    }

    pub fn find_bind(
        &self,
        mods: Modifiers,
        modified: Keysym,
        raw: Option<Keysym>,
    ) -> Option<&Action> {
        self.binds
            .iter()
            .rfind(|bind| {
                bind.mods == mods
                    && matches!(bind.trigger, BindTrigger::Key(key) if key == modified || raw == Some(key))
            })
            .map(|bind| &bind.action)
    }

    pub fn find_wheel_bind(&self, mods: Modifiers, direction: WheelDirection) -> Option<&Action> {
        self.binds
            .iter()
            .rfind(|bind| bind.mods == mods && bind.trigger == BindTrigger::Wheel(direction))
            .map(|bind| &bind.action)
    }
}
