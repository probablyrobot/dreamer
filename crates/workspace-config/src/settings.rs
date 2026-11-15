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
        use terminal_core::schemes::{DREAMER_DARK, DREAMER_LIGHT};
        if let Some(s) = self.terminal.theme {
            return s;
        }
        let light = match self.appearance {
            Appearance::Dark => false,
            Appearance::Light => true,
            Appearance::System => os_prefers_light(),
        };
        if light { &DREAMER_LIGHT } else { &DREAMER_DARK }
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClipboardSettings {
    pub write: ClipboardWrite,
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
    if let Some(a) = file.appearance {
        s.appearance = a;
    }
    if let Some(name) = file.terminal.theme {
        s.terminal.theme = Some(terminal_core::schemes::find(&name).ok_or_else(|| {
            let names: Vec<&str> = terminal_core::schemes::all().map(|s| s.name).collect();
            error(
                origin,
                "terminal.theme",
                format!(
                    "no built-in theme `{name}`; choose one of: {}",
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
    fn appearance_follows_the_system_unless_set() {
        assert_eq!(Settings::default().appearance, Appearance::System);
        let s = parse_settings("appearance: light", None).expect("light");
        assert_eq!(s.appearance, Appearance::Light);
        assert!(parse_settings("appearance: sepia", None).is_err());
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
