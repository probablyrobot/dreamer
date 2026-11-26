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
    pub render: RenderSettings,
}

/// How the desktop draws terminal panes, and when the daemon switches a
/// pane a window allowed frames for between the byte stream and frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderSettings {
    pub frames: RenderFrames,
    /// The daemon switches a pane to frames once it writes more than this
    /// many bytes a second over `frames_dwell`.
    pub frames_above: u64,
    /// ... and back to the stream once it writes less than this.
    pub frames_below: u64,
    /// The least time a pane stays on either feed before the daemon
    /// switches it, and the window its rate is measured over.
    pub frames_dwell: std::time::Duration,
    /// How long a pane stays on the stream after the user stops working
    /// with its history (selecting, scrolling back, searching).
    pub frames_hold: std::time::Duration,
}

/// The defaults come from measured output rates on an M3 Max (tmux 3.8,
/// rates of `%output` over 1 s windows): typing at a prompt, `ping`, a
/// build log, and `top` stay under 5 KiB/s, a full-screen program
/// redrawing 30 times a second about 115 KiB/s, while `cat` of a large
/// file or `seq` write 8 to 14 MiB/s. A listing of a large directory
/// tree comes in bursts of up to about 1.4 MiB/s for a second or less.
pub const DEFAULT_FRAMES_ABOVE: u64 = 1 << 20;
pub const DEFAULT_FRAMES_BELOW: u64 = 256 << 10;
pub const DEFAULT_FRAMES_DWELL: std::time::Duration = std::time::Duration::from_secs(1);
pub const DEFAULT_FRAMES_HOLD: std::time::Duration = std::time::Duration::from_secs(2);

impl Default for RenderSettings {
    fn default() -> Self {
        RenderSettings {
            frames: RenderFrames::Off,
            frames_above: DEFAULT_FRAMES_ABOVE,
            frames_below: DEFAULT_FRAMES_BELOW,
            frames_dwell: DEFAULT_FRAMES_DWELL,
            frames_hold: DEFAULT_FRAMES_HOLD,
        }
    }
}

/// Whether a desktop window may show a pane from the daemon's screen
/// frames instead of following every byte with its own engine
/// (`docs/desktop-internals.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RenderFrames {
    /// Never: every pane follows the byte stream.
    #[default]
    Off,
    /// The window accepts frames for a pane the daemon switches to them.
    On,
    /// Every pane the window shows is followed on frames (for checks).
    Always,
}

impl RenderFrames {
    pub fn name(self) -> &'static str {
        match self {
            RenderFrames::Off => "off",
            RenderFrames::On => "on",
            RenderFrames::Always => "always",
        }
    }

    pub fn from_name(name: &str) -> Option<RenderFrames> {
        match name {
            "off" => Some(RenderFrames::Off),
            "on" => Some(RenderFrames::On),
            "always" => Some(RenderFrames::Always),
            _ => None,
        }
    }
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
    pub sqlite: SqliteSettings,
    pub kosmonaut: KosmonautSettings,
    /// `memory.embedder`, shared by `sqlite` and `kosmonaut`. `None`: no
    /// vectors; recall by words and links.
    pub embedder: Option<EmbedderSettings>,
    /// What the memory settings should change, though they still work: a
    /// key that moved. `dream config check` shows these, and `dreamerd`
    /// logs them.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryProvider {
    /// Markdown files with frontmatter and a `MEMORY.md` index.
    Files,
    /// One SQLite database for the user, compiled into `dreamerd`; what
    /// the files hold is copied in once.
    #[default]
    Sqlite,
    /// A Kosmonaut server, over gRPC.
    Kosmonaut,
}

impl MemoryProvider {
    pub fn name(self) -> &'static str {
        match self {
            MemoryProvider::Files => "files",
            MemoryProvider::Sqlite => "sqlite",
            MemoryProvider::Kosmonaut => "kosmonaut",
        }
    }
}

