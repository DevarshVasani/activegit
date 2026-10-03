//! Phase 5: TOML config — keybinding overrides and theme selection.
//!
//! `~/.config/activegit/config.toml` (`%APPDATA%\activegit\config.toml` on
//! Windows). Missing file means defaults; anything
//! present overrides just that piece. Invalid names fail fast with the file
//! path and the offending value.

use anyhow::{Context, Result};
use crossterm::event::KeyCode;
use git_tui_core::llm::LlmConfig;
use ratatui::style::Color;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Canonical action names for `[keys]`.
pub const ACTIONS: &[&str] = &[
    "nav_down",
    "nav_up",
    "stage",
    "discard",
    "commit",
    "refresh",
    "quit",
    "focus_next",
    "focus_status",
    "focus_branches",
    "focus_log",
    "focus_stash",
    "focus_diff",
    "scroll_up",
    "scroll_down",
    "branch_new",
    "branch_delete",
    "checkout",
    "stash_pop",
    "stash_push",
    "stash_drop",
    "find_files",
    "project_next",
    "project_prev",
    "project_open",
    "project_close",
    "sync_pull",
    "sync_push",
    "llm_settings",
    "toggle_markdown_preview",
];

/// Key names accepted in `[keys]` besides single characters.
pub fn parse_key(name: &str) -> Result<KeyCode> {
    match name {
        "space" => Ok(KeyCode::Char(' ')),
        "tab" => Ok(KeyCode::Tab),
        "enter" => Ok(KeyCode::Enter),
        "esc" => Ok(KeyCode::Esc),
        "backspace" => Ok(KeyCode::Backspace),
        "delete" => Ok(KeyCode::Delete),
        "insert" => Ok(KeyCode::Insert),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "pageup" => Ok(KeyCode::PageUp),
        "pagedown" => Ok(KeyCode::PageDown),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        s if s.chars().count() == 1 => Ok(KeyCode::Char(s.chars().next().unwrap())),
        _ => anyhow::bail!("unknown key name: {name:?}"),
    }
}

/// One action's bindings (defaults preserve legacy behavior).
#[derive(Debug, Clone)]
pub struct KeyBindings {
    pub nav_down: Vec<KeyCode>,
    pub nav_up: Vec<KeyCode>,
    pub stage: Vec<KeyCode>,
    /// Discard changes in the selected file (`d` by default).
    pub discard: Vec<KeyCode>,
    pub commit: Vec<KeyCode>,
    pub refresh: Vec<KeyCode>,
    pub quit: Vec<KeyCode>,
    pub focus_next: Vec<KeyCode>,
    pub focus_status: Vec<KeyCode>,
    pub focus_branches: Vec<KeyCode>,
    pub focus_log: Vec<KeyCode>,
    pub focus_stash: Vec<KeyCode>,
    pub focus_diff: Vec<KeyCode>,
    pub scroll_up: Vec<KeyCode>,
    pub scroll_down: Vec<KeyCode>,
    pub branch_new: Vec<KeyCode>,
    pub branch_delete: Vec<KeyCode>,
    pub checkout: Vec<KeyCode>,
    pub stash_pop: Vec<KeyCode>,
    pub stash_push: Vec<KeyCode>,
    pub stash_drop: Vec<KeyCode>,
    pub find_files: Vec<KeyCode>,
    pub project_next: Vec<KeyCode>,
    pub project_prev: Vec<KeyCode>,
    pub project_open: Vec<KeyCode>,
    pub project_close: Vec<KeyCode>,
    pub sync_pull: Vec<KeyCode>,
    pub sync_push: Vec<KeyCode>,
    /// Opens the in-TUI LLM setup form (`A` by default in the file list).
    pub llm_settings: Vec<KeyCode>,
    /// Toggles rendered Markdown preview for `.md` files (`m`).
    pub toggle_markdown_preview: Vec<KeyCode>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        use KeyCode::*;
        Self {
            nav_down: vec![Char('j'), Down],
            nav_up: vec![Char('k'), Up],
            stage: vec![Char(' '), Char('s')],
            discard: vec![Char('d')],
            commit: vec![Char('c')],
            refresh: vec![Char('r')],
            quit: vec![Char('Q')],
            focus_next: vec![KeyCode::Tab],
            focus_status: vec![Char('1'), KeyCode::Left],
            focus_branches: vec![Char('2')],
            focus_log: vec![Char('3')],
            focus_stash: vec![Char('4')],
            focus_diff: vec![Char('5')],
            scroll_up: vec![KeyCode::PageUp],
            scroll_down: vec![KeyCode::PageDown],
            branch_new: vec![Char('a')],
            branch_delete: vec![Char('D')],
            checkout: vec![KeyCode::Enter],
            stash_pop: vec![KeyCode::Enter],
            stash_push: vec![Char('a')],
            stash_drop: vec![Char('D')],
            find_files: vec![Char('/')],
            project_next: vec![Char(']')],
            project_prev: vec![Char('[')],
            project_open: vec![Char('o')],
            project_close: vec![Char('q')],
            sync_pull: vec![Char('p')],
            sync_push: vec![Char('P')],
            // Shift+A in the file list (Shift+a normalizes to `A`).
            llm_settings: vec![Char('A')],
            toggle_markdown_preview: vec![Char('m')],
        }
    }
}

