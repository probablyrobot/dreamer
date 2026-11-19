//! User settings: `<config dir>/dreamer.yaml` (see `docs/settings.md`).
//!
//! Every field is optional; a missing file means all defaults. Unknown keys
//! and out-of-range values are errors with the file, line, and column, so a
//! typo never silently falls back to a default.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ConfigError;

/// JetBrains Mono (IntelliJ's font) with Nerd Font icons, in the variant
/// whose icons stay one cell wide.
pub const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
pub const DEFAULT_LINE_HEIGHT: f32 = 1.2;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub font: FontSettings,
    pub links: LinkSettings,
    pub clipboard: ClipboardSettings,
    pub fallback: FallbackSettings,
    pub appearance: Appearance,
    pub terminal: TerminalSettings,
    pub providers: ProviderSettings,
    pub tiers: TierSettings,
    pub agents: crate::agents::AgentSettings,
    pub drawer: DrawerSettings,
    pub memory: MemorySettings,
    pub window: WindowSettings,
    pub web: WebSettings,
}

/// Dreamer Web, the Chromium component web panes need (`docs/desktop.md`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebSettings {
    pub component: WebComponent,
    /// Where to download the component from instead of the release: the
    /// archive's address, or a directory's that holds it. The
    /// `DREAMER_WEB_URL` variable, when set, comes first.
    pub url: Option<String>,
    pub downloads: DownloadSettings,
    pub color: WebColor,
}

/// The color space Chromium draws pages in. Read when Chromium starts
/// (the first web pane), so a change applies after Dreamer restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebColor {
    /// sRGB, as the rest of the window is drawn: wide-gamut colors are
    /// clamped to it.
    #[default]
    Srgb,
    /// The display's own color space: Display P3 images, video, and CSS
    /// colors keep their saturation on a wide-gamut display.
    Display,
}

impl WebColor {
    pub fn name(self) -> &'static str {
        match self {
            WebColor::Srgb => "srgb",
            WebColor::Display => "display",
        }
    }
}

/// Whether Dreamer offers to add Dreamer Web when it is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebComponent {
    /// Offer it once at the first start, and in each web pane that needs
    /// it; nothing downloads without a yes.
    #[default]
    Ask,
    /// Add it without asking when something needs it (managed setups).
    Auto,
    /// Never offer it; web panes say it is turned off.
    Never,
}

impl WebComponent {
    pub fn name(self) -> &'static str {
        match self {
            WebComponent::Ask => "ask",
            WebComponent::Auto => "auto",
            WebComponent::Never => "never",
        }
    }
}

/// Downloads in web panes (see [`crate::downloads`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DownloadSettings {
    /// The folder they are saved to, as written (`~` for the home
    /// directory, else absolute). `None`: the platform's download folder.
    pub folder: Option<String>,
}

/// The desktop's windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowSettings {
    pub decorations: Decorations,
}

/// Whether a window has the system's title bar and buttons (close, zoom,
/// minimize). With `none` the tab strip still moves the window, and the
/// window still resizes from its edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decorations {
    #[default]
    Full,
    None,
}

/// Durable memory (see `docs/memory.md`): which provider keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemorySettings {
    pub provider: MemoryProvider,
    pub kosmonaut: KosmonautSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryProvider {
    /// Markdown files with frontmatter and a `MEMORY.md` index.
    #[default]
    Files,
    /// Kosmonaut, a graph database, embedded in the daemon.
    Kosmonaut,
}

impl MemoryProvider {
    pub fn name(self) -> &'static str {
        match self {
            MemoryProvider::Files => "files",
            MemoryProvider::Kosmonaut => "kosmonaut",
        }
    }
}

/// `memory.kosmonaut`: the embedded database (the only mode so far).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KosmonautSettings {
    /// The database directory; `None`: `memory.kosmonaut` in Dreamer's
    /// data directory.
    pub path: Option<PathBuf>,
    /// `None`: no vectors; recall by words and the graph.
    pub embedder: Option<EmbedderSettings>,
}

/// `memory.kosmonaut.embedder`: what turns memories into vectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedderSettings {
    /// Word vectors from a file (word2vec text format).
    Static { path: PathBuf },
    /// A local model behind a program: JSON texts in, JSON vectors out.
    Local {
        command: Vec<String>,
        dimension: usize,
        timeout: std::time::Duration,
    },
}

/// The longest a local embedder may take for one call, by default.
pub const DEFAULT_EMBEDDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Where the harness drawer docks in a window, and how much of it it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawerSettings {
    pub side: DrawerSide,
    /// Percent of the window's height (bottom) or width (left, right).
    pub size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DrawerSide {
    Bottom,
    Right,
    Left,
}

pub const DEFAULT_DRAWER_SIZE: u32 = 35;

impl Default for DrawerSettings {
    fn default() -> Self {
        DrawerSettings {
            side: DrawerSide::Bottom,
            size: DEFAULT_DRAWER_SIZE,
        }
    }
}

/// When panes that nothing follows (no view, tap, or filter) go Cold on
/// their own (spec 16.6). The defaults come from the M7 measurements in
/// `docs/performance.md`: a Warm pane holds about 1 KiB per line of
/// history (about 2 MiB at tmux's default 2,000 lines), a Cold one about
/// 3 KiB, and opening a Cold pane again takes tens of milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierSettings {
    /// A pane that has written nothing for this long goes Cold; `None`:
    /// never for being quiet.
    pub cold_after: Option<std::time::Duration>,
    /// At most this many Warm panes that nothing follows; beyond it the
    /// quietest go Cold. `None`: no limit.
    pub warm_limit: Option<usize>,
}