/// `memory.sqlite`: where the database is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SqliteSettings {
    /// The database file; `None`: `memory.sqlite` in Dreamer's data
    /// directory.
    pub path: Option<PathBuf>,
}

/// `memory.kosmonaut`: the server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KosmonautSettings {
    /// `None` when no `url` is set; `provider: kosmonaut` needs one.
    pub server: Option<KosmonautServer>,
}

/// `memory.kosmonaut`: a Kosmonaut server over gRPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KosmonautServer {
    /// `grpc://host:port`, or `unix:/path` for a socket on this machine.
    pub url: String,
    /// TLS to the server. Off only to this machine.
    pub tls: bool,
    /// The environment variable that holds the token, read at each call;
    /// `None`: no token (a server with authentication off).
    pub token_env: Option<String>,
}

/// `memory.embedder`: what turns memories into vectors.
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
    /// OpenAI's embeddings API (`POST <url>/embeddings`), with the key
    /// read from the environment variable `key_env` at each call.
    OpenAi {
        model: String,
        key_env: String,
        /// The API base, [`OPENAI_URL`] unless set. Plain `http` only to
        /// this machine (a stand-in for tests).
        url: String,
        /// Sent as `dimensions` when set; otherwise the model's own.
        dimension: Option<usize>,
        timeout: std::time::Duration,
    },
}

/// The API base an `openai` embedder uses unless `url` names another.
pub const OPENAI_URL: &str = "https://api.openai.com/v1";

/// The longest an embedder may take for one call (a local program's run,
/// or one request to an API), by default.
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
/// their own. The defaults come from the M7 measurements in
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
    /// At most this much memory, in bytes, held by every mirror on the
    /// host together; beyond it the quietest panes that nothing follows go
    /// Cold. `None`: no ceiling.
    pub warm_memory: Option<u64>,
}

pub const DEFAULT_COLD_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);
pub const DEFAULT_WARM_LIMIT: usize = 128;
/// 1 GiB: the 128 Warm panes `warm_limit` allows at about 8,000 lines of
/// 120 columns each, or 512 panes at tmux's default 2,000 lines.
pub const DEFAULT_WARM_MEMORY: u64 = 1 << 30;

impl Default for TierSettings {
    fn default() -> Self {
        TierSettings {
            cold_after: Some(DEFAULT_COLD_AFTER),
            warm_limit: Some(DEFAULT_WARM_LIMIT),
            warm_memory: Some(DEFAULT_WARM_MEMORY),
        }
    }
}

