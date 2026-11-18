use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use terminal_render::GlassEffect;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

static CONFIG_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn select_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "--config requires an absolute file path"
    );
    ensure!(!path.is_dir(), "--config must name a configuration file");
    CONFIG_PATH
        .set(path.to_path_buf())
        .map_err(|_| anyhow::anyhow!("configuration path has already been selected"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Dock {
    Left,
    #[default]
    Right,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    Off,
    #[default]
    Balanced,
    Full,
}
impl Effect {
    pub fn renderer(self) -> GlassEffect {
        match self {
            Self::Off => GlassEffect::Off,
            Self::Balanced => GlassEffect::Balanced,
            Self::Full => GlassEffect::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum GpuPowerPreference {
    Auto,
    #[default]
    LowPower,
    HighPerformance,
}

impl GpuPowerPreference {
    pub fn renderer_preference(self) -> terminal_render::AdapterPreference {
        match self {
            Self::Auto => terminal_render::AdapterPreference::Auto,
            Self::LowPower => terminal_render::AdapterPreference::LowPower,
            Self::HighPerformance => terminal_render::AdapterPreference::HighPerformance,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Material {
    #[default]
    LiquidClear,
    LiquidRegular,
    Hud,
    Sidebar,
    UnderWindowBackground,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum QuitMode {
    #[default]
    Confirm,
    Hold,
    HoldOrDoublePress,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuitProtection {
    pub mode: QuitMode,
    pub hold_duration_ms: u16,
    pub double_press_interval_ms: u16,
}

impl Default for QuitProtection {
    fn default() -> Self {
        Self {
            mode: if cfg!(target_os = "macos") {
                QuitMode::HoldOrDoublePress
            } else {
                QuitMode::Confirm
            },
            hold_duration_ms: 1200,
            double_press_interval_ms: 800,
        }
    }
}

impl QuitProtection {
    fn validate(&self) -> Result<()> {
        ensure!(
            (500..=3000).contains(&self.hold_duration_ms),
            "Quit hold duration must be 500..3000 milliseconds"
        );
        ensure!(
            (200..=2000).contains(&self.double_press_interval_ms),
            "Quit double-press interval must be 200..2000 milliseconds"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum PanelMaterial {
    LiquidClear,
    #[default]
    LiquidRegular,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaterialParameters {
    pub tint_strength: f32,
    pub grain_strength: f32,
    pub sheen_strength: f32,
    pub rim_strength: f32,
    pub backdrop_blur_radius_px: f32,
}

impl Default for MaterialParameters {
    fn default() -> Self {
        let parameters = terminal_render::MaterialParameters::default();
        Self {
            tint_strength: parameters.tint_strength,
            grain_strength: parameters.grain_strength,
            sheen_strength: parameters.sheen_strength,
            rim_strength: parameters.rim_strength,
            backdrop_blur_radius_px: parameters.backdrop_blur_radius_px,
        }
    }
}

impl MaterialParameters {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("tint_strength", self.tint_strength),
            ("grain_strength", self.grain_strength),
            ("sheen_strength", self.sheen_strength),
            ("rim_strength", self.rim_strength),
        ] {
            ensure!(
                value.is_finite() && (0.0..=2.0).contains(&value),
                "Material {name} must be a finite multiplier in 0..2"
            );
        }
        ensure!(
            self.backdrop_blur_radius_px.is_finite()
                && (0.0..=100.0).contains(&self.backdrop_blur_radius_px),
            "Backdrop blur radius must be 0..100 physical pixels"
        );
        Ok(())
    }

    fn renderer(self) -> terminal_render::MaterialParameters {
        terminal_render::MaterialParameters {
            tint_strength: self.tint_strength,
            grain_strength: self.grain_strength,
            sheen_strength: self.sheen_strength,
            rim_strength: self.rim_strength,
            backdrop_blur_radius_px: self.backdrop_blur_radius_px,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppearanceUi {
    pub window_corner_radius: f32,
    pub macos_hover_buttons: bool,
    pub panel_corner_radius: u8,
    pub panel_opacity: f32,
    pub panel_material: PanelMaterial,
    pub animations: bool,
    pub animation_duration_ms: u16,
    #[serde(skip_serializing_if = "MaterialParameters::is_default")]
    pub material_parameters: MaterialParameters,
}

impl Default for AppearanceUi {
    fn default() -> Self {
        Self {
            window_corner_radius: 0.0,
            macos_hover_buttons: true,
            panel_corner_radius: 16,
            panel_opacity: if cfg!(windows) { 0.96 } else { 0.82 },
            panel_material: PanelMaterial::LiquidRegular,
            animations: true,
            animation_duration_ms: 160,
            material_parameters: MaterialParameters::default(),
        }
    }
}

impl AppearanceUi {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.window_corner_radius.is_finite()
                && (0.0..=48.0).contains(&self.window_corner_radius),
            "Window corner radius must be 0..48 points"
        );
        ensure!(
            self.panel_corner_radius <= 48,
            "Panel corner radius must be 0..48 points"
        );
        ensure!(
            self.panel_opacity.is_finite() && (0.2..=1.0).contains(&self.panel_opacity),
            "Panel opacity must be 0.2..1.0"
        );
        ensure!(
            self.animation_duration_ms <= 500,
            "Animation duration must be 0..500 milliseconds"
        );
        self.material_parameters.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    pub selection_background: String,
    pub selection_foreground: String,
    pub ansi: [String; 16],
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: "#bbbbbb".into(),
            background: "#000000".into(),
            cursor: "#bbbbbb".into(),
            selection_background: "#b5d5ff".into(),
            selection_foreground: "#000000".into(),
            ansi: [
                "#000000", "#bb0000", "#00bb00", "#bbbb00", "#0000bb", "#bb00bb", "#00bbbb",
                "#bbbbbb", "#555555", "#ff5555", "#55ff55", "#ffff55", "#5555ff", "#ff55ff",
                "#55ffff", "#ffffff",
            ]
            .map(str::to_owned),
        }
    }
}
impl Theme {
    fn validate(&self) -> Result<()> {
        for value in [
            &self.foreground,
            &self.background,
            &self.cursor,
            &self.selection_background,
            &self.selection_foreground,
        ]
        .into_iter()
        .chain(self.ansi.iter())
        {
            color(value)?;
        }
        Ok(())
    }
    pub fn palette_colors(&self) -> [[u8; 3]; 16] {
        self.ansi
            .each_ref()
            .map(|value| color(value).expect("validated ANSI color"))
    }
}

pub fn color(value: &str) -> Result<[u8; 3]> {
    ensure!(
        value.len() == 7
            && value.starts_with('#')
            && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit),
        "Color must use #RRGGBB"
    );
    Ok([
        u8::from_str_radix(&value[1..3], 16)?,
        u8::from_str_radix(&value[3..5], 16)?,
        u8::from_str_radix(&value[5..7], 16)?,
    ])
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellProfile {
    pub integration: session_manager::ShellIntegration,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_presentation: Option<bool>,
    pub program: Option<String>,
    pub args: Vec<String>,
    pub login: Option<bool>,
    pub environment: BTreeMap<String, String>,
}
impl ShellProfile {
    pub fn resolved(
        &self,
        program: Option<&str>,
        integration: Option<session_manager::ShellIntegration>,
    ) -> Result<Self> {
        let mut resolved = self.clone();
        if let Some(program) = program {
            resolved.program = Some(program.to_owned());
        }
        if resolved.program.as_deref() == Some("@aether") {
            #[cfg(target_os = "android")]
            let shell = {
                let shell = crate::android_arch::aether_shell()?;
                let bridge = crate::android_arch::system_bridge()?;
                let files = system_api::android_root::files_dir()?;
                resolved.environment.insert(
                    "AETHER_SYSTEM_BRIDGE".into(),
                    bridge
                        .to_str()
                        .context("Android system bridge path is not UTF-8")?
                        .into(),
                );
                resolved.environment.insert(
                    "GLASS_ANDROID_APP_FILES".into(),
                    files
                        .to_str()
                        .context("Android app files path is not UTF-8")?
                        .into(),
                );
                shell
            };
            #[cfg(not(target_os = "android"))]
            let shell = {
                let executable = std::env::current_exe().context("locating the current application")?;
                let directory = executable
                    .parent()
                    .context("application executable has no parent directory")?;
                let shell = directory.join(format!("aether-shell{}", std::env::consts::EXE_SUFFIX));
                let bridge = directory.join(format!(
                    "glass-system-bridge{}",
                    std::env::consts::EXE_SUFFIX
                ));
                ensure!(
                    shell.is_file(),
                    "Bundled Aether Shell is missing · {}",
                    shell.display()
                );
                ensure!(
                    bridge.is_file(),
                    "Bundled System API bridge is missing · {}",
                    bridge.display()
                );
                resolved.environment.insert(
                    "AETHER_SYSTEM_BRIDGE".into(),
                    bridge.to_string_lossy().into_owned(),
                );
                shell
            };
            resolved.program = Some(shell.to_string_lossy().into_owned());
            resolved.integration = session_manager::ShellIntegration::AetherV1;
            resolved.login = Some(false);
        }
        if let Some(integration) = integration {
            resolved.integration = integration;
        }
        resolved.validate()?;
        Ok(resolved)
    }
    pub fn validate(&self) -> Result<()> {
        if let Some(program) = &self.program {
            if program != "@aether" {
                absolute_program(program)?;
            }
        }
        ensure!(self.args.len() <= 64, "Shell accepts at most 64 arguments");
        let mut bytes = 0usize;
        for arg in &self.args {
            ensure!(
                !arg.contains('\0') && arg.len() <= 8192,
                "Invalid shell argument"
            );
            bytes += arg.len();
        }
        ensure!(
            self.environment.len() <= 64,
            "Shell accepts at most 64 environment overrides"
        );
        for (name, value) in &self.environment {
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    && !name.as_bytes()[0].is_ascii_digit(),
                "Invalid environment variable name"
            );
            ensure!(
                !value.contains('\0') && value.len() <= 8192,
                "Invalid environment value"
            );
            bytes += name.len() + value.len();
        }
        ensure!(
            bytes <= 64 * 1024,
            "Shell arguments and environment exceed 64 KiB"
        );
        Ok(())
    }
}

fn absolute_program(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 4096
            && !value.contains('\0')
            && Path::new(value).is_absolute(),
        "Program must be an absolute executable path"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shortcut {
    NewShell,
    SplitRight,
    SplitBelow,
    ClosePane,
    CloseTab,
    Quit,
    NextTab,
    PreviousTab,
    NextPane,
    PreviousPane,
    Tab1,
    Tab2,
    Tab3,
    Tab4,
    Tab5,
    Tab6,
    Tab7,
    Tab8,
    Tab9,
    Commands,
    Search,
    Settings,
    Copy,
    Paste,
    Dismiss,
    HistoryPageUp,
    HistoryPageDown,
    ReloadConfig,
    Mcp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyBinding {
    pub keys: String,
    pub action: Shortcut,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Chord {
    pub key: String,
    pub modifiers: u8,
}
impl Chord {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(value.len() <= 64, "Shortcut is too long");
        let mut modifiers = 0u8;
        let mut key = None;
        for piece in value.split('+') {
            let piece = piece.trim().to_lowercase();
            let bit = match piece.as_str() {
                "cmd" | "super" => Some(1),
                "ctrl" | "control" => Some(2),
                "alt" | "option" => Some(4),
                "shift" => Some(8),
                _ => None,
            };
            if let Some(bit) = bit {
                ensure!(modifiers & bit == 0, "Duplicate shortcut modifier");
                modifiers |= bit;
            } else {
                ensure!(key.is_none(), "Shortcut must contain one key");
                ensure!(
                    piece.chars().count() == 1
                        || matches!(
                            piece.as_str(),
                            "escape"
                                | "pageup"
                                | "pagedown"
                                | "enter"
                                | "tab"
                                | "space"
                                | "backspace"
                                | "delete"
                                | "arrowup"
                                | "arrowdown"
                                | "arrowleft"
                                | "arrowright"
                        ),
                    "Unsupported shortcut key"
                );
                key = Some(piece);
            }
        }
        let key = key.context("Shortcut key is missing")?;
        ensure!(!key.is_empty(), "Shortcut key is missing");
        Ok(Self { key, modifiers })
    }
}

fn default_bindings() -> Vec<KeyBinding> {
    bindings_for_platform(cfg!(any(windows, target_os = "linux")))
}

fn bindings_for_platform(control_shortcuts: bool) -> Vec<KeyBinding> {
    use Shortcut::*;
    let mut bindings: Vec<_> = [
        ("Cmd+T", NewShell),
        ("Ctrl+Shift+T", NewShell),
        ("Cmd+D", SplitRight),
        ("Cmd+Shift+D", SplitBelow),
        ("Cmd+W", ClosePane),
        ("Cmd+Shift+W", CloseTab),
        ("Cmd+Q", Quit),
        ("Cmd+Shift+]", NextTab),
        ("Cmd+Shift+[", PreviousTab),
        ("Cmd+]", NextPane),
        ("Cmd+[", PreviousPane),
        ("Cmd+1", Tab1),
        ("Cmd+2", Tab2),
        ("Cmd+3", Tab3),
        ("Cmd+4", Tab4),
        ("Cmd+5", Tab5),
        ("Cmd+6", Tab6),
        ("Cmd+7", Tab7),
        ("Cmd+8", Tab8),
        ("Cmd+9", Tab9),
        ("Cmd+K", Commands),
        ("Cmd+Shift+P", Commands),
        ("Cmd+F", Search),
        ("Cmd+,", Settings),
        ("Cmd+C", Copy),
        ("Cmd+V", Paste),
        ("Escape", Dismiss),
        ("Shift+PageUp", HistoryPageUp),
        ("Shift+PageDown", HistoryPageDown),
        ("Cmd+Shift+R", ReloadConfig),
    ]
    .into_iter()
    .map(|(keys, action)| KeyBinding {
        keys: keys.into(),
        action,
    })
    .collect();
    if control_shortcuts {
        bindings.extend(
            [
                ("Ctrl+Shift+D", SplitRight),
                ("Ctrl+Shift+E", SplitBelow),
                ("Ctrl+Shift+W", ClosePane),
                ("Ctrl+Alt+W", CloseTab),
                ("Ctrl+Shift+Q", Quit),
                ("Ctrl+PageDown", NextTab),
                ("Ctrl+PageUp", PreviousTab),
                ("Ctrl+Shift+]", NextPane),
                ("Ctrl+Shift+[", PreviousPane),
                ("Alt+1", Tab1),
                ("Alt+2", Tab2),
                ("Alt+3", Tab3),
                ("Alt+4", Tab4),
                ("Alt+5", Tab5),
                ("Alt+6", Tab6),
                ("Alt+7", Tab7),
                ("Alt+8", Tab8),
                ("Alt+9", Tab9),
                ("Ctrl+Shift+P", Commands),
                ("Ctrl+Shift+F", Search),
                ("Ctrl+,", Settings),
                ("Ctrl+Shift+C", Copy),
                ("Ctrl+Shift+V", Paste),
                ("Ctrl+Shift+R", ReloadConfig),
            ]
            .into_iter()
            .map(|(keys, action)| KeyBinding {
                keys: keys.into(),
                action,
            }),
        );
    }
    bindings
}

fn default_ssh_program() -> String {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32")
            .join("OpenSSH")
            .join("ssh.exe")
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(windows))]
    {
        "/usr/bin/ssh".into()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub schema_version: u32,
    pub font_size: f32,
    pub font_family: String,
    pub gpu_power_preference: GpuPowerPreference,
    pub theme: Theme,
    pub opacity: f32,
    pub material: Material,
    pub effect: Effect,
    pub reduce_motion: bool,
    pub appearance_ui: AppearanceUi,
    pub quit_protection: QuitProtection,
    pub administrator_persistent: bool,
    pub shell: ShellProfile,
    #[cfg(target_os = "android")]
    pub android_root: system_api::android_root::RootOptions,
    pub ssh_program: String,
    pub keybindings: Vec<KeyBinding>,
    pub mcp_servers: Vec<mcp_client::ServerConfig>,
    pub dock: Dock,
    pub system_open: bool,
    pub layout: Vec<crate::workspace::SplitTree>,
    pub active_tab: usize,
    #[serde(default)]
    pub appearance_version: u32,
    #[serde(skip)]
    disk_bytes: Option<Vec<u8>>,
    #[serde(skip)]
    save_blocked: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            font_size: 15.0,
            font_family: "JetBrainsMono Nerd Font Mono".into(),
            gpu_power_preference: if cfg!(windows) {
                GpuPowerPreference::HighPerformance
            } else {
                GpuPowerPreference::LowPower
            },
            theme: Theme::default(),
            // A stronger Android tint keeps text readable over unblurred content.
            opacity: if cfg!(target_os = "android") {
                0.82
            } else if cfg!(windows) {
                0.65
            } else {
                0.503074
            },
            material: Material::LiquidClear,
            effect: Effect::Balanced,
            reduce_motion: false,
            appearance_ui: AppearanceUi::default(),
            quit_protection: QuitProtection::default(),
            administrator_persistent: false,
            shell: ShellProfile::default(),
            #[cfg(target_os = "android")]
            android_root: Default::default(),
            ssh_program: default_ssh_program(),
            keybindings: default_bindings(),
            mcp_servers: Vec::new(),
            dock: Dock::Right,
            system_open: false,
            layout: Vec::new(),
            active_tab: 0,
            appearance_version: 1,
            disk_bytes: None,
            save_blocked: false,
        }
    }
}
impl Settings {
    pub fn path() -> PathBuf {
        if let Some(path) = CONFIG_PATH.get() {
            return path.clone();
        }
        #[cfg(target_os = "android")]
        {
            system_api::android_root::files_dir()
                .expect("Android Activity context must precede Settings")
                .join("config.json")
        }
        #[cfg(not(target_os = "android"))]
        {
            if cfg!(target_os = "macos") {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join("Library/Application Support/GlassTerminal/config.json")
            } else if cfg!(windows) {
                PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default())
                    .join("GlassTerminal/config.json")
            } else {
                let base = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                    });
                base.join("glass-terminal/config.json")
            }
        }
    }
    pub fn load() -> Result<Self> {
        Self::load_path(&Self::path())
    }
    fn load_path(path: &Path) -> Result<Self> {
        if path.exists() {
            let bytes = read_config(path)?;
            let mut settings = Self::parse(&bytes)?;
            settings.disk_bytes = Some(bytes);
            return Ok(settings);
        }
        let legacy = path.with_file_name("settings.json");
        let mut settings = if legacy.exists() {
            let mut settings: Self = serde_json::from_slice(&read_config(&legacy)?)
                .context("Reading legacy Plyra Terminal settings")?;
            settings.migrate_appearance();
            settings.validate()?;
            settings
        } else {
            Self::default()
        };
        settings.save_path(path)?;
        Ok(settings)
    }
    pub fn startup() -> (Self, Option<String>) {
        match Self::load() {
            Ok(settings) => (settings, None),
            Err(error) => {
                let primary = Self::path();
                let backup = read_config(&primary.with_file_name("config.last-good.json"))
                    .and_then(|bytes| Self::parse(&bytes));
                let (mut settings, fallback) = match backup {
                    Ok(settings) => (settings, "Using last valid configuration"),
                    Err(_) => (Self::default(), "Using default configuration"),
                };
                settings.save_blocked = true;
                (settings, Some(format!("Config rejected · {error:#}. {fallback}. Correct config.json and Reload configuration.")))
            }
        }
    }
    fn migrate_appearance(&mut self) {
        if self.appearance_version == 0 {
            if self.font_family == "DejaVu Sans Mono" && (self.font_size - 16.0).abs() < 0.01 {
                self.font_size = 15.0;
                self.font_family = Self::default().font_family;
            }
            self.appearance_version = 1;
        }
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= MAX_CONFIG_BYTES, "Config exceeds 1 MiB");
        let settings: Self =
            serde_json::from_slice(bytes).context("Parsing Plyra Terminal config.json")?;
        settings.validate()?;
        Ok(settings)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "Unsupported config schema_version {}",
            self.schema_version
        );
        ensure!(
            self.font_size.is_finite() && (10.0..=28.0).contains(&self.font_size),
            "Font size must be 10..28 points"
        );
        ensure!(
            !self.font_family.is_empty()
                && self.font_family.len() <= 256
                && !self.font_family.chars().any(char::is_control),
            "Invalid font family"
        );
        ensure!(
            self.opacity.is_finite() && (0.0..=1.0).contains(&self.opacity),
            "Opacity must be 0.0..1.0"
        );
        self.theme.validate()?;
        self.appearance_ui.validate()?;
        self.quit_protection.validate()?;
        self.shell.validate()?;
        #[cfg(target_os = "android")]
        self.android_root.validate()?;
        absolute_program(&self.ssh_program)?;
        ensure!(
            self.keybindings.len() <= 128,
            "At most 128 shortcuts are supported"
        );
        let mut chords = BTreeSet::new();
        for binding in &self.keybindings {
            ensure!(
                chords.insert(Chord::parse(&binding.keys)?),
                "Duplicate shortcut {}",
                binding.keys
            );
        }
        ensure!(
            self.mcp_servers.len() <= 16,
            "At most 16 MCP servers are supported"
        );
        let mut server_ids = BTreeSet::new();
        for server in &self.mcp_servers {
            server.validate()?;
            ensure!(
                server_ids.insert(&server.id),
                "Duplicate MCP server ID {}",
                server.id
            );
        }
        ensure!(
            self.layout.len() <= 32,
            "At most 32 restored tabs are supported"
        );
        let mut pane_count = 0;
        for tree in &self.layout {
            validate_tree(tree, 0, &mut pane_count)?;
        }
        ensure!(
            self.layout.is_empty() || self.active_tab < self.layout.len(),
            "Active tab is outside the saved workspace"
        );
        Ok(())
    }
    pub fn save(&mut self) -> Result<()> {
        self.save_path(&Self::path())
    }
    fn save_path(&mut self, path: &Path) -> Result<()> {
        self.validate()?;
        ensure!(
            !self.save_blocked,
            "Config contains an error. Correct it and Reload configuration before saving"
        );
        let current = if path.exists() {
            Some(read_config(path)?)
        } else {
            None
        };
        ensure!(
            current == self.disk_bytes,
            "Config changed on disk. Reload configuration before saving"
        );
        let bytes = serde_json::to_vec_pretty(self)?;
        write_bytes(path, &bytes)?;
        self.disk_bytes = Some(bytes.clone());
        write_bytes(&path.with_file_name("config.last-good.json"), &bytes)?;
        Ok(())
    }
    pub fn replace_json(&mut self, text: &str) -> Result<()> {
        let mut next = Self::parse(text.as_bytes())?;
        // The editor is an explicit replacement. Background workspace saves use conflict checking.
        next.disk_bytes = if Self::path().exists() {
            Some(read_config(&Self::path())?)
        } else {
            None
        };
        next.save()?;
        *self = next;
        Ok(())
    }
    pub fn editor_text(&self) -> Result<String> {
        let bytes = match read_config(&Self::path()) {
            Ok(bytes) => bytes,
            Err(_) => serde_json::to_vec_pretty(self)?,
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    pub fn appearance(&self) -> terminal_render::Appearance {
        terminal_render::Appearance {
            foreground: color(&self.theme.foreground).expect("validated foreground"),
            background: color(&self.theme.background).expect("validated background"),
            cursor: color(&self.theme.cursor).expect("validated cursor"),
            selection_background: color(&self.theme.selection_background)
                .expect("validated selection"),
            selection_foreground: color(&self.theme.selection_foreground)
                .expect("validated selection"),
            ansi: self.theme.palette_colors(),
            opacity: self.opacity,
            window_corner_radius: self.appearance_ui.window_corner_radius,
            material_parameters: self.appearance_ui.material_parameters.renderer(),
            material: match self.material {
                Material::LiquidClear => terminal_render::GlassMaterial::LiquidClear,
                Material::LiquidRegular => terminal_render::GlassMaterial::LiquidRegular,
                Material::Hud => terminal_render::GlassMaterial::Hud,
                Material::Sidebar => terminal_render::GlassMaterial::Sidebar,
                Material::UnderWindowBackground => {
                    terminal_render::GlassMaterial::UnderWindowBackground
                }
            },
        }
    }
    pub fn palette(&self) -> terminal_core::TerminalPalette {
        terminal_core::TerminalPalette {
            foreground: color(&self.theme.foreground).expect("validated foreground"),
            background: color(&self.theme.background).expect("validated background"),
            ansi: self.theme.palette_colors(),
        }
    }
    pub fn shortcut(&self, chord: &Chord) -> Option<Shortcut> {
        self.keybindings
            .iter()
            .find(|binding| Chord::parse(&binding.keys).is_ok_and(|value| value == *chord))
            .map(|binding| binding.action)
    }
}

fn validate_tree(
    tree: &crate::workspace::SplitTree,
    depth: usize,
    panes: &mut usize,
) -> Result<()> {
    ensure!(depth <= 8, "Workspace split nesting exceeds 8");
    match tree {
        crate::workspace::SplitTree::Pane(_) => *panes += 1,
        crate::workspace::SplitTree::Split {
            ratio,
            first,
            second,
            ..
        } => {
            ensure!(
                ratio.is_finite() && (0.15..=0.85).contains(ratio),
                "Split ratio must be 0.15..0.85"
            );
            validate_tree(first, depth + 1, panes)?;
            validate_tree(second, depth + 1, panes)?;
        }
    }
    ensure!(*panes <= 64, "At most 64 restored panes are supported");
    Ok(())
}
fn read_config(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        fs::metadata(path)?.len() <= MAX_CONFIG_BYTES as u64,
        "Config exceeds 1 MiB"
    );
    let bytes = fs::read(path)?;
    ensure!(bytes.len() <= MAX_CONFIG_BYTES, "Config exceeds 1 MiB");
    Ok(bytes)
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_bytes(path, &serde_json::to_vec_pretty(value)?)
}
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Config parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".config-{}-{}.tmp",
        std::process::id(),
        crate::now_ms()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gpu_preference_defaults_by_platform_and_round_trips_typed_variants() {
        assert_eq!(
            Settings::parse(b"{}").unwrap().gpu_power_preference,
            if cfg!(windows) {
                GpuPowerPreference::HighPerformance
            } else {
                GpuPowerPreference::LowPower
            }
        );
        for value in ["Auto", "LowPower", "HighPerformance"] {
            let bytes = format!(r#"{{"gpu_power_preference":"{value}"}}"#);
            let settings = Settings::parse(bytes.as_bytes()).unwrap();
            assert_eq!(
                serde_json::to_value(settings).unwrap()["gpu_power_preference"],
                value
            );
        }
        assert!(Settings::parse(br#"{"gpu_power_preference":"Unknown"}"#).is_err());
        assert!(matches!(
            GpuPowerPreference::Auto.renderer_preference(),
            terminal_render::AdapterPreference::Auto
        ));
        assert!(matches!(
            GpuPowerPreference::LowPower.renderer_preference(),
            terminal_render::AdapterPreference::LowPower
        ));
        assert!(matches!(
            GpuPowerPreference::HighPerformance.renderer_preference(),
            terminal_render::AdapterPreference::HighPerformance
        ));
    }

    #[test]
    fn bundled_aether_alias_is_persisted_and_explicit_program_override_is_resolved() {
        let settings =
            Settings::parse(br#"{"shell":{"program":"@aether","native_presentation":true}}"#)
                .unwrap();
        assert_eq!(settings.shell.program.as_deref(), Some("@aether"));
        assert_eq!(settings.shell.native_presentation, Some(true));
        let encoded = serde_json::to_value(&settings).unwrap();
        assert_eq!(encoded["shell"]["program"], "@aether");
        let resolved = settings.shell.resolved(Some("/bin/sh"), None).unwrap();
        assert_eq!(resolved.program.as_deref(), Some("/bin/sh"));
        assert_eq!(
            resolved.integration,
            session_manager::ShellIntegration::None
        );
    }

    #[test]
    fn appearance_defaults_keep_old_config_and_validate_new_ranges() {
        let settings = Settings::parse(b"{}").unwrap();
        assert_eq!(settings.appearance_ui, AppearanceUi::default());
        assert!(!settings.administrator_persistent);
        let settings = Settings::parse(
            br#"{"appearance_ui":{"window_corner_radius":12,"panel_corner_radius":0,"panel_opacity":0.7,"panel_material":"LiquidClear","animations":false,"animation_duration_ms":120},"administrator_persistent":true}"#,
        )
        .unwrap();
        assert_eq!(settings.appearance().window_corner_radius, 12.0);
        assert_eq!(settings.appearance_ui.panel_corner_radius, 0);
        assert!(settings.administrator_persistent);
        for bytes in [
            br#"{"appearance_ui":{"window_corner_radius":49}}"#.as_slice(),
            br#"{"appearance_ui":{"window_corner_radius":-1}}"#.as_slice(),
            br#"{"appearance_ui":{"panel_corner_radius":49}}"#.as_slice(),
            br#"{"appearance_ui":{"panel_opacity":0.1}}"#.as_slice(),
            br#"{"appearance_ui":{"animation_duration_ms":501}}"#.as_slice(),
        ] {
            assert!(Settings::parse(bytes).is_err());
        }
    }

    #[test]
    fn material_parameters_keep_old_configs_and_round_trip_partial_overrides() {
        let old = Settings::parse(br#"{"opacity":0.72,"material":"LiquidRegular"}"#).unwrap();
        assert_eq!(old.appearance().material_parameters, Default::default());
        let old_json = serde_json::to_value(&old).unwrap();
        assert!(old_json["appearance_ui"]
            .get("material_parameters")
            .is_none());
        let settings = Settings::parse(
            br#"{"opacity":0.72,"appearance_ui":{"material_parameters":{"grain_strength":0,"rim_strength":2,"backdrop_blur_radius_px":40}}}"#,
        )
        .unwrap();
        let appearance = settings.appearance();
        assert_eq!(appearance.opacity, 0.72);
        assert_eq!(appearance.material_parameters.tint_strength, 1.0);
        assert_eq!(appearance.material_parameters.grain_strength, 0.0);
        assert_eq!(appearance.material_parameters.rim_strength, 2.0);
        assert_eq!(appearance.material_parameters.backdrop_blur_radius_px, 40.0);
        let bytes = serde_json::to_vec(&settings).unwrap();
        let restored = Settings::parse(&bytes).unwrap();
        assert_eq!(restored.appearance(), appearance);
        for bytes in [
            br#"{"appearance_ui":{"material_parameters":{"tint_strength":-0.1}}}"#.as_slice(),
            br#"{"appearance_ui":{"material_parameters":{"grain_strength":2.1}}}"#.as_slice(),
            br#"{"appearance_ui":{"material_parameters":{"sheen_strength":3}}}"#.as_slice(),
            br#"{"appearance_ui":{"material_parameters":{"rim_strength":-1}}}"#.as_slice(),
            br#"{"appearance_ui":{"material_parameters":{"backdrop_blur_radius_px":101}}}"#
                .as_slice(),
        ] {
            assert!(Settings::parse(bytes).is_err());
        }
    }

    #[test]
    fn quit_protection_defaults_and_validates_configured_hold_duration() {
        let old = Settings::parse(b"{}").unwrap();
        assert_eq!(old.quit_protection, QuitProtection::default());
        #[cfg(target_os = "macos")]
        assert_eq!(old.quit_protection.mode, QuitMode::HoldOrDoublePress);
        let confirmation =
            Settings::parse(br#"{"quit_protection":{"mode":"Confirm"}}"#).unwrap();
        assert_eq!(confirmation.quit_protection.mode, QuitMode::Confirm);
        let hold =
            Settings::parse(br#"{"quit_protection":{"mode":"Hold","hold_duration_ms":1800}}"#)
                .unwrap();
        assert_eq!(hold.quit_protection.mode, QuitMode::Hold);
        assert_eq!(hold.quit_protection.hold_duration_ms, 1800);
        assert_eq!(hold.quit_protection.double_press_interval_ms, 800);
        let combined = Settings::parse(br#"{"quit_protection":{"mode":"HoldOrDoublePress","hold_duration_ms":1000,"double_press_interval_ms":600}}"#).unwrap();
        assert_eq!(combined.quit_protection.mode, QuitMode::HoldOrDoublePress);
        assert_eq!(combined.quit_protection.double_press_interval_ms, 600);
        let restored = Settings::parse(&serde_json::to_vec(&combined).unwrap()).unwrap();
        assert_eq!(restored.quit_protection, combined.quit_protection);
        for bytes in [
            br#"{"quit_protection":{"hold_duration_ms":499}}"#.as_slice(),
            br#"{"quit_protection":{"hold_duration_ms":3001}}"#.as_slice(),
            br#"{"quit_protection":{"double_press_interval_ms":199}}"#.as_slice(),
            br#"{"quit_protection":{"double_press_interval_ms":2001}}"#.as_slice(),
        ] {
            assert!(Settings::parse(bytes).is_err());
        }
    }

    #[test]
    fn desktop_defaults_expose_control_shortcuts_and_keep_shell_control_keys() {
        let settings = Settings {
            keybindings: bindings_for_platform(true),
            ..Settings::default()
        };
        settings.validate().unwrap();
        for (keys, expected) in [
            ("Ctrl+Shift+T", Shortcut::NewShell),
            ("Ctrl+Shift+D", Shortcut::SplitRight),
            ("Ctrl+Shift+E", Shortcut::SplitBelow),
            ("Ctrl+Shift+P", Shortcut::Commands),
            ("Ctrl+Shift+F", Shortcut::Search),
            ("Ctrl+,", Shortcut::Settings),
            ("Ctrl+Shift+C", Shortcut::Copy),
            ("Ctrl+Shift+V", Shortcut::Paste),
            ("Ctrl+PageDown", Shortcut::NextTab),
            ("Ctrl+Shift+[", Shortcut::PreviousPane),
        ] {
            assert_eq!(
                settings.shortcut(&Chord::parse(keys).unwrap()),
                Some(expected)
            );
        }
        assert_eq!(settings.shortcut(&Chord::parse("Ctrl+C").unwrap()), None);
        assert_eq!(settings.shortcut(&Chord::parse("Ctrl+V").unwrap()), None);
    }
    #[test]
    fn mac_defaults_keep_the_existing_command_bindings() {
        let settings = Settings {
            keybindings: bindings_for_platform(false),
            ..Settings::default()
        };
        settings.validate().unwrap();
        assert_eq!(
            settings.shortcut(&Chord::parse("Cmd+,").unwrap()),
            Some(Shortcut::Settings)
        );
        assert_eq!(
            settings.shortcut(&Chord::parse("Cmd+C").unwrap()),
            Some(Shortcut::Copy)
        );
        assert_eq!(
            settings.shortcut(&Chord::parse("Cmd+Shift+[").unwrap()),
            Some(Shortcut::PreviousTab)
        );
        assert_eq!(
            settings.shortcut(&Chord::parse("Ctrl+Shift+C").unwrap()),
            None
        );
    }
    #[test]
    fn loading_user_bindings_preserves_the_exact_configured_list() {
        let settings =
            Settings::parse(br#"{"keybindings":[{"keys":"Ctrl+Alt+K","action":"commands"}]}"#)
                .unwrap();
        assert_eq!(settings.keybindings.len(), 1);
        assert_eq!(
            settings.shortcut(&Chord::parse("Ctrl+Alt+K").unwrap()),
            Some(Shortcut::Commands)
        );
        assert_eq!(
            settings.shortcut(&Chord::parse("Ctrl+Shift+C").unwrap()),
            None
        );
    }
    #[test]
    fn older_settings_gain_workspace_defaults() {
        let settings: Settings =
            serde_json::from_str(r#"{"font_size":13.0,"effect":"Off"}"#).unwrap();
        assert_eq!(settings.font_size, 13.0);
        assert!(settings.layout.is_empty());
        assert_eq!(settings.active_tab, 0);
        assert_eq!(settings.dock, Dock::Right);
    }
    #[test]
    fn settings_roundtrip_keeps_layout_and_accessibility() {
        let settings = Settings {
            effect: Effect::Full,
            reduce_motion: true,
            dock: Dock::Bottom,
            layout: vec![crate::workspace::SplitTree::Split {
                axis: crate::workspace::Axis::Vertical,
                ratio: 0.4,
                first: Box::new(crate::workspace::SplitTree::Pane(5)),
                second: Box::new(crate::workspace::SplitTree::Pane(8)),
            }],
            ..Settings::default()
        };
        let restored = Settings::parse(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(restored.layout, settings.layout);
        assert!(restored.reduce_motion);
        assert_eq!(restored.effect, Effect::Full);
        assert_eq!(restored.dock, Dock::Bottom);
    }
    #[test]
    fn rejects_invalid_colors_chords_and_unknown_fields() {
        assert!(color("#1234ff").is_ok());
        assert!(color("#é1234").is_err());
        assert!(Settings::parse(br#"{"opacity":2.0}"#).is_err());
        assert!(Settings::parse(br#"{"opacity":0.0,"material":"LiquidClear"}"#).is_ok());
        assert!(Settings::parse(br#"{"blur_radius":14}"#).is_err());
        assert!(Chord::parse("Cmd+Cmd+C").is_err());
        let mut config = Settings::default();
        config.keybindings.push(config.keybindings[0].clone());
        assert!(config.validate().is_err());
    }
    #[test]
    fn external_edits_are_preserved_and_invalid_config_keeps_last_good() {
        let root = std::env::temp_dir().join(format!(
            "glass-config-test-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let path = root.join("config.json");
        let mut config = Settings::load_path(&path).unwrap();
        let before = fs::read(root.join("config.last-good.json")).unwrap();
        fs::write(&path, b"broken JSON").unwrap();
        assert!(config.save_path(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken JSON");
        assert_eq!(
            fs::read(root.join("config.last-good.json")).unwrap(),
            before
        );
        fs::remove_dir_all(root).unwrap();
    }
}

// FVPROV1 44492 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNjRlMjAzZDkxMGNkMDY0NDY3MDM4YzJkYmVjNDkwYWQwZTY0N2MyYmRiYTY3ZDdiYTk1ZGI5YjhmM2Q0ZmNlYiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4yIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjIiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNDMwZWVjYTRlMzA5NjBkYWFiODQ2ZGZmNjQ5MTY4YTU5NjFjMzc2ZjE1N2EwNmIwNTEyZGE5ODlmODI2NzczNWIzYmY1MDNkOGE4MjY4ZWFmN2IyYTk5YWQ2OTQ2OTg2ZmEzYTkwYzZjYTJmYWExZjg4MmI1OTgyNjBmM2YxMDYifV19XQ== FVPROV1-END