pub const DEFAULT_COLD_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);
pub const DEFAULT_WARM_LIMIT: usize = 128;

impl Default for TierSettings {
    fn default() -> Self {
        TierSettings {
            cold_after: Some(DEFAULT_COLD_AFTER),
            warm_limit: Some(DEFAULT_WARM_LIMIT),
        }
    }
}

/// Agent providers (spec 13.5). The CLIs need nothing here; an API
/// adapter is used only when it is configured, and its model is this
/// configuration, never a constant in code.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderSettings {
    /// The provider used when none is named: `claude`, `codex`,
    /// `claude-api`, or `openai-api`. Without one, the first ready CLI.
    pub default: Option<String>,
    pub claude_api: Option<ApiProvider>,
    pub openai_api: Option<ApiProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiProvider {
    pub model: String,
    pub key: ApiKey,
    pub base_url: Option<String>,
    pub max_tokens: u32,
}

/// Where an API key is read from: never the settings file itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKey {
    /// This environment variable.
    Env(String),
    /// The OS keychain under service `dreamer` and this account.
    Keychain(String),
}

/// The terminal's defaults, applied by the daemon to every pane's
/// canonical mirror (spec 14.2): views get them through checkpoints.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalSettings {
    /// A built-in color scheme (`dream themes` lists them). Without one,
    /// the scheme follows `appearance` (see [`Settings::scheme`]).
    pub theme: Option<&'static terminal_core::schemes::Scheme>,
    /// The cursor until a program sets its own.
    pub cursor_style: CursorStyle,
    pub cursor_blink: bool,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        TerminalSettings {
            theme: None,
            cursor_style: CursorStyle::Block,
            cursor_blink: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
}

impl CursorStyle {
    pub fn shape(self) -> terminal_core::CursorShape {
        match self {
            CursorStyle::Block => terminal_core::CursorShape::Block,
            CursorStyle::Bar => terminal_core::CursorShape::Bar,
            CursorStyle::Underline => terminal_core::CursorShape::Underline,
        }
    }
}

/// The desktop's skin: the dark or light default, or whichever the system
/// uses (followed live).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Dark,
    Light,
}

impl Settings {
    /// The terminal color scheme, which the desktop's chrome is drawn
    /// from too, so the whole window is one theme: `terminal.theme` if
    /// set, else Dreamer's dark or light scheme for `appearance`. With
    /// `system`, the OS's appearance when this is read.
    pub fn scheme(&self) -> &'static terminal_core::schemes::Scheme {
        if let Some(s) = self.terminal.theme {
            return s;
        }
        let light = match self.appearance {
            Appearance::Dark => false,
            Appearance::Light => true,
            Appearance::System => os_prefers_light(),
        };
        if light {
            terminal_core::schemes::dreamer_light()
        } else {
            terminal_core::schemes::dreamer_dark()
        }
    }
}

/// Whether the OS is set to a light appearance. macOS: the global
/// `AppleInterfaceStyle` (absent means light). Linux: the desktop portal's
/// GNOME setting, where present. Anything unknown counts as dark.
fn os_prefers_light() -> bool {
    use std::process::{Command, Stdio};
    let read = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
    };
    if cfg!(target_os = "macos") {
        return read("defaults", &["read", "-g", "AppleInterfaceStyle"]).is_some_and(|o| {
            !String::from_utf8_lossy(&o.stdout)
                .trim()
                .eq_ignore_ascii_case("dark")
        });
    }
    read(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "color-scheme"],
    )
    .filter(|o| o.status.success())
    .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("light"))
}

/// How to open what Dreamer cannot draw (floating panes, tmux modes) in an
/// ordinary terminal.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FallbackSettings {
    /// The terminal to run the attach command in, as argv with one
    /// `{command}` element standing for it, such as
    /// `[kitty, -e, "{command}"]`. Empty: only the copyable command.
    pub launch: Vec<String>,
}

/// When a program's clipboard write (OSC 52) may reach the system
/// clipboard. The daemon addresses each write to one window: the one
/// focused on the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardWrite {
    /// Only while the window is focused (default).
    #[default]
    Focused,
    /// Whenever the write is addressed to this window.
    Allow,
    /// Never.
    Deny,
}

/// How tmux's paste buffers and the system clipboard follow each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardTmux {
    /// Both ways (default): a copy in tmux reaches the clipboard (under
    /// `write`), and a copy in Dreamer becomes tmux's newest buffer.
    #[default]
    Sync,
    /// Only tmux's copies reach the clipboard.
    FromTmux,
    /// Only Dreamer's copies become tmux buffers.
    ToTmux,
    /// Neither.
    Off,
}

impl ClipboardTmux {
    pub fn from_tmux(self) -> bool {
        matches!(self, ClipboardTmux::Sync | ClipboardTmux::FromTmux)
    }
    pub fn to_tmux(self) -> bool {
        matches!(self, ClipboardTmux::Sync | ClipboardTmux::ToTmux)
    }
}

/// What the middle mouse button does in a terminal pane that does not
/// track the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MiddleClick {
    /// Paste tmux's newest buffer, as tmux does (default).
    #[default]
    Paste,
    Off,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClipboardSettings {
    pub write: ClipboardWrite,
    pub tmux: ClipboardTmux,
    /// A finished mouse selection also goes to the system clipboard, not
    /// only to tmux's buffer.
    pub copy_on_select: bool,
    pub middle_click: MiddleClick,
}

/// Which hyperlink schemes a modifier-click may open.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSettings {
    /// Lowercase scheme names.
    pub schemes: Vec<String>,
}

pub const DEFAULT_LINK_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