/// Agent providers. The CLIs need nothing here; an API
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
/// canonical mirror: views get them through checkpoints.
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
            render: RenderSettings::default(),
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
    #[serde(default)]
    render: RenderFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderFile {
    frames: Option<RenderFrames>,
    /// `1MiB`, `256KiB`: bytes a second.
    frames_above: Option<serde_norway::Value>,
    frames_below: Option<serde_norway::Value>,
    /// `1s`, `500ms`.
    frames_dwell: Option<serde_norway::Value>,
    frames_hold: Option<serde_norway::Value>,
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
    sqlite: SqliteFile,
    #[serde(default)]
    kosmonaut: KosmonautFile,
    embedder: Option<EmbedderFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SqliteFile {
    path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KosmonautFile {
    mode: Option<String>,
    path: Option<String>,
    url: Option<String>,
    tls: Option<bool>,
    token_env: Option<String>,
    /// Never accepted: read only to say where a token goes instead.
    token: Option<serde_norway::Value>,
    /// Moved to `memory.embedder`; still read, with a note.
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
    key_env: Option<String>,
    url: Option<String>,
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

/// What `mode: embedded` and `path` get: embedded Kosmonaut is not in
/// this Dreamer.
const NO_EMBEDDED: &str = "embedded Kosmonaut is not in this Dreamer; it returns when Kosmonaut \
     is published. Use provider: sqlite for a database on this machine, or a Kosmonaut server \
     (url)";

fn parse_kosmonaut(f: &KosmonautFile) -> Result<KosmonautSettings, (String, String)> {
    let at = |k: &str| format!("memory.kosmonaut.{k}");
    if f.token.is_some() {
        return Err((
            at("token"),
            "a token is never kept in the settings: put it in an environment variable and name \
             that variable in token_env"
                .into(),
        ));
    }
    match f.mode.as_deref() {
        None | Some("server") => {}
        Some("embedded") => return Err((at("mode"), NO_EMBEDDED.into())),
        Some(m) => return Err((at("mode"), format!("`{m}` is not server"))),
    }
    if f.path.is_some() {
        return Err((at("path"), NO_EMBEDDED.into()));
    }
    let server = if f.url.is_some() || f.mode.is_some() {
        Some(parse_server(f)?)
    } else {
        let set = [
            ("tls", f.tls.is_some()),
            ("token_env", f.token_env.is_some()),
        ];
        if let Some((k, _)) = set.iter().find(|(_, s)| *s) {
            return Err((at(k), "used only with url".into()));
        }
        None
    };
    Ok(KosmonautSettings { server })
}

/// The memory section: provider, sqlite, kosmonaut, and the embedder,
/// read from `memory.embedder` or, with a note, from where it was before
/// (`memory.kosmonaut.embedder`).
fn parse_memory(f: &MemoryFile) -> Result<MemorySettings, (String, String)> {
    let provider = f.provider.unwrap_or_default();
    let kosmonaut = parse_kosmonaut(&f.kosmonaut)?;
    if provider == MemoryProvider::Kosmonaut && kosmonaut.server.is_none() {
        return Err((
            "memory.kosmonaut.url".into(),
            format!("provider: kosmonaut needs a server (grpc://host:port); {NO_EMBEDDED}"),
        ));
    }
    let sqlite = SqliteSettings {
        path: f
            .sqlite
            .path
            .as_deref()
            .map(|p| absolute("memory.sqlite.path", p))
            .transpose()?,
    };
    let mut notes = Vec::new();
    let embedder = match (&f.embedder, &f.kosmonaut.embedder) {
        (Some(_), Some(_)) => {
            return Err((
                "memory.kosmonaut.embedder".into(),
                "moved to memory.embedder, which is set too: keep one".into(),
            ));
        }
        (Some(e), None) => Some(parse_embedder(e, "memory.embedder")?),
        (None, Some(e)) => {
            notes.push(
                "memory.kosmonaut.embedder has moved to memory.embedder (shared by sqlite and \
                 kosmonaut); the old key is still read, but not after the next release"
                    .to_owned(),
            );
            Some(parse_embedder(e, "memory.kosmonaut.embedder")?)
        }
        (None, None) => None,
    };
    Ok(MemorySettings {
        provider,
        sqlite,
        kosmonaut,
        embedder,
        notes,
    })
}

/// The server: a `grpc://host:port` URL with TLS unless `tls: false`,
/// which only this machine may have, so a token never crosses the network
/// in the clear; or `unix:/path`, a socket on this machine, without TLS.
fn parse_server(f: &KosmonautFile) -> Result<KosmonautServer, (String, String)> {
    let at = |k: &str| format!("memory.kosmonaut.{k}");
    let url = f
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            (
                at("url"),
                "the server is required (grpc://host:port)".into(),
            )
        })?;
    let token_env = match f.token_env.as_deref() {
        None => None,
        Some(k) if !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => {
            Some(k.to_owned())
        }
        Some(_) => {
            return Err((
                at("token_env"),
                "the name of the environment variable that holds the token (KOSMONAUT_TOKEN)"
                    .into(),
            ));
        }
    };
    if let Some(path) = url.strip_prefix("unix:") {
        if !path.starts_with('/') {
            return Err((
                at("url"),
                format!("`{url}`: a socket needs an absolute path"),
            ));
        }
        if f.tls == Some(true) {
            return Err((at("tls"), "a socket on this machine takes no TLS".into()));
        }
        return Ok(KosmonautServer {
            url: url.to_owned(),
            tls: false,
            token_env,
        });
    }
    let bad = || {
        (
            at("url"),
            format!("`{url}` is not grpc://host:port or unix:/path"),
        )
    };
    let rest = url.strip_prefix("grpc://").ok_or_else(bad)?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains(['/', '@', '?', '#']) {
        return Err(bad());
    }
    let (name, port) = match rest.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map(|(h, p)| (h, p.strip_prefix(':'))),
        None => rest.rsplit_once(':').map(|(h, p)| (h, Some(p))),
    }
    .ok_or_else(bad)?;
    if name.is_empty() || port.is_none_or(|p| p.parse::<u16>().is_err()) {
        return Err(bad());
    }
    let tls = f.tls.unwrap_or(true);
    if !tls && !matches!(name, "localhost" | "127.0.0.1" | "::1") {
        return Err((
            at("tls"),
            format!("`{url}`: without TLS only to this machine (localhost)"),
        ));
    }
    Ok(KosmonautServer {
        url: format!("grpc://{rest}"),
        tls,
        token_env,
    })
}