/// Named UI palette. `default` is Catppuccin Mocha (proper RGB colors
/// with a Telescope-style selection wash); `legacy` keeps the original
/// 16-color ANSI look.
/// Syntax colors follow LazyVim (tokyo-night + treesitter): comments are
/// dim italic gray, strings green, keywords magenta italic, functions blue,
/// types cyan, numbers orange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Theme {
    pub border_focused: Color,
    pub border_unfocused: Color,
    pub hint: Color,
    pub error: Color,
    pub staged: Color,
    pub unstaged: Color,
    pub untracked: Color,
    pub conflicted: Color,
    pub both_staged: Color,
    pub hunk_header: Color,
    pub commit_id: Color,
    pub branch_current: Color,
    #[allow(dead_code)]
    pub context: Color,
    /// Side-by-side tints: deleted lines get a red wash with a stronger
    /// background on exactly the changed words; added lines get a single
    /// very light green wash so syntax colors stay easily readable
    /// (green is never painted twice on the same cell).
    pub diff_del_bg: Color,
    pub diff_add_bg: Color,
    pub diff_del_word_bg: Color,
    // --- LazyVim-style code view ---
    /// Opaque editor background (LazyVim Normal bg). Every panel paints it
    /// so the terminal wallpaper never bleeds through the code view.
    pub bg: Color,
    /// Main editor foreground (LazyVim fg #c0caf5).
    pub fg: Color,
    /// Dim line numbers / gutter.
    pub line_nr: Color,
    /// Telescope-style selection wash.
    pub selection_bg: Color,
    /// Syntax groups.
    pub syntax_comment: Color,
    pub syntax_string: Color,
    pub syntax_keyword: Color,
    pub syntax_function: Color,
    pub syntax_type: Color,
    pub syntax_number: Color,
}

impl Theme {
    /// The out-of-the-box look: Catppuccin Mocha with proper RGB colors
    /// and a Telescope-style selection wash.
    pub fn default_theme() -> Self {
        Self::catppuccin()
    }

    /// Legacy 16-color ANSI palette for terminals without truecolor.
    pub fn legacy() -> Self {
        Self {
            border_focused: Color::White,
            border_unfocused: Color::DarkGray,
            hint: Color::DarkGray,
            error: Color::Red,
            staged: Color::Green,
            unstaged: Color::Yellow,
            untracked: Color::DarkGray,
            conflicted: Color::Red,
            both_staged: Color::Magenta,
            hunk_header: Color::Cyan,
            commit_id: Color::Yellow,
            branch_current: Color::Green,
            context: Color::DarkGray,
            diff_del_bg: Color::Rgb(55, 25, 32),
            diff_add_bg: Color::Rgb(22, 45, 30),
            diff_del_word_bg: Color::Rgb(78, 32, 42),
            // LazyVim-ish on top of legacy ANSI base.
            bg: Color::Black,
            fg: Color::White,
            line_nr: Color::DarkGray,
            selection_bg: Color::Rgb(40, 42, 60),
            syntax_comment: Color::DarkGray,
            syntax_string: Color::Green,
            syntax_keyword: Color::Magenta,
            syntax_function: Color::Blue,
            syntax_type: Color::Cyan,
            syntax_number: Color::Yellow,
        }
    }