impl LinkSettings {
    /// Whether `uri` has an allowed scheme. A URI with no scheme is refused.
    pub fn allows(&self, uri: &str) -> bool {
        let Some((scheme, _)) = uri.split_once(':') else {
            return false;
        };
        let scheme = scheme.to_ascii_lowercase();
        self.schemes.contains(&scheme)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FontSettings {
    pub family: String,
    /// Points (logical pixels).
    pub size: f32,
    /// Multiple of the font's own line height (ascent plus descent).
    pub line_height: f32,
    /// Logical pixels added to every cell's width; negative tightens.
    pub letter_spacing: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font: FontSettings {
                family: DEFAULT_FONT_FAMILY.into(),
                size: DEFAULT_FONT_SIZE,
                line_height: DEFAULT_LINE_HEIGHT,
                letter_spacing: 0.0,
            },
            links: LinkSettings {
                schemes: DEFAULT_LINK_SCHEMES.map(String::from).to_vec(),
            },
            clipboard: ClipboardSettings::default(),
            fallback: FallbackSettings::default(),
            appearance: Appearance::default(),
            terminal: TerminalSettings::default(),
            providers: ProviderSettings::default(),
            tiers: TierSettings::default(),
            agents: crate::agents::AgentSettings::default(),
            drawer: DrawerSettings::default(),
            memory: MemorySettings::default(),
            window: WindowSettings::default(),
            web: WebSettings::default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    font: FontFile,
    #[serde(default)]
    links: LinksFile,
    #[serde(default)]
    clipboard: ClipboardFile,
    #[serde(default)]
    fallback: FallbackFile,
    appearance: Option<Appearance>,
    #[serde(default)]
    terminal: TerminalFile,
    #[serde(default)]
    providers: ProvidersFile,
    #[serde(default)]
    tiers: TiersFile,
    agents: Option<serde_norway::Value>,
    #[serde(default)]
    drawer: DrawerFile,
    #[serde(default)]
    memory: MemoryFile,
    #[serde(default)]
    window: WindowFile,
    #[serde(default)]
    web: WebFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebFile {
    component: Option<WebComponent>,
    url: Option<String>,
    #[serde(default)]
    downloads: DownloadsFile,
    color: Option<WebColor>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadsFile {
    folder: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowFile {
    decorations: Option<Decorations>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryFile {
    provider: Option<MemoryProvider>,
    #[serde(default)]
    kosmonaut: KosmonautFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KosmonautFile {
    mode: Option<String>,
    path: Option<String>,
    embedder: Option<EmbedderFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbedderFile {
    kind: String,
    path: Option<String>,
    command: Option<Vec<String>>,
    dimension: Option<usize>,
    timeout: Option<serde_norway::Value>,
    model: Option<String>,
    api_key_env: Option<String>,
}

/// `~/x` under the home directory, or an absolute path.
fn absolute(at: &str, v: &str) -> Result<PathBuf, (String, String)> {
    let bad = || {
        (
            at.to_owned(),
            format!("`{v}` is not an absolute path or one under ~/"),
        )
    };
    if let Some(rest) = v.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(bad)?;
        return Ok(PathBuf::from(home).join(rest));
    }
    let p = PathBuf::from(v);
    if p.is_absolute() { Ok(p) } else { Err(bad()) }
}

fn parse_kosmonaut(f: &KosmonautFile) -> Result<KosmonautSettings, (String, String)> {
    match f.mode.as_deref() {
        None | Some("embedded") => {}
        Some("server") => {
            return Err((
                "memory.kosmonaut.mode".into(),
                "server is not available yet; embedded is".into(),
            ));
        }
        Some(m) => {
            return Err((
                "memory.kosmonaut.mode".into(),
                format!("`{m}` is not embedded"),
            ));
        }
    }
    let path = f
        .path
        .as_deref()
        .map(|p| absolute("memory.kosmonaut.path", p))
        .transpose()?;
    let embedder = match &f.embedder {
        None => None,
        Some(e) => Some(parse_embedder(e)?),
    };
    Ok(KosmonautSettings { path, embedder })
}

fn parse_embedder(e: &EmbedderFile) -> Result<EmbedderSettings, (String, String)> {
    let at = |k: &str| format!("memory.kosmonaut.embedder.{k}");
    let unused = |keys: &[(&str, bool)]| -> Result<(), (String, String)> {
        match keys.iter().find(|(_, set)| *set) {
            Some((k, _)) => Err((at(k), format!("not used by kind {}", e.kind))),
            None => Ok(()),
        }
    };
    match e.kind.as_str() {
        "static" => {
            unused(&[
                ("command", e.command.is_some()),
                ("dimension", e.dimension.is_some()),
                ("timeout", e.timeout.is_some()),
                ("model", e.model.is_some()),
                ("api_key_env", e.api_key_env.is_some()),
            ])?;
            let p = e
                .path
                .as_deref()
                .ok_or_else(|| (at("path"), "the word vector file is required".into()))?;
            Ok(EmbedderSettings::Static {
                path: absolute(&at("path"), p)?,
            })
        }
        "local" => {
            unused(&[
                ("path", e.path.is_some()),
                ("model", e.model.is_some()),
                ("api_key_env", e.api_key_env.is_some()),
            ])?;
            let command = e
                .command
                .clone()
                .filter(|c| c.first().is_some_and(|p| !p.trim().is_empty()))
                .ok_or_else(|| {
                    (
                        at("command"),
                        "the program and its arguments are required".into(),
                    )
                })?;
            let dimension = e
                .dimension
                .filter(|d| (1..=65_536).contains(d))
                .ok_or_else(|| {
                    (
                        at("dimension"),
                        "the vector length (1 to 65536) is required".into(),
                    )
                })?;
            let timeout = match &e.timeout {
                None => DEFAULT_EMBEDDER_TIMEOUT,
                Some(v) => parse_wait(v)
                    .map_err(|m| (at("timeout"), m))?
                    .ok_or_else(|| (at("timeout"), "a local embedder needs a timeout".into()))?,
            };
            Ok(EmbedderSettings::Local {
                command,
                dimension,
                timeout,
            })
        }
        "openai" => Err((
            at("kind"),
            "openai is not available yet: Dreamer opens no network connections of its own; \
             use static, or local with a program that runs your model"
                .into(),
        )),
        k => Err((at("kind"), format!("`{k}` is not static or local"))),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DrawerFile {
    side: Option<DrawerSide>,
    size: Option<serde_norway::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TiersFile {
    /// `30m`, `90s`, `2h`, or `never`.
    cold_after: Option<serde_norway::Value>,
    /// A number, or `none`.
    warm_limit: Option<serde_norway::Value>,
}

/// `90s`, `30m`, `2h`; `never` is `None`.
fn parse_wait(v: &serde_norway::Value) -> Result<Option<std::time::Duration>, String> {
    let text = match v {
        serde_norway::Value::String(t) => t.trim().to_owned(),
        serde_norway::Value::Number(n) => format!("{n}"),
        _ => String::new(),
    };
    if text == "never" {
        return Ok(None);
    }
    let (n, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => 0,
    };
    match n.parse::<u64>() {
        Ok(n) if secs > 0 && n > 0 => Ok(Some(std::time::Duration::from_secs(n * secs))),
        _ => Err(format!(
            "{text:?} is not a wait: write a number with s, m, or h (`30m`), or `never`"
        )),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvidersFile {
    default: Option<String>,
    claude_api: Option<ApiFile>,
    openai_api: Option<ApiFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiFile {
    model: String,
    /// `env` (the vendor's variable), `env:NAME`, or `keychain`.
    #[serde(default)]
    key: Option<String>,
    base_url: Option<String>,
    max_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalFile {
    theme: Option<String>,
    #[serde(default)]
    cursor: CursorFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorFile {
    style: Option<CursorStyle>,
    blink: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackFile {
    launch: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipboardFile {
    write: Option<ClipboardWrite>,
    tmux: Option<ClipboardTmux>,
    copy_on_select: Option<bool>,
    middle_click: Option<MiddleClick>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinksFile {
    schemes: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FontFile {
    family: Option<String>,
    size: Option<f32>,
    line_height: Option<f32>,
    letter_spacing: Option<f32>,
}

/// `<config dir>/dreamer.yaml`.
pub fn settings_path() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("dreamer.yaml"))
}

/// What a new `dreamer.yaml` holds: no settings (every default applies),
/// and where to read about them.
pub const STARTER_SETTINGS: &str = "\
# Dreamer settings. Every setting is optional; a missing one takes its
# default, and a change applies as soon as the file is saved.
#
#   dream config schema   lists every setting, its default, and an example
#   dream config check    checks this file
#
# For example:
#
# font:
#   size: 14
# terminal:
#   theme: catppuccin-mocha
";

/// The settings file at `path`, created with [`STARTER_SETTINGS`] if there
/// is none. An existing file is never changed.
pub fn ensure_settings_file(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => std::io::Write::write_all(&mut f, STARTER_SETTINGS.as_bytes()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn error(origin: Option<&Path>, at: &str, message: String) -> ConfigError {
    ConfigError {
        file: origin.map(Path::to_path_buf),
        line: None,
        column: None,
        at: at.into(),
        message,
    }
}

pub fn parse_settings(text: &str, origin: Option<&Path>) -> Result<Settings, ConfigError> {
    let file: File = if text.trim().is_empty() {
        File::default()
    } else {
        serde_norway::from_str(text).map_err(|e| {
            let loc = e.location();
            ConfigError {
                file: origin.map(Path::to_path_buf),
                line: loc.as_ref().map(serde_norway::Location::line),
                column: loc.as_ref().map(serde_norway::Location::column),
                at: String::new(),
                message: e.to_string(),
            }
        })?
    };
    let mut s = Settings::default();
    if let Some(family) = file.font.family {
        if family.trim().is_empty() {
            return Err(error(origin, "font.family", "must not be empty".into()));
        }
        s.font.family = family;
    }
    if let Some(size) = file.font.size {
        if !(6.0..=72.0).contains(&size) {
            return Err(error(
                origin,
                "font.size",
                format!("{size} is outside 6 to 72 points"),
            ));
        }
        s.font.size = size;
    }
    if let Some(lh) = file.font.line_height {
        if !(0.8..=3.0).contains(&lh) {
            return Err(error(
                origin,
                "font.line_height",
                format!("{lh} is outside 0.8 to 3.0"),
            ));
        }
        s.font.line_height = lh;
    }
    if let Some(ls) = file.font.letter_spacing {
        if !(-3.0..=10.0).contains(&ls) {
            return Err(error(
                origin,
                "font.letter_spacing",
                format!("{ls} is outside -3 to 10 pixels"),
            ));
        }
        s.font.letter_spacing = ls;
    }
    if let Some(schemes) = file.links.schemes {
        let valid = |x: &str| {
            x.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && x.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        };
        if let Some(bad) = schemes.iter().find(|x| !valid(x)) {
            return Err(error(
                origin,
                "links.schemes",
                format!("{bad:?} is not a URI scheme name"),
            ));
        }
        s.links.schemes = schemes.iter().map(|x| x.to_ascii_lowercase()).collect();
    }
    if let Some(w) = file.clipboard.write {
        s.clipboard.write = w;
    }
    if let Some(t) = file.clipboard.tmux {
        s.clipboard.tmux = t;
    }
    if let Some(c) = file.clipboard.copy_on_select {
        s.clipboard.copy_on_select = c;
    }
    if let Some(m) = file.clipboard.middle_click {
        s.clipboard.middle_click = m;
    }
    if let Some(a) = file.appearance {
        s.appearance = a;
    }
    if let Some(name) = file.terminal.theme {
        // The user's theme files sit beside this settings file.
        let dir = origin.and_then(Path::parent).map(|d| d.join("themes"));
        let found = crate::themes::find(&name, dir.as_deref())
            .map_err(|e| error(origin, "terminal.theme", e))?;
        s.terminal.theme = Some(found.ok_or_else(|| {
            let names: Vec<&str> = terminal_core::schemes::all().map(|s| s.name).collect();
            error(
                origin,
                "terminal.theme",
                format!(
                    "no theme `{name}` in {} or built in; built-in ones: {}",
                    dir.as_deref()
                        .map_or_else(|| "themes/".to_owned(), |d| d.display().to_string()),
                    names.join(", ")
                ),
            )
        })?);
    }
    if let Some(style) = file.terminal.cursor.style {
        s.terminal.cursor_style = style;
    }
    if let Some(blink) = file.terminal.cursor.blink {
        s.terminal.cursor_blink = blink;
    }
    if let Some(v) = &file.tiers.cold_after {
        s.tiers.cold_after = parse_wait(v).map_err(|e| error(origin, "tiers.cold_after", e))?;
    }
    if let Some(v) = &file.tiers.warm_limit {
        s.tiers.warm_limit = match v {
            serde_norway::Value::String(t) if t.trim() == "none" => None,
            serde_norway::Value::Number(n) if n.as_u64().is_some() => {
                n.as_u64().map(|n| n as usize)
            }
            _ => {
                return Err(error(
                    origin,
                    "tiers.warm_limit",
                    "must be a whole number of panes, or `none`".into(),
                ));
            }
        };
    }
    if let Some(launch) = file.fallback.launch {
        let slots = launch.iter().filter(|a| *a == "{command}").count();
        if !launch.is_empty() && slots != 1 {
            return Err(error(
                origin,
                "fallback.launch",
                "needs exactly one \"{command}\" element for the attach command".into(),
            ));
        }
        s.fallback.launch = launch;
    }
    if let Some(d) = file.providers.default {
        const NAMES: [&str; 4] = ["claude", "codex", "claude-api", "openai-api"];
        if !NAMES.contains(&d.as_str()) {
            return Err(error(
                origin,
                "providers.default",
                format!("`{d}` is not one of {}", NAMES.join(", ")),
            ));
        }
        s.providers.default = Some(d);
    }
    for (name, file, var, account) in [
        (
            "claude_api",
            file.providers.claude_api,
            "ANTHROPIC_API_KEY",
            "claude-api",
        ),
        (
            "openai_api",
            file.providers.openai_api,
            "OPENAI_API_KEY",
            "openai-api",
        ),
    ] {
        let Some(f) = file else { continue };
        let at = |k: &str| format!("providers.{name}.{k}");
        if f.model.trim().is_empty() {
            return Err(error(origin, &at("model"), "must not be empty".into()));
        }
        let key = match f.key.as_deref() {
            None | Some("env") => ApiKey::Env(var.into()),
            Some("keychain") => ApiKey::Keychain(account.into()),
            Some(k) => match k.strip_prefix("env:") {
                Some(v)
                    if !v.is_empty()
                        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
                {
                    ApiKey::Env(v.into())
                }
                _ => {
                    return Err(error(
                        origin,
                        &at("key"),
                        format!(
                            "`{k}` is not `env`, `env:NAME`, or `keychain` (a key itself never goes here)"
                        ),
                    ));
                }
            },
        };
        if let Some(u) = &f.base_url
            && !(u.starts_with("https://")
                || u.starts_with("http://127.0.0.1")
                || u.starts_with("http://localhost"))
        {
            return Err(error(
                origin,
                &at("base_url"),
                format!("`{u}` must be https (or http on this machine)"),
            ));
        }
        let max_tokens = f.max_tokens.unwrap_or(4096);
        if !(1..=200_000).contains(&max_tokens) {
            return Err(error(
                origin,
                &at("max_tokens"),
                format!("{max_tokens} is outside 1 to 200000"),
            ));
        }
        let api = ApiProvider {
            model: f.model,
            key,
            base_url: f.base_url,
            max_tokens,
        };
        if name == "claude_api" {
            s.providers.claude_api = Some(api);
        } else {
            s.providers.openai_api = Some(api);
        }
    }
    if let Some(v) = &file.agents {
        s.agents = crate::agents::parse(v).map_err(|(at, m)| error(origin, &at, m))?;
    }
    if let Some(side) = file.drawer.side {
        s.drawer.side = side;
    }
    if let Some(v) = &file.drawer.size {
        let text = match v {
            serde_norway::Value::String(t) => t.trim().trim_end_matches('%').trim().to_owned(),
            serde_norway::Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        s.drawer.size = match text.parse::<u32>() {
            Ok(n) if (10..=90).contains(&n) => n,
            _ => {
                return Err(error(
                    origin,
                    "drawer.size",
                    format!("{text:?} is not a percent from 10 to 90"),
                ));
            }
        };
    }
    if let Some(p) = file.memory.provider {
        s.memory.provider = p;
    }
    s.memory.kosmonaut =
        parse_kosmonaut(&file.memory.kosmonaut).map_err(|(at, m)| error(origin, &at, m))?;
    if let Some(d) = file.window.decorations {
        s.window.decorations = d;
    }
    if let Some(c) = file.web.component {
        s.web.component = c;
    }
    if let Some(c) = file.web.color {
        s.web.color = c;
    }
    if let Some(u) = file.web.url {
        let u = u.trim();
        if !["https://", "http://", "file://"]
            .iter()
            .any(|p| u.starts_with(p) && u.len() > p.len())
        {
            return Err(error(
                origin,
                "web.url",
                format!("`{u}` is not an https, http, or file address"),
            ));
        }
        s.web.url = Some(u.to_owned());
    }
    if let Some(folder) = file.web.downloads.folder {
        let f = folder.trim();
        if !(f == "~" || f.starts_with("~/") || Path::new(f).is_absolute()) {
            return Err(error(
                origin,
                "web.downloads.folder",
                format!("{folder:?} is not a folder: write an absolute path, or one from `~/`"),
            ));
        }
        s.web.downloads.folder = Some(f.to_owned());
    }
    Ok(s)
}

/// The user's settings: defaults when the file does not exist, an error
/// when it exists but is invalid.
pub fn load_settings() -> Result<Settings, ConfigError> {
    let Some(path) = settings_path() else {
        return Ok(Settings::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_settings(&text, Some(&path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(error(Some(&path), "", e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_jetbrains_mono_nerd_font_at_13_points() {
        let s = parse_settings("", None).expect("empty");
        assert_eq!(s, Settings::default());
        assert_eq!(s.font.family, "JetBrainsMono Nerd Font Mono");
        assert_eq!(s.font.size, 13.0);
    }

    #[test]
    fn a_new_settings_file_holds_the_defaults_and_an_existing_one_is_kept() {
        assert_eq!(
            parse_settings(STARTER_SETTINGS, None).expect("starter"),
            Settings::default()
        );
        let dir = std::env::temp_dir().join(format!("dreamer-starter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("dreamer.yaml");
        ensure_settings_file(&path).expect("create");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            STARTER_SETTINGS
        );
        std::fs::write(&path, "font:\n  size: 15\n").expect("write");
        ensure_settings_file(&path).expect("keep");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "font:\n  size: 15\n"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn memory_takes_a_known_provider() {
        let s = parse_settings("memory:\n  provider: files\n", None).expect("files");
        assert_eq!(s.memory.provider, MemoryProvider::Files);
        let e = parse_settings("memory:\n  provider: notes\n", None).expect_err("notes");
        assert!(e.message.contains("files"), "{e}");
        assert!(parse_settings("memory:\n  path: x\n", None).is_err());
    }

    #[test]
    fn kosmonaut_is_embedded_and_its_embedder_is_optional() {
        let s = parse_settings("memory:\n  provider: kosmonaut\n", None).expect("kosmonaut");
        assert_eq!(s.memory.provider, MemoryProvider::Kosmonaut);
        assert_eq!(s.memory.kosmonaut, KosmonautSettings::default());
        let s = parse_settings(
            "memory:\n  provider: kosmonaut\n  kosmonaut:\n    mode: embedded\n    path: /data/m\n    \
             embedder:\n      kind: local\n      command: [embed, --stdin]\n      dimension: 384\n      timeout: 5s\n",
            None,
        )
        .expect("local");
        assert_eq!(s.memory.kosmonaut.path, Some(PathBuf::from("/data/m")));
        assert_eq!(
            s.memory.kosmonaut.embedder,
            Some(EmbedderSettings::Local {
                command: vec!["embed".into(), "--stdin".into()],
                dimension: 384,
                timeout: std::time::Duration::from_secs(5),
            })
        );
        let s = parse_settings(
            "memory:\n  kosmonaut:\n    embedder:\n      kind: static\n      path: /v/words.vec\n",
            None,
        )
        .expect("static");
        assert_eq!(
            s.memory.kosmonaut.embedder,
            Some(EmbedderSettings::Static {
                path: PathBuf::from("/v/words.vec")
            })
        );
        for (bad, why) in [
            ("    mode: server\n", "not available yet"),
            ("    path: relative\n", "absolute"),
            (
                "    embedder:\n      kind: openai\n      api_key_env: OPENAI_API_KEY\n",
                "network",
            ),
            ("    embedder:\n      kind: static\n", "required"),
            (
                "    embedder:\n      kind: local\n      command: [x]\n",
                "vector length",
            ),
            (
                "    embedder:\n      kind: static\n      path: /v\n      dimension: 3\n",
                "not used",
            ),
            ("    embedder:\n      kind: magic\n", "magic"),
        ] {
            let e = parse_settings(&format!("memory:\n  kosmonaut:\n{bad}"), None).expect_err(bad);
            assert!(e.message.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn tier_budgets_take_waits_counts_and_words() {
        let s = parse_settings("tiers:\n  cold_after: 90s\n  warm_limit: 10\n", None).expect("ok");
        assert_eq!(s.tiers.cold_after, Some(std::time::Duration::from_secs(90)));
        assert_eq!(s.tiers.warm_limit, Some(10));
        let s =
            parse_settings("tiers:\n  cold_after: never\n  warm_limit: none\n", None).expect("ok");
        assert_eq!(
            s.tiers,
            TierSettings {
                cold_after: None,
                warm_limit: None
            }
        );
        for bad in [
            "cold_after: 30",
            "cold_after: 5d",
            "cold_after: 0m",
            "warm_limit: -1",
            "warm_limit: lots",
        ] {
            let e = parse_settings(&format!("tiers:\n  {bad}\n"), None).expect_err(bad);
            assert!(e.at.starts_with("tiers."), "{bad}: {e:?}");
        }
    }

    #[test]
    fn terminal_theme_and_cursor_are_validated() {
        let s = parse_settings(
            "terminal:\n  theme: Catppuccin_Mocha\n  cursor:\n    style: bar\n    blink: true\n",
            None,
        )
        .expect("terminal");
        assert_eq!(s.scheme().name, "catppuccin-mocha");
        assert_eq!(s.terminal.cursor_style, CursorStyle::Bar);
        assert!(s.terminal.cursor_blink);
        let dark = parse_settings("appearance: dark", None).expect("dark");
        assert_eq!(dark.scheme().name, "dreamer-dark");
        let light = parse_settings("appearance: light", None).expect("light");
        assert_eq!(light.scheme().name, "dreamer-light");
        // An explicit theme wins over the appearance.
        let both = parse_settings(
            "appearance: light\nterminal:\n  theme: dreamer-dark\n",
            None,
        )
        .expect("both");
        assert_eq!(both.scheme().name, "dreamer-dark");
        let e = parse_settings("terminal:\n  theme: nope\n", None).expect_err("unknown");
        assert_eq!(e.at, "terminal.theme");
        assert!(e.message.contains("dreamer-light"), "{e}");
        let e = parse_settings("terminal:\n  cursor:\n    style: beam\n", None).expect_err("style");
        assert_eq!(e.line, Some(3), "{e}");
    }

    #[test]
    fn fields_override_defaults_one_by_one() {
        let s = parse_settings("font:\n  size: 15\n", None).expect("size");
        assert_eq!(s.font.size, 15.0);
        assert_eq!(s.font.family, DEFAULT_FONT_FAMILY);
        let s = parse_settings("font: { family: Menlo, line_height: 1.4 }", None).expect("family");
        assert_eq!((s.font.family.as_str(), s.font.line_height), ("Menlo", 1.4));
    }

    #[test]
    fn api_providers_are_configured_with_a_model_and_a_key_source_never_a_key() {
        let s = parse_settings(
            "providers:\n  default: claude-api\n  claude_api:\n    model: some-model\n  openai_api:\n    model: other\n    key: keychain\n    max_tokens: 1000\n",
            None,
        )
        .expect("valid");
        assert_eq!(s.providers.default.as_deref(), Some("claude-api"));
        let c = s.providers.claude_api.expect("claude");
        assert_eq!(
            (c.model.as_str(), c.key, c.max_tokens),
            ("some-model", ApiKey::Env("ANTHROPIC_API_KEY".into()), 4096)
        );
        let o = s.providers.openai_api.expect("openai");
        assert_eq!(o.key, ApiKey::Keychain("openai-api".into()));
        let bad = |y: &str, at: &str| {
            let e = parse_settings(y, None).expect_err(y);
            assert!(e.to_string().contains(at), "{e}");
        };
        bad(
            "providers:\n  claude_api:\n    model: m\n    key: sk-ant-123\n",
            "a key itself never goes here",
        );
        bad(
            "providers:\n  claude_api:\n    model: m\n    base_url: http://example.com\n",
            "must be https",
        );
        bad("providers:\n  default: gpt\n", "providers.default");
        assert!(
            parse_settings("providers:\n  claude_api: {}\n", None).is_err(),
            "a model is required"
        );
    }

    #[test]
    fn typos_and_bad_values_are_errors_with_a_location() {
        let e = parse_settings("font:\n  sise: 15\n", None).expect_err("typo");
        assert_eq!(e.line, Some(2), "{e}");
        assert!(e.message.contains("sise"), "{e}");
        let e = parse_settings("font: { size: 200 }", None).expect_err("range");
        assert_eq!(e.at, "font.size");
        let s = parse_settings("font: { letter_spacing: -0.5 }", None).expect("tighter");
        assert_eq!(s.font.letter_spacing, -0.5);
        let e = parse_settings("font: { letter_spacing: -8 }", None).expect_err("range");
        assert_eq!(e.at, "font.letter_spacing");
        assert!(parse_settings("font: { family: '  ' }", None).is_err());
        assert!(parse_settings("colors: {}", None).is_err());
        let e = parse_settings("links: { schemes: ['https:'] }", None).expect_err("scheme");
        assert_eq!(e.at, "links.schemes");
    }

    #[test]
    fn the_drawer_docks_at_the_bottom_at_35_percent_unless_configured() {
        let s = parse_settings("", None).expect("empty");
        assert_eq!(s.drawer.side, DrawerSide::Bottom);
        assert_eq!(s.drawer.size, 35);
        let s = parse_settings("drawer:\n  side: right\n  size: 40\n", None).expect("valid");
        assert_eq!(
            s.drawer,
            DrawerSettings {
                side: DrawerSide::Right,
                size: 40
            }
        );
        let s = parse_settings("drawer: {size: 50%}\n", None).expect("percent sign");
        assert_eq!(s.drawer.size, 50);
        for (bad, at) in [
            ("drawer: {side: top}", "drawer"),
            ("drawer: {size: 5}", "drawer.size"),
            ("drawer: {size: 95}", "drawer.size"),
            ("drawer: {size: half}", "drawer.size"),
            ("drawer: {width: 30}", "drawer"),
        ] {
            let e = parse_settings(bad, None).expect_err(bad);
            assert!(
                e.at.starts_with(at) || e.message.contains("drawer"),
                "{bad}: {e:?}"
            );
        }
    }

    #[test]
    fn a_bad_agents_block_is_refused_at_its_key_path() {
        let s = parse_settings("agents:\n  default: codex\n", None).expect("valid");
        assert_eq!(s.agents.default_harness(), "codex");
        let e = parse_settings("agents:\n  pi:\n    modle: x\n", None).expect_err("typo");
        assert_eq!(e.at, "agents.pi", "{e}");
        assert!(e.to_string().contains("modle"), "{e}");
    }

    #[test]
    fn appearance_follows_the_system_unless_set() {
        assert_eq!(Settings::default().appearance, Appearance::System);
        let s = parse_settings("appearance: light", None).expect("light");
        assert_eq!(s.appearance, Appearance::Light);
        assert!(parse_settings("appearance: sepia", None).is_err());
    }

    #[test]
    fn dreamer_web_is_asked_for_unless_the_settings_say_auto_or_never() {
        let s = Settings::default();
        assert_eq!(s.web.component, WebComponent::Ask);
        assert_eq!(s.web.url, None);
        for (text, want) in [
            ("ask", WebComponent::Ask),
            ("auto", WebComponent::Auto),
            ("never", WebComponent::Never),
        ] {
            let s = parse_settings(&format!("web:\n  component: {text}\n"), None).expect(text);
            assert_eq!(s.web.component, want);
            assert_eq!(want.name(), text);
        }
        let e = parse_settings("web:\n  component: always\n", None).expect_err("bad");
        assert!(e.to_string().contains("always"), "{e}");
        let s =
            parse_settings("web:\n  url: https://example.com/dreamer-web/\n", None).expect("url");
        assert_eq!(
            s.web.url.as_deref(),
            Some("https://example.com/dreamer-web/")
        );
        let e = parse_settings("web:\n  url: example.com\n", None).expect_err("no scheme");
        assert_eq!(e.at, "web.url");
        assert!(parse_settings("web:\n  urls: x\n", None).is_err());
    }

    #[test]
    fn windows_keep_their_title_bar_unless_decorations_are_none() {
        assert_eq!(Settings::default().window.decorations, Decorations::Full);
        let s = parse_settings("window:\n  decorations: none\n", None).expect("none");
        assert_eq!(s.window.decorations, Decorations::None);
        let e = parse_settings("window:\n  decorations: minimal\n", None).expect_err("bad");
        assert_eq!(e.line, Some(2), "{e}");
        assert!(e.message.contains("full"), "{e}");
    }

    #[test]
    fn web_pages_are_drawn_in_srgb_unless_the_settings_say_display() {
        assert_eq!(Settings::default().web.color, WebColor::Srgb);
        for (text, want) in [("srgb", WebColor::Srgb), ("display", WebColor::Display)] {
            let s = parse_settings(&format!("web:\n  color: {text}\n"), None).expect(text);
            assert_eq!(s.web.color, want);
            assert_eq!(want.name(), text);
        }
        let e = parse_settings("web:\n  color: p3\n", None).expect_err("bad");
        assert!(e.to_string().contains("p3"), "{e}");
    }

    #[test]
    fn the_downloads_folder_is_absolute_or_under_home() {
        assert_eq!(Settings::default().web.downloads.folder, None);
        for ok in ["~", "~/Inbox", "/tmp/dl"] {
            let s = parse_settings(&format!("web:\n  downloads:\n    folder: {ok:?}\n"), None)
                .expect(ok);
            assert_eq!(s.web.downloads.folder.as_deref(), Some(ok));
        }
        for bad in ["Downloads", "~ada/x", "''"] {
            let e = parse_settings(&format!("web: {{ downloads: {{ folder: {bad} }} }}"), None)
                .expect_err(bad);
            assert_eq!(e.at, "web.downloads.folder", "{bad}: {e}");
        }
        assert!(parse_settings("web: { downloads: { dir: /x } }", None).is_err());
    }

    #[test]
    fn a_fallback_launch_needs_one_command_slot() {
        let s =
            parse_settings("fallback: { launch: [kitty, -e, '{command}'] }", None).expect("launch");
        assert_eq!(s.fallback.launch, ["kitty", "-e", "{command}"]);
        let e = parse_settings("fallback: { launch: [kitty] }", None).expect_err("no slot");
        assert_eq!(e.at, "fallback.launch");
        assert!(Settings::default().fallback.launch.is_empty());
    }

    #[test]
    fn clipboard_writes_default_to_the_focused_window() {
        assert_eq!(Settings::default().clipboard.write, ClipboardWrite::Focused);
        assert_eq!(Settings::default().clipboard.tmux, ClipboardTmux::Sync);
        let c = parse_settings(
            "clipboard:\n  tmux: from_tmux\n  copy_on_select: true\n  middle_click: off\n",
            None,
        )
        .expect("clipboard")
        .clipboard;
        assert!(c.tmux.from_tmux() && !c.tmux.to_tmux());
        assert!(c.copy_on_select);
        assert_eq!(c.middle_click, MiddleClick::Off);
        assert!(parse_settings("clipboard:\n  tmux: sometimes\n", None).is_err());
        let s = parse_settings("clipboard: { write: deny }", None).expect("deny");
        assert_eq!(s.clipboard.write, ClipboardWrite::Deny);
        assert!(parse_settings("clipboard: { write: sometimes }", None).is_err());
    }

    #[test]
    fn only_listed_link_schemes_open() {
        let d = Settings::default().links;
        assert!(d.allows("https://example.com"));
        assert!(d.allows("HTTP://example.com"));
        assert!(d.allows("mailto:someone@example.com"));
        assert!(!d.allows("file:///etc/passwd"));
        assert!(!d.allows("javascript:alert(1)"));
        assert!(!d.allows("no-scheme-here"));
        let s = parse_settings("links: { schemes: [https, SSH] }", None).expect("schemes");
        assert!(s.links.allows("ssh://host") && !s.links.allows("http://x"));
    }
}