fn parse_embedder(e: &EmbedderFile, key: &str) -> Result<EmbedderSettings, (String, String)> {
    let at = |k: &str| format!("{key}.{k}");
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
                ("key_env", e.key_env.is_some()),
                ("url", e.url.is_some()),
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
                ("key_env", e.key_env.is_some()),
                ("url", e.url.is_some()),
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
        "openai" => {
            unused(&[("path", e.path.is_some()), ("command", e.command.is_some())])?;
            let model = e
                .model
                .clone()
                .filter(|m| !m.trim().is_empty())
                .ok_or_else(|| {
                    (
                        at("model"),
                        "the model is required (text-embedding-3-small)".into(),
                    )
                })?;
            let key_env = e
                .key_env
                .clone()
                .filter(|k| {
                    !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                })
                .ok_or_else(|| {
                    (
                        at("key_env"),
                        "the name of the environment variable that holds the key is required \
                         (OPENAI_API_KEY)"
                            .into(),
                    )
                })?;
            let url = match e.url.as_deref() {
                None => OPENAI_URL.to_owned(),
                Some(u) => api_base(u).map_err(|m| (at("url"), m))?,
            };
            let dimension = match e.dimension {
                None => None,
                Some(d) if (1..=65_536).contains(&d) => Some(d),
                Some(_) => {
                    return Err((at("dimension"), "the vector length is 1 to 65536".into()));
                }
            };
            let timeout = match &e.timeout {
                None => DEFAULT_EMBEDDER_TIMEOUT,
                Some(v) => parse_wait(v)
                    .map_err(|m| (at("timeout"), m))?
                    .ok_or_else(|| (at("timeout"), "an openai embedder needs a timeout".into()))?,
            };
            Ok(EmbedderSettings::OpenAi {
                model,
                key_env,
                url,
                dimension,
                timeout,
            })
        }
        k => Err((at("kind"), format!("`{k}` is not static, local, or openai"))),
    }
}