    /// https://github.com/tokyo-night/tokyo-night.nvim — Storm palette.
    pub fn tokyo_night() -> Self {
        let rgb = Color::Rgb;
        Self {
            border_focused: rgb(122, 162, 247), // blue #7aa2f7
            border_unfocused: rgb(59, 66, 97),  // #3b4261
            hint: rgb(86, 95, 137),             // comment #565f89
            error: rgb(247, 118, 142),          // red #f7768e
            staged: rgb(158, 206, 106),         // green #9ece6a
            unstaged: rgb(224, 175, 104),       // yellow #e0af68
            untracked: rgb(86, 95, 137),        // comment #565f89
            conflicted: rgb(247, 118, 142),     // red #f7768e
            both_staged: rgb(187, 154, 247),    // magenta #bb9af7
            hunk_header: rgb(125, 207, 255),    // cyan #7dcfff
            commit_id: rgb(224, 175, 104),      // yellow #e0af68
            branch_current: rgb(158, 206, 106), // green #9ece6a
            context: rgb(86, 95, 137),          // comment #565f89
            diff_del_bg: rgb(60, 32, 42),
            diff_add_bg: rgb(28, 48, 38),
            diff_del_word_bg: rgb(82, 38, 50),
            // LazyVim editor colors (tokyo-night Storm).
            bg: rgb(36, 40, 59),                 // #24283b Storm bg
            fg: rgb(192, 202, 245),              // #c0caf5
            line_nr: rgb(59, 66, 97),            // #3b4261 dim gutter
            selection_bg: rgb(40, 52, 94),       // #28365e Telescope selection
            syntax_comment: rgb(86, 95, 137),    // #565f89 italic
            syntax_string: rgb(158, 206, 106),   // #9ece6a
            syntax_keyword: rgb(187, 154, 247),  // #bb9af7 italic
            syntax_function: rgb(122, 162, 247), // #7aa2f7
            syntax_type: rgb(125, 207, 255),     // #7dcfff
            syntax_number: rgb(255, 158, 100),   // #ff9e64 orange
        }
    }

    /// https://catppuccin.com — Mocha palette.
    pub fn catppuccin() -> Self {
        let rgb = Color::Rgb;
        Self {
            border_focused: rgb(137, 180, 250), // blue #89b4fa
            border_unfocused: rgb(69, 71, 90),  // surface1 #45475a
            hint: rgb(108, 112, 134),           // overlay0 #6c7086
            error: rgb(243, 139, 168),          // red #f38ba8
            staged: rgb(166, 227, 161),         // green #a6e3a1
            unstaged: rgb(249, 226, 175),       // yellow #f9e2af
            untracked: rgb(108, 112, 134),      // overlay0 #6c7086
            conflicted: rgb(243, 139, 168),     // red #f38ba8
            both_staged: rgb(203, 166, 247),    // mauve #cba6f7
            hunk_header: rgb(137, 220, 235),    // sky #89dceb
            commit_id: rgb(250, 179, 135),      // peach #fab387
            branch_current: rgb(166, 227, 161), // green #a6e3a1
            context: rgb(108, 112, 134),        // overlay0 #6c7086
            diff_del_bg: rgb(58, 36, 48),
            diff_add_bg: rgb(30, 49, 42),
            diff_del_word_bg: rgb(79, 42, 56),
            bg: rgb(30, 30, 46),                 // base #1e1e2e
            fg: rgb(205, 214, 244),              // text #cdd6f4
            line_nr: rgb(69, 71, 90),            // surface1 #45475a
            selection_bg: rgb(49, 50, 68),       // surface0 #313244
            syntax_comment: rgb(108, 112, 134),  // overlay0 #6c7086
            syntax_string: rgb(166, 227, 161),   // green #a6e3a1
            syntax_keyword: rgb(203, 166, 247),  // mauve #cba6f7
            syntax_function: rgb(137, 180, 250), // blue #89b4fa
            syntax_type: rgb(137, 220, 235),     // sky #89dceb
            syntax_number: rgb(250, 179, 135),   // peach #fab387
        }
    }

    pub fn by_name(name: &str) -> Result<Self> {
        match name {
            "default" => Ok(Self::default_theme()),
            "tokyo-night" => Ok(Self::tokyo_night()),
            "catppuccin" => Ok(Self::catppuccin()),
            "legacy" => Ok(Self::legacy()),
            _ => anyhow::bail!(
                "unknown theme {name:?} (expected \"default\", \"tokyo-night\", \"catppuccin\", or \"legacy\")"
            ),
        }
    }
}

/// Resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub keys: KeyBindings,
    pub theme: Theme,
    /// LLM provider for Shift+A commit generation in the commit box.
    /// `~/.config/activegit/config.toml` `[llm]` section; empty key falls
    /// back to `$OPENAI_API_KEY` / `$ANTHROPIC_API_KEY` / `$GEMINI_API_KEY`
    /// / `$OPENROUTER_API_KEY` / `$LLM_API_KEY` at generation time.
    pub llm: LlmConfig,
}

/// App directory name under the XDG config home.
pub const APP_DIR: &str = "activegit";

/// `$XDG_CONFIG_HOME/activegit` (or `~/.config/activegit`; on Windows
/// `%APPDATA%\activegit`); holds `config.toml` and `session.toml`. `None`
/// when no base directory can be found.
pub fn config_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => default_config_base()?,
    };
    Some(pick_app_dir(&base))
}

#[cfg(windows)]
fn default_config_base() -> Option<PathBuf> {
    match std::env::var_os("APPDATA") {
        Some(appdata) if !appdata.is_empty() => Some(PathBuf::from(appdata)),
        _ => git_tui_core::repo::home_dir().map(|h| h.join("AppData").join("Roaming")),
    }
}

#[cfg(not(windows))]
fn default_config_base() -> Option<PathBuf> {
    git_tui_core::repo::home_dir().map(|h| h.join(".config"))
}

/// `base/activegit`.
fn pick_app_dir(base: &Path) -> PathBuf {
    base.join(APP_DIR)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keys: KeyBindings::default(),
            theme: Theme::default_theme(),
            llm: LlmConfig::default(),
        }
    }
}