/// An API base URL: `https`, or plain `http` to this machine only, so a
/// key never crosses the network in the clear. Without a trailing `/`.
fn api_base(u: &str) -> Result<String, String> {
    let u = u.trim().trim_end_matches('/');
    let (scheme, rest) = u
        .split_once("://")
        .ok_or_else(|| format!("`{u}` is not an http or https URL"))?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() || host.contains('@') || rest.contains(['?', '#']) {
        return Err(format!(
            "`{u}` is not an API base URL (a host, and a path at most)"
        ));
    }
    // The host without its port; an IPv6 literal without its brackets.
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    match scheme {
        "https" => Ok(u.to_owned()),
        "http" if matches!(name, "localhost" | "127.0.0.1" | "::1") => Ok(u.to_owned()),
        "http" => Err(format!(
            "`{u}`: plain http only to this machine (localhost); use https"
        )),
        _ => Err(format!("`{u}` is not an http or https URL")),
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
    /// `512MiB`, `2GiB`, or `none`.
    warm_memory: Option<serde_norway::Value>,
}

/// `64MiB`, `2GiB` (also `KiB`); `none` is `None`.
fn parse_size(v: &serde_norway::Value) -> Result<Option<u64>, String> {
    let text = match v {
        serde_norway::Value::String(t) => t.trim().to_owned(),
        serde_norway::Value::Number(n) => format!("{n}"),
        _ => String::new(),
    };
    if text == "none" {
        return Ok(None);
    }
    let (n, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let scale: u64 = match unit.trim() {
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => 0,
    };
    match n.parse::<u64>().ok().and_then(|n| n.checked_mul(scale)) {
        Some(bytes) if bytes > 0 => Ok(Some(bytes)),
        _ => Err(format!(
            "{text:?} is not a size: write a number with KiB, MiB, or GiB (`512MiB`), or `none`"
        )),
    }
}

/// `500ms`, `1s`, `2m`: a short time, more than zero.
fn parse_short_wait(v: &serde_norway::Value) -> Result<std::time::Duration, String> {
    let text = match v {
        serde_norway::Value::String(t) => t.trim().to_owned(),
        serde_norway::Value::Number(n) => format!("{n}"),
        _ => String::new(),
    };
    let (n, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let ms = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        _ => 0,
    };
    match n.parse::<u64>() {
        Ok(n) if ms > 0 && n > 0 => Ok(std::time::Duration::from_millis(n * ms)),
        _ => Err(format!(
            "{text:?} is not a time: write a number with ms, s, or m (`500ms`, `2s`)"
        )),
    }
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
    if let Some(v) = &file.tiers.warm_memory {
        s.tiers.warm_memory = parse_size(v).map_err(|e| error(origin, "tiers.warm_memory", e))?;
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
    s.memory = parse_memory(&file.memory).map_err(|(at, m)| error(origin, &at, m))?;
    if let Some(d) = file.window.decorations {
        s.window.decorations = d;
    }
    if let Some(c) = file.web.component {
        s.web.component = c;
    }
    if let Some(c) = file.web.color {
        s.web.color = c;
    }
    if let Some(f) = file.render.frames {
        s.render.frames = f;
    }
    for (key, value, to) in [
        (
            "render.frames_above",
            &file.render.frames_above,
            &mut s.render.frames_above,
        ),
        (
            "render.frames_below",
            &file.render.frames_below,
            &mut s.render.frames_below,
        ),
    ] {
        if let Some(v) = value {
            *to = parse_size(v)
                .map_err(|e| error(origin, key, e))?
                .ok_or_else(|| error(origin, key, "must be a rate, not `none`".into()))?;
        }
    }
    for (key, value, to) in [
        (
            "render.frames_dwell",
            &file.render.frames_dwell,
            &mut s.render.frames_dwell,
        ),
        (
            "render.frames_hold",
            &file.render.frames_hold,
            &mut s.render.frames_hold,
        ),
    ] {
        if let Some(v) = value {
            *to = parse_short_wait(v).map_err(|e| error(origin, key, e))?;
        }
    }
    if s.render.frames_below >= s.render.frames_above {
        return Err(error(
            origin,
            "render.frames_below",
            format!(
                "must be less than render.frames_above ({} bytes a second), or a pane at the threshold flips between the feeds",
                s.render.frames_above
            ),
        ));
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
        // sqlite unless the settings say otherwise.
        for text in ["", "font:\n  size: 13\n", "memory:\n  sqlite: {}\n"] {
            let s = parse_settings(text, None).expect("default");
            assert_eq!(s.memory.provider, MemoryProvider::Sqlite, "{text:?}");
        }
        assert_eq!(Settings::default().memory.provider, MemoryProvider::Sqlite);
        let s = parse_settings("memory:\n  provider: files\n", None).expect("files");
        assert_eq!(s.memory.provider, MemoryProvider::Files);
        let e = parse_settings("memory:\n  provider: notes\n", None).expect_err("notes");
        assert!(e.message.contains("files"), "{e}");
        assert!(parse_settings("memory:\n  path: x\n", None).is_err());
    }

    #[test]
    fn kosmonaut_server_mode_names_the_token_variable_not_the_token() {
        let server = |yaml: &str| {
            parse_settings(
                &format!("memory:\n  kosmonaut:\n    mode: server\n{yaml}"),
                None,
            )
            .expect(yaml)
            .memory
            .kosmonaut
            .server
            .expect("server")
        };
        assert_eq!(
            server("    url: grpc://memory.example:9432\n    token_env: KOSMONAUT_TOKEN\n"),
            KosmonautServer {
                url: "grpc://memory.example:9432".into(),
                tls: true,
                token_env: Some("KOSMONAUT_TOKEN".into()),
            }
        );
        assert_eq!(
            server("    url: grpc://127.0.0.1:7687\n    tls: false\n"),
            KosmonautServer {
                url: "grpc://127.0.0.1:7687".into(),
                tls: false,
                token_env: None,
            }
        );
        assert_eq!(
            server("    url: \"grpc://[::1]:7687\"\n    tls: false\n").url,
            "grpc://[::1]:7687"
        );
        // A url alone means a server.
        let s = parse_settings("memory:\n  kosmonaut:\n    url: grpc://h:1\n", None).expect("url");
        assert_eq!(s.memory.kosmonaut.server.map(|k| k.tls), Some(true));
        assert_eq!(
            server("    url: unix:/run/kosmonaut.sock\n"),
            KosmonautServer {
                url: "unix:/run/kosmonaut.sock".into(),
                tls: false,
                token_env: None,
            }
        );
    }

    #[test]
    fn sqlite_and_the_embedder_are_optional_and_the_old_embedder_key_is_noted() {
        let s = parse_settings("memory:\n  provider: sqlite\n", None).expect("sqlite");
        assert_eq!(s.memory.provider, MemoryProvider::Sqlite);
        assert_eq!(s.memory.sqlite, SqliteSettings::default());
        assert_eq!(s.memory.embedder, None);
        assert!(s.memory.notes.is_empty());
        let s = parse_settings(
            "memory:\n  provider: sqlite\n  sqlite:\n    path: /data/m.sqlite\n  \
             embedder:\n    kind: local\n    command: [embed, --stdin]\n    dimension: 384\n    timeout: 5s\n",
            None,
        )
        .expect("local");
        assert_eq!(s.memory.sqlite.path, Some(PathBuf::from("/data/m.sqlite")));
        assert_eq!(
            s.memory.embedder,
            Some(EmbedderSettings::Local {
                command: vec!["embed".into(), "--stdin".into()],
                dimension: 384,
                timeout: std::time::Duration::from_secs(5),
            })
        );
        let s = parse_settings(
            "memory:\n  embedder:\n    kind: static\n    path: /v/words.vec\n",
            None,
        )
        .expect("static");
        assert_eq!(
            s.memory.embedder,
            Some(EmbedderSettings::Static {
                path: PathBuf::from("/v/words.vec")
            })
        );
        let s = parse_settings(
            "memory:\n  embedder:\n    kind: openai\n    model: text-embedding-3-small\n    \
             key_env: OPENAI_API_KEY\n",
            None,
        )
        .expect("openai");
        assert_eq!(
            s.memory.embedder,
            Some(EmbedderSettings::OpenAi {
                model: "text-embedding-3-small".into(),
                key_env: "OPENAI_API_KEY".into(),
                url: OPENAI_URL.into(),
                dimension: None,
                timeout: DEFAULT_EMBEDDER_TIMEOUT,
            })
        );
        let s = parse_settings(
            "memory:\n  embedder:\n    kind: openai\n    model: m\n    key_env: K\n    \
             url: http://127.0.0.1:8080/v1/\n    dimension: 256\n    timeout: 5s\n",
            None,
        )
        .expect("openai stand-in");
        assert_eq!(
            s.memory.embedder,
            Some(EmbedderSettings::OpenAi {
                model: "m".into(),
                key_env: "K".into(),
                url: "http://127.0.0.1:8080/v1".into(),
                dimension: Some(256),
                timeout: std::time::Duration::from_secs(5),
            })
        );
        // The key it had before still works, with a note.
        let s = parse_settings(
            "memory:\n  provider: sqlite\n  kosmonaut:\n    embedder:\n      kind: static\n      \
             path: /v/words.vec\n",
            None,
        )
        .expect("old key");
        assert_eq!(
            s.memory.embedder,
            Some(EmbedderSettings::Static {
                path: PathBuf::from("/v/words.vec")
            })
        );
        assert_eq!(s.memory.notes.len(), 1);
        assert!(
            s.memory.notes[0].contains("memory.embedder"),
            "{:?}",
            s.memory.notes
        );
        let e = parse_settings(
            "memory:\n  kosmonaut:\n    embedder:\n      kind: magic\n",
            None,
        )
        .expect_err("old key, checked");
        assert!(
            e.to_string().contains("memory.kosmonaut.embedder.kind"),
            "{e}"
        );
        let e = parse_settings(
            "memory:\n  embedder:\n    kind: static\n    path: /a\n  kosmonaut:\n    embedder:\n      \
             kind: static\n      path: /b\n",
            None,
        )
        .expect_err("both");
        assert!(e.message.contains("keep one"), "{e}");
        for (bad, why) in [
            ("  provider: kosmonaut\n", "needs a server"),
            ("  kosmonaut:\n    mode: server\n", "server is required"),
            (
                "  kosmonaut:\n    mode: embedded\n",
                "returns when Kosmonaut is published",
            ),
            ("  kosmonaut:\n    path: /data/m\n", "provider: sqlite"),
            ("  kosmonaut:\n    mode: cloud\n", "not server"),
            (
                "  kosmonaut:\n    token_env: KOSMONAUT_TOKEN\n",
                "only with url",
            ),
            (
                "  kosmonaut:\n    url: grpc://h:1\n    token: abc\n",
                "token_env",
            ),
            ("  kosmonaut:\n    url: grpc://h\n", "not grpc://"),
            ("  kosmonaut:\n    url: https://h:1\n", "not grpc://"),
            (
                "  kosmonaut:\n    url: grpc://memory.example:9432\n    tls: false\n",
                "only to this machine",
            ),
            (
                "  kosmonaut:\n    url: unix:/run/k.sock\n    tls: true\n",
                "no TLS",
            ),
            (
                "  kosmonaut:\n    url: grpc://h:1\n    token_env: \"a b\"\n",
                "environment variable",
            ),
            ("  sqlite:\n    path: relative\n", "absolute"),
            ("  sqlite:\n    mode: x\n", "mode"),
            (
                "  embedder:\n    kind: openai\n    key_env: OPENAI_API_KEY\n",
                "model is required",
            ),
            (
                "  embedder:\n    kind: openai\n    model: m\n",
                "environment variable",
            ),
            (
                "  embedder:\n    kind: openai\n    model: m\n    key_env: \"sk-a b\"\n",
                "environment variable",
            ),
            (
                "  embedder:\n    kind: openai\n    model: m\n    key_env: K\n    url: http://api.example.com/v1\n",
                "plain http",
            ),
            (
                "  embedder:\n    kind: openai\n    model: m\n    key_env: K\n    url: ftp://x/v1\n",
                "not an http",
            ),
            (
                "  embedder:\n    kind: openai\n    model: m\n    key_env: K\n    command: [x]\n",
                "not used",
            ),
            ("  embedder:\n    kind: static\n", "required"),
            (
                "  embedder:\n    kind: local\n    command: [x]\n",
                "vector length",
            ),
            (
                "  embedder:\n    kind: static\n    path: /v\n    dimension: 3\n",
                "not used",
            ),
            ("  embedder:\n    kind: magic\n", "magic"),
        ] {
            let e = parse_settings(&format!("memory:\n{bad}"), None).expect_err(bad);
            assert!(e.to_string().contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn tier_budgets_take_waits_counts_and_words() {
        let s = parse_settings(
            "tiers:\n  cold_after: 90s\n  warm_limit: 10\n  warm_memory: 512MiB\n",
            None,
        )
        .expect("ok");
        assert_eq!(s.tiers.cold_after, Some(std::time::Duration::from_secs(90)));
        assert_eq!(s.tiers.warm_limit, Some(10));
        assert_eq!(s.tiers.warm_memory, Some(512 << 20));
        let s = parse_settings(
            "tiers:\n  cold_after: never\n  warm_limit: none\n  warm_memory: none\n",
            None,
        )
        .expect("ok");
        assert_eq!(
            s.tiers,
            TierSettings {
                cold_after: None,
                warm_limit: None,
                warm_memory: None,
            }
        );
        for bad in [
            "cold_after: 30",
            "cold_after: 5d",
            "cold_after: 0m",
            "warm_limit: -1",
            "warm_limit: lots",
            "warm_memory: 512",
            "warm_memory: 512MB",
            "warm_memory: 0GiB",
            "warm_memory: lots",
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
    fn the_switch_to_frames_has_a_gap_between_its_thresholds() {
        let d = Settings::default().render;
        assert_eq!(
            (d.frames_above, d.frames_below),
            (DEFAULT_FRAMES_ABOVE, DEFAULT_FRAMES_BELOW)
        );
        let s = parse_settings(
            "render:\n  frames_above: 4MiB\n  frames_below: 1MiB\n  frames_dwell: 500ms\n  frames_hold: 3s\n",
            None,
        )
        .expect("frames policy");
        assert_eq!(
            (
                s.render.frames_above,
                s.render.frames_below,
                s.render.frames_dwell,
                s.render.frames_hold
            ),
            (
                4 << 20,
                1 << 20,
                std::time::Duration::from_millis(500),
                std::time::Duration::from_secs(3)
            )
        );
        // A pane at the threshold must not flip: below stays under above.
        for bad in [
            "frames_below: 1MiB",
            "frames_below: 2MiB",
            "frames_above: none",
            "frames_dwell: 0s",
            "frames_dwell: 1",
            "frames_hold: soon",
        ] {
            let e = parse_settings(&format!("render:\n  {bad}\n"), None).expect_err(bad);
            assert!(e.at.starts_with("render.frames_"), "{bad}: {e:?}");
        }
    }

    #[test]
    fn panes_follow_the_byte_stream_unless_frames_are_turned_on() {
        assert_eq!(Settings::default().render.frames, RenderFrames::Off);
        for want in [RenderFrames::Off, RenderFrames::On, RenderFrames::Always] {
            let text = format!("render:\n  frames: {}\n", want.name());
            let s = parse_settings(&text, None).expect("frames");
            assert_eq!(s.render.frames, want);
            assert_eq!(RenderFrames::from_name(want.name()), Some(want));
        }
        let e = parse_settings("render:\n  frames: sometimes\n", None).expect_err("bad");
        assert!(e.to_string().contains("sometimes"), "{e}");
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