impl Config {
    /// Load from an explicit path. Missing file means defaults.
    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("bad config {}", path.display()))
    }

    /// Load from `$XDG_CONFIG_HOME/activegit/config.toml`, falling back to
    /// `~/.config/activegit/config.toml` (`%APPDATA%\activegit` on Windows).
    /// Missing file means defaults.
    pub fn load() -> Result<Self> {
        match Self::default_path() {
            Some(path) => Self::load_from_path(&path),
            None => Ok(Self::default()),
        }
    }

    pub fn default_path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("config.toml"))
    }

    /// Persist just the `[llm]` section to `path`, preserving every other
    /// section already in the file (keys, theme). Creates parent dirs.
    /// Used by the in-TUI setup form (`A` in the file list).
    pub fn save_llm_to_path(path: &Path, llm: &LlmConfig) -> Result<()> {
        let mut doc: toml::Table = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read config {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("bad config {}", path.display()))?
        } else {
            toml::Table::new()
        };
        let mut table = toml::Table::new();
        table.insert(
            "provider".to_string(),
            toml::Value::String(llm.provider.clone()),
        );
        table.insert("model".to_string(), toml::Value::String(llm.model.clone()));
        table.insert(
            "api_key".to_string(),
            toml::Value::String(llm.api_key.clone()),
        );
        if let Some(url) = llm.base_url.as_deref().filter(|u| !u.trim().is_empty()) {
            table.insert("base_url".to_string(), toml::Value::String(url.to_string()));
        }
        doc.insert("llm".to_string(), toml::Value::Table(table));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(&doc).context("cannot serialize config")?;
        std::fs::write(path, text)
            .with_context(|| format!("cannot write config {}", path.display()))?;
        Ok(())
    }

    fn from_toml(text: &str) -> Result<Self> {
        #[derive(Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FileConfig {
            #[serde(default)]
            keys: HashMap<String, OneOrMany>,
            #[serde(default)]
            theme: ThemeSection,
            #[serde(default)]
            llm: LlmSection,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }

        #[derive(Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ThemeSection {
            #[serde(default)]
            name: Option<String>,
        }

        #[derive(Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LlmSection {
            #[serde(default)]
            provider: Option<String>,
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            api_key: Option<String>,
            #[serde(default)]
            base_url: Option<String>,
        }

        let file: FileConfig = toml::from_str(text).context("cannot parse TOML")?;
        let mut cfg = Self::default();
        for (action, value) in &file.keys {
            let names: &[String] = match value {
                OneOrMany::One(s) => std::slice::from_ref(s),
                OneOrMany::Many(v) => v,
            };
            let mut keys = Vec::with_capacity(names.len());
            for name in names {
                keys.push(parse_key(name).with_context(|| format!("bad key for [{action}]"))?);
            }
            if keys.is_empty() {
                anyhow::bail!("no keys listed for [{action}]");
            }
            let k = &mut cfg.keys;
            match action.as_str() {
                "nav_down" => k.nav_down = keys,
                "nav_up" => k.nav_up = keys,
                "stage" => k.stage = keys,
                "discard" => k.discard = keys,
                "commit" => k.commit = keys,
                "refresh" => k.refresh = keys,
                "quit" => k.quit = keys,
                "focus_next" => k.focus_next = keys,
                "focus_status" => k.focus_status = keys,
                "focus_branches" => k.focus_branches = keys,
                "focus_log" => k.focus_log = keys,
                "focus_stash" => k.focus_stash = keys,
                "focus_diff" => k.focus_diff = keys,
                "scroll_up" => k.scroll_up = keys,
                "scroll_down" => k.scroll_down = keys,
                "branch_new" => k.branch_new = keys,
                "branch_delete" => k.branch_delete = keys,
                "checkout" => k.checkout = keys,
                "stash_pop" => k.stash_pop = keys,
                "stash_push" => k.stash_push = keys,
                "stash_drop" => k.stash_drop = keys,
                "find_files" => k.find_files = keys,
                "project_next" => k.project_next = keys,
                "project_prev" => k.project_prev = keys,
                "project_open" => k.project_open = keys,
                "project_close" => k.project_close = keys,
                "sync_pull" => k.sync_pull = keys,
                "sync_push" => k.sync_push = keys,
                "llm_settings" => k.llm_settings = keys,
                "toggle_markdown_preview" => k.toggle_markdown_preview = keys,
                _ => anyhow::bail!(
                    "unknown action [{action}] (expected one of: {})",
                    ACTIONS.join(", ")
                ),
            }
        }
        if let Some(name) = file.theme.name {
            cfg.theme = Theme::by_name(&name)?;
        }
        if let Some(provider) = file.llm.provider {
            let p = provider.trim().to_lowercase();
            if git_tui_core::llm::PROVIDERS.contains(&p.as_str()) {
                cfg.llm.provider = p;
            } else {
                anyhow::bail!(
                    "unknown llm provider {provider:?} (expected one of: {})",
                    git_tui_core::llm::PROVIDERS.join(", ")
                );
            }
        }
        if let Some(model) = file.llm.model {
            if model.trim().is_empty() {
                anyhow::bail!("llm model must not be empty");
            }
            cfg.llm.model = model.trim().to_string();
        }
        if let Some(key) = file.llm.api_key {
            cfg.llm.api_key = key.trim().to_string();
        }
        if let Some(url) = file.llm.base_url {
            if url.trim().is_empty() {
                anyhow::bail!("llm base_url must not be empty");
            }
            cfg.llm.base_url = Some(url.trim().to_string());
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_preserve_legacy_behavior() {
        let keys = KeyBindings::default();
        assert!(keys.nav_down.contains(&KeyCode::Char('j')));
        assert!(keys.nav_down.contains(&KeyCode::Down));
        assert!(keys.stage.contains(&KeyCode::Char(' ')));
        assert!(keys.stage.contains(&KeyCode::Char('s')));
        assert!(keys.discard.contains(&KeyCode::Char('d')));
        // `q` closes the current project, `Q` (Shift+q) quits the app.
        assert!(keys.project_close.contains(&KeyCode::Char('q')));
        assert!(keys.quit.contains(&KeyCode::Char('Q')));
        assert!(keys.focus_next.contains(&KeyCode::Tab));
        assert!(keys.checkout.contains(&KeyCode::Enter));
        assert!(keys.branch_delete.contains(&KeyCode::Char('D')));
        assert!(keys.find_files.contains(&KeyCode::Char('/')));
    }

    #[test]
    fn sync_keys_default_to_lazygit_pull_push() {
        let keys = KeyBindings::default();
        assert!(keys.sync_pull.contains(&KeyCode::Char('p')));
        assert!(keys.sync_push.contains(&KeyCode::Char('P')));
    }

    #[test]
    fn left_arrow_focuses_status_by_default() {
        let keys = KeyBindings::default();
        assert!(keys.focus_status.contains(&KeyCode::Left));
        // Vertical-only navigation: nothing is bound to Right by default.
        for action in [
            &keys.nav_down,
            &keys.nav_up,
            &keys.stage,
            &keys.focus_branches,
            &keys.focus_log,
            &keys.focus_stash,
        ] {
            assert!(!action.contains(&KeyCode::Right));
        }
    }

    #[test]
    fn key_names_parse() {
        assert_eq!(parse_key("space").unwrap(), KeyCode::Char(' '));
        assert_eq!(parse_key("tab").unwrap(), KeyCode::Tab);
        assert_eq!(parse_key("enter").unwrap(), KeyCode::Enter);
        assert_eq!(parse_key("esc").unwrap(), KeyCode::Esc);
        assert_eq!(parse_key("pageup").unwrap(), KeyCode::PageUp);
        assert_eq!(parse_key("D").unwrap(), KeyCode::Char('D'));
    }

    #[test]
    fn unknown_key_name_errors() {
        assert!(parse_key("ctrl-x").is_err());
        assert!(parse_key("").is_err());
    }

    #[test]
    fn themes_resolve() {
        assert!(Theme::by_name("default").is_ok());
        let tn = Theme::by_name("tokyo-night").unwrap();
        assert_eq!(tn.error, Color::Rgb(247, 118, 142));
        assert_eq!(tn.hunk_header, Color::Rgb(125, 207, 255));
        // Default ships the Catppuccin Mocha palette; legacy keeps bare ANSI.
        assert_eq!(Theme::default_theme().bg, Color::Rgb(30, 30, 46));
        assert_eq!(Theme::default_theme().error, Color::Rgb(243, 139, 168));
        assert_eq!(Theme::by_name("legacy").unwrap().staged, Color::Green);
        let mocha = Theme::by_name("catppuccin").unwrap();
        assert_eq!(mocha, Theme::default_theme());
        assert!(Theme::by_name("dracula").is_err());
    }

    #[test]
    fn editor_background_is_opaque() {
        // LazyVim Normal bg: solid so the wallpaper never bleeds through.
        assert_eq!(
            Theme::by_name("tokyo-night").unwrap().bg,
            Color::Rgb(36, 40, 59)
        );
        assert_eq!(Theme::default_theme().bg, Color::Rgb(30, 30, 46));
        assert_eq!(Theme::by_name("legacy").unwrap().bg, Color::Black);
    }

    #[test]
    fn diff_washes_are_dark_enough_for_syntax_colors() {
        // Diffs must stay readable with syntax highlighting: deep red/green
        // washes keep the colored foreground readable (contrast ratio >= 3).
        // Green/teal washes stay dark; red washes stay deep crimson so any
        // syntax fg remains readable on top.
        for theme in [
            Theme::by_name("tokyo-night").unwrap(),
            Theme::by_name("catppuccin").unwrap(),
            Theme::default_theme(),
            Theme::legacy(),
        ] {
            let Color::Rgb(ar, ag, ab) = theme.diff_add_bg else {
                panic!("expected RGB add wash, got {:?}", theme.diff_add_bg);
            };
            assert!(ag > ar, "add wash must stay green-tinted: {ar},{ag},{ab}");
            for wash in [theme.diff_del_bg, theme.diff_del_word_bg] {
                let Color::Rgb(r, g, b) = wash else {
                    panic!("expected RGB wash, got {wash:?}");
                };
                assert!(r > g, "del wash must stay red-tinted: {wash:?}");
                assert!((r + g + b) / 3 < 90, "wash too light: {wash:?}");
            }
        }
    }

    #[test]
    fn legacy_theme_matches_old_ansi_colors() {
        let t = Theme::legacy();
        assert_eq!(t.staged, Color::Green);
        assert_eq!(t.unstaged, Color::Yellow);
        assert_eq!(t.conflicted, Color::Red);
        assert_eq!(t.hunk_header, Color::Cyan);
    }

    // RED: file loading not implemented yet.
    #[test]
    fn missing_file_means_defaults() {
        let cfg = Config::load_from_path(Path::new("/nonexistent/config.toml")).unwrap();
        assert!(cfg.keys.quit.contains(&KeyCode::Char('Q')));
        assert!(cfg.keys.project_close.contains(&KeyCode::Char('q')));
    }

    // RED: file loading not implemented yet.
    #[test]
    fn file_overrides_keys_and_theme() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[keys]\nstage = \"s\"\nquit = [\"q\", \"Q\"]\n\n[theme]\nname = \"tokyo-night\"\n",
        )
        .unwrap();
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.keys.stage, vec![KeyCode::Char('s')]);
        assert!(cfg.keys.quit.contains(&KeyCode::Char('Q')));
        assert_eq!(cfg.theme.error, Color::Rgb(247, 118, 142));
        // Untouched actions keep defaults.
        assert!(cfg.keys.commit.contains(&KeyCode::Char('c')));
    }

    // RED: file loading not implemented yet.
    #[test]
    fn unknown_action_or_key_in_file_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let bad_action = dir.path().join("a.toml");
        std::fs::write(&bad_action, "[keys]\nfly = \"f\"\n").unwrap();
        assert!(Config::load_from_path(&bad_action).is_err());
        let bad_key = dir.path().join("b.toml");
        std::fs::write(&bad_key, "[keys]\nquit = \"ctrl-q\"\n").unwrap();
        assert!(Config::load_from_path(&bad_key).is_err());
    }

    #[test]
    fn misspelled_section_or_theme_field_errors_instead_of_silently_ignored() {
        // `[themes]` (plural) must not silently fall back to defaults.
        let dir = tempfile::TempDir::new().unwrap();
        let plural = dir.path().join("c.toml");
        std::fs::write(&plural, "[themes]\nname = \"tokyo-night\"\n").unwrap();
        assert!(Config::load_from_path(&plural).is_err());
        // `[theme] style = ...` (wrong field) must not be ignored either.
        let field = dir.path().join("d.toml");
        std::fs::write(&field, "[theme]\nstyle = \"tokyo-night\"\n").unwrap();
        assert!(Config::load_from_path(&field).is_err());
    }

    #[test]
    fn llm_defaults_to_openai_without_key() {
        let cfg = Config::default();
        assert_eq!(cfg.llm.provider, "openai");
        assert_eq!(cfg.llm.model, "gpt-4o-mini");
        assert!(cfg.llm.api_key.is_empty());
    }

    #[test]
    fn llm_section_overrides_provider_model_and_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("llm.toml");
        std::fs::write(
            &path,
            "[llm]\nprovider = \"anthropic\"\nmodel = \"claude-3-5-haiku-latest\"\napi_key = \"sk-test\"\n",
        )
        .unwrap();
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.llm.provider, "anthropic");
        assert_eq!(cfg.llm.model, "claude-3-5-haiku-latest");
        assert_eq!(cfg.llm.api_key, "sk-test");
        // Keys/theme untouched.
        assert!(cfg.keys.commit.contains(&KeyCode::Char('c')));
    }

    #[test]
    fn llm_settings_key_defaults_to_shift_a() {
        assert!(KeyBindings::default()
            .llm_settings
            .contains(&KeyCode::Char('A')));
    }

    #[test]
    fn llm_custom_base_url_and_unknown_provider() {
        let dir = tempfile::TempDir::new().unwrap();
        let ok = dir.path().join("ok.toml");
        std::fs::write(
            &ok,
            "[llm]\nprovider = \"custom\"\nbase_url = \"http://localhost:8080/v1\"\n",
        )
        .unwrap();
        let cfg = Config::load_from_path(&ok).unwrap();
        assert_eq!(cfg.llm.provider, "custom");
        assert_eq!(
            cfg.llm.base_url.as_deref(),
            Some("http://localhost:8080/v1")
        );
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[llm]\nprovider = \"skynet\"\n").unwrap();
        assert!(Config::load_from_path(&bad).is_err());
        let bad_field = dir.path().join("bad2.toml");
        std::fs::write(&bad_field, "[llm]\napi_keys = \"x\"\n").unwrap();
        assert!(Config::load_from_path(&bad_field).is_err());
    }

    #[test]
    fn app_dir_is_activegit() {
        let base = tempfile::TempDir::new().unwrap();
        assert_eq!(pick_app_dir(base.path()), base.path().join("activegit"));
    }

    #[test]
    fn save_llm_preserves_other_sections_and_roundtrips() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("activegit").join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[keys]\nquit = \"Q\"\n\n[theme]\nname = \"tokyo-night\"\n",
        )
        .unwrap();
        let llm = LlmConfig {
            provider: "ollama".into(),
            model: "llama3.1".into(),
            api_key: String::new(),
            base_url: None,
        };
        Config::save_llm_to_path(&path, &llm).unwrap();
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.llm.provider, "ollama");
        assert_eq!(cfg.llm.model, "llama3.1");
        // Untouched sections survive the merge.
        assert!(cfg.keys.quit.contains(&KeyCode::Char('Q')));
        assert_eq!(
            cfg.theme.border_focused,
            ratatui::style::Color::Rgb(122, 162, 247)
        );
    }
}
