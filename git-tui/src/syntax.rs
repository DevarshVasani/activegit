//! LazyVim-style syntax highlighting for the file viewer.
//!
//! LazyVim = tokyo-night colors + treesitter grammars. Here we approximate
//! that with `syntect` Sublime grammars and a hand-built tmTheme derived
//! from our [`crate::config::Theme`], so `default` and `tokyo-night` both
//! get readable code colors instead of a flat gray wash.
//!
//! Design notes:
//! - `SyntaxSet` is global (large, ~MBs) via `OnceLock`.
//! - The syntect `Theme` is built per call from the app `Theme` (cheap).
//! - `highlight_line` highlights one line in isolation (fresh state). This
//!   covers keywords/strings/comments/functions for ~95% of lines. Multi-line
//!   block comments spanning lines lose state — accepted v1 tradeoff to keep
//!   per-frame rendering cheap (only visible lines are highlighted).
//! - `highlight_file_lines` reuses one `HighlightLines` across lines so a
//!   whole-file view keeps block-comment/string state correctly.
//! - Markdown gets LazyVim-style styling too: bold blue headings, bold/italic
//!   emphasis, green inline code and fences, underlined cyan links, dimmed
//!   `#`/`*`/`-`/`>`/`[]()` punctuation. The rules are prefixed with
//!   `text.html.markdown` so code highlighting is never affected.
//! - TOML has no grammar in syntect's default set (`.toml` fell back to
//!   flat plain text), so `Cargo.toml` / `config.toml` / `Cargo.lock` get a
//!   small hand-rolled highlighter: cyan tables, blue keys, green strings,
//!   orange numbers/dates, magenta booleans, dimmed punctuation, italic
//!   comments.

use crate::config::Theme;
use ratatui::style::{Color as RatColor, Modifier};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, Theme as SynTheme, ThemeSettings,
};
use syntect::parsing::SyntaxSet;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_nonewlines)
}

fn rat_to_syn(c: RatColor) -> SynColor {
    match c {
        RatColor::Rgb(r, g, b) => SynColor { r, g, b, a: 255 },
        RatColor::White => SynColor {
            r: 192,
            g: 202,
            b: 245,
            a: 255,
        },
        RatColor::Black => SynColor {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        },
        RatColor::Red => SynColor {
            r: 247,
            g: 118,
            b: 142,
            a: 255,
        },
        RatColor::Green => SynColor {
            r: 158,
            g: 206,
            b: 106,
            a: 255,
        },
        RatColor::Yellow => SynColor {
            r: 224,
            g: 175,
            b: 104,
            a: 255,
        },
        RatColor::Blue => SynColor {
            r: 122,
            g: 162,
            b: 247,
            a: 255,
        },
        RatColor::Magenta => SynColor {
            r: 187,
            g: 154,
            b: 247,
            a: 255,
        },
        RatColor::Cyan => SynColor {
            r: 125,
            g: 207,
            b: 255,
            a: 255,
        },
        RatColor::Gray => SynColor {
            r: 169,
            g: 177,
            b: 214,
            a: 255,
        },
        RatColor::DarkGray => SynColor {
            r: 86,
            g: 95,
            b: 137,
            a: 255,
        },
        RatColor::LightRed => SynColor {
            r: 247,
            g: 118,
            b: 142,
            a: 255,
        },
        RatColor::LightGreen => SynColor {
            r: 158,
            g: 206,
            b: 106,
            a: 255,
        },
        RatColor::LightYellow => SynColor {
            r: 224,
            g: 175,
            b: 104,
            a: 255,
        },
        RatColor::LightBlue => SynColor {
            r: 122,
            g: 162,
            b: 247,
            a: 255,
        },
        RatColor::LightMagenta => SynColor {
            r: 187,
            g: 154,
            b: 247,
            a: 255,
        },
        RatColor::LightCyan => SynColor {
            r: 125,
            g: 207,
            b: 255,
            a: 255,
        },
        RatColor::Indexed(_) => SynColor {
            r: 192,
            g: 202,
            b: 245,
            a: 255,
        },
        RatColor::Reset => SynColor {
            r: 192,
            g: 202,
            b: 245,
            a: 255,
        },
    }
}

fn syn_to_rat(c: SynColor) -> RatColor {
    RatColor::Rgb(c.r, c.g, c.b)
}

fn rule(
    selector: &str,
    fg: RatColor,
    font_style: Option<FontStyle>,
) -> syntect::highlighting::ThemeItem {
    syntect::highlighting::ThemeItem {
        scope: ScopeSelectors::from_str(selector).unwrap_or_default(),
        style: StyleModifier {
            foreground: Some(rat_to_syn(fg)),
            background: None,
            font_style,
        },
    }
}

/// Built syntect themes, one per app [`Theme`]. Building parses a dozen
/// scope selectors, so doing it per line per frame (~1ms) was the main
/// source of UI lag — now it happens once per theme.
fn syntect_theme_cached(theme: Theme) -> std::sync::Arc<SynTheme> {
    static CACHE: OnceLock<Mutex<HashMap<Theme, std::sync::Arc<SynTheme>>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("syntect theme cache poisoned");
    cache
        .entry(theme)
        .or_insert_with(|| std::sync::Arc::new(syntect_theme(theme)))
        .clone()
}

/// Highlighted-line cache: the TUI redraws up to 10x/sec and the same
/// lines are visible across frames, so repeat renders become HashMap hits
/// instead of regex highlighting. Pure function — no invalidation needed.
/// Bounded: cleared once it grows past the cap.
type LineCache = Mutex<HashMap<(String, String, Theme), Vec<HiToken>>>;

fn line_cache() -> &'static LineCache {
    static CACHE: OnceLock<LineCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const LINE_CACHE_CAP: usize = 4096;

/// Lines past this length skip syntect (plain foreground): regex
/// highlighting on multi-KB minified lines costs tens of ms per frame while
/// scrolling, and flat text stays fully readable.
const HIGHLIGHT_LEN_CAP: usize = 2000;

/// Build a syntect theme from the app theme: LazyVim groups.
fn syntect_theme(theme: Theme) -> SynTheme {
    let fg = rat_to_syn(theme.fg);
    SynTheme {
        name: Some("activegit-lazyvim".into()),
        author: Some("activegit".into()),
        settings: ThemeSettings {
            foreground: Some(fg),
            background: None,
            gutter: None,
            gutter_foreground: None,
            line_highlight: None,
            ..Default::default()
        },
        scopes: vec![
            rule("comment", theme.syntax_comment, Some(FontStyle::ITALIC)),
            rule("comment.line", theme.syntax_comment, Some(FontStyle::ITALIC)),
            rule("comment.block", theme.syntax_comment, Some(FontStyle::ITALIC)),
            rule("string", theme.syntax_string, None),
            rule("string.quoted", theme.syntax_string, None),
            rule(
                "constant.numeric, constant.language, constant.character",
                theme.syntax_number,
                None,
            ),
            rule(
                "keyword, keyword.control, keyword.operator, storage, storage.type, storage.modifier",
                theme.syntax_keyword,
                Some(FontStyle::ITALIC),
            ),
            rule(
                "entity.name.function, support.function, variable.function, entity.name.method",
                theme.syntax_function,
                None,
            ),
            rule(
                "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, support.type, support.class",
                theme.syntax_type,
                None,
            ),
            // --- Markdown (text.html.markdown) ---
            // Prefixed with the root scope so these never recolor code:
            // headings bold blue, emphasis keeps fg + modifier, inline code
            // and fences green, link text/URLs underlined cyan, and the
            // `#`/`*`/`-`/`>`/`[]()` punctuation dimmed.
            rule(
                "text.html.markdown markup.heading",
                theme.syntax_function,
                Some(FontStyle::BOLD),
            ),
            rule(
                "text.html.markdown markup.bold",
                theme.fg,
                Some(FontStyle::BOLD),
            ),
            rule(
                "text.html.markdown markup.italic",
                theme.fg,
                Some(FontStyle::ITALIC),
            ),
            rule("text.html.markdown markup.raw", theme.syntax_string, None),
            rule(
                "text.html.markdown markup.underline.link, text.html.markdown meta.link.inline.description, text.html.markdown constant.other.reference.link",
                theme.syntax_type,
                Some(FontStyle::UNDERLINE),
            ),
            rule(
                "text.html.markdown punctuation.definition.heading, text.html.markdown punctuation.definition.bold, text.html.markdown punctuation.definition.italic, text.html.markdown punctuation.definition.raw, text.html.markdown punctuation.definition.link, text.html.markdown punctuation.definition.metadata, text.html.markdown punctuation.definition.blockquote, text.html.markdown punctuation.definition.list_item, text.html.markdown constant.other.language-name",
                theme.hint,
                None,
            ),
        ],
    }
}

fn find_syntax<'a>(ss: &'a SyntaxSet, path: &str) -> &'a syntect::parsing::SyntaxReference {
    if let Ok(Some(s)) = ss.find_syntax_for_file(path) {
        return s;
    }
    if let Some(ext) = path.rsplit('.').next() {
        // syntect's default set ships no TypeScript/JSX grammar; fall back
        // to JavaScript, which covers keywords/strings/comments for the
        // whole TS/JS family (.ts/.tsx/.mts/.cts/.jsx/.mjs/.cjs).
        let mapped: &str = match ext.to_ascii_lowercase().as_str() {
            "ts" | "mts" | "cts" | "tsx" | "jsx" | "mjs" | "cjs" => "js",
            // Markdown variants with no dedicated grammar render as Markdown.
            "mdx" | "mkd" => "md",
            _ => ext,
        };
        if let Some(s) = ss.find_syntax_by_extension(mapped) {
            return s;
        }
    }
    ss.find_syntax_plain_text()
}

/// syntect's default set ships no TOML grammar, so `.toml` files (and
/// `Cargo.lock`, which is TOML without the extension) fell back to flat
/// plain text. Detect them here and route to the hand-rolled highlighter.
fn is_toml_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".toml") {
        return true;
    }
    let base = lower.rsplit('/').next().unwrap_or(lower.as_str());
    base == "cargo.lock"
}

/// One highlighted token: text + fg + modifier (italic/bold from tmTheme).
#[derive(Debug, Clone)]
pub struct HiToken {
    pub text: String,
    pub fg: RatColor,
    pub modifier: Modifier,
}

fn convert_ranges(ranges: Vec<(syntect::highlighting::Style, &str)>) -> Vec<HiToken> {
    ranges
        .into_iter()
        .map(|(st, text)| {
            let mut modifier = Modifier::empty();
            if st.font_style.contains(FontStyle::ITALIC) {
                modifier |= Modifier::ITALIC;
            }
            if st.font_style.contains(FontStyle::BOLD) {
                modifier |= Modifier::BOLD;
            }
            if st.font_style.contains(FontStyle::UNDERLINE) {
                modifier |= Modifier::UNDERLINED;
            }
            HiToken {
                text: text.to_string(),
                fg: syn_to_rat(st.foreground),
                modifier,
            }
        })
        .filter(|t| !t.text.is_empty())
        .collect()
}

/// Highlight a single line in isolation (fresh parser state).
pub fn highlight_line(path: &str, text: &str, theme: Theme) -> Vec<HiToken> {
    if text.is_empty() {
        return Vec::new();
    }
    // Very long lines render in the plain foreground: full syntect on
    // multi-KB lines costs tens of ms, and flat text stays readable.
    if text.len() > HIGHLIGHT_LEN_CAP {
        return vec![HiToken {
            text: text.to_string(),
            fg: theme.fg,
            modifier: Modifier::empty(),
        }];
    }
    // Repeat frames show the same lines: serve them from the cache.
    let key = (path.to_string(), text.to_string(), theme);
    if let Some(hit) = line_cache()
        .lock()
        .expect("highlight cache poisoned")
        .get(&key)
    {
        return hit.clone();
    }
    let out = highlight_line_uncached(path, text, theme);
    let mut cache = line_cache().lock().expect("highlight cache poisoned");
    if cache.len() > LINE_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, out.clone());
    out
}

/// Uncached single-line highlight (fresh parser state).
fn highlight_line_uncached(path: &str, text: &str, theme: Theme) -> Vec<HiToken> {
    if is_toml_path(path) {
        return highlight_toml_line(text, theme);
    }
    let ss = syntaxes();
    let syn = syntect_theme_cached(theme);
    let syntax = find_syntax(ss, path);
    let mut hl = HighlightLines::new(syntax, &syn);
    match hl.highlight_line(text, ss) {
        Ok(ranges) => {
            let out = convert_ranges(ranges);
            if out.is_empty() {
                vec![HiToken {
                    text: text.to_string(),
                    fg: theme.fg,
                    modifier: Modifier::empty(),
                }]
            } else {
                out
            }
        }
        Err(_) => vec![HiToken {
            text: text.to_string(),
            fg: theme.fg,
            modifier: Modifier::empty(),
        }],
    }
}

#[allow(dead_code)]
/// Highlight whole-file lines with shared parser state (correct multi-line
/// comments/strings). Returns one token vec per input line.
pub fn highlight_file_lines(path: &str, lines: &[&str], theme: Theme) -> Vec<Vec<HiToken>> {
    if is_toml_path(path) {
        return lines
            .iter()
            .map(|text| {
                if text.is_empty() {
                    Vec::new()
                } else {
                    highlight_toml_line(text, theme)
                }
            })
            .collect();
    }
    let ss = syntaxes();
    let syn = syntect_theme_cached(theme);
    let syntax = find_syntax(ss, path);
    let mut hl = HighlightLines::new(syntax, &syn);
    lines
        .iter()
        .map(|text| match hl.highlight_line(text, ss) {
            Ok(ranges) => {
                let out = convert_ranges(ranges);
                if out.is_empty() && !text.is_empty() {
                    vec![HiToken {
                        text: (*text).to_string(),
                        fg: theme.fg,
                        modifier: Modifier::empty(),
                    }]
                } else {
                    out
                }
            }
            Err(_) => vec![HiToken {
                text: (*text).to_string(),
                fg: theme.fg,
                modifier: Modifier::empty(),
            }],
        })
        .collect()
}

/// Push a TOML token, merging with the previous one when the style matches
/// so one line stays a handful of spans instead of per-char fragments.
fn push_toml(out: &mut Vec<HiToken>, text: &str, fg: RatColor, modifier: Modifier) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.fg == fg && last.modifier == modifier {
            last.text.push_str(text);
            return;
        }
    }
    out.push(HiToken {
        text: text.to_string(),
        fg,
        modifier,
    });
}

fn toml_char_len(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
}

/// Byte offset of the `#` starting a comment (outside any string), if any.
fn find_toml_comment_start(line: &str) -> Option<usize> {
    #[derive(PartialEq)]
    enum S {
        Basic,
        Literal,
        MlBasic,
        MlLiteral,
    }
    let mut state: Option<S> = None;
    let mut i = 0;
    while i < line.len() {
        match state {
            None => {
                if line[i..].starts_with("\"\"\"") {
                    state = Some(S::MlBasic);
                    i += 3;
                } else if line[i..].starts_with("'''") {
                    state = Some(S::MlLiteral);
                    i += 3;
                } else {
                    let c = line.as_bytes()[i] as char;
                    match c {
                        '"' => {
                            state = Some(S::Basic);
                            i += 1;
                        }
                        '\'' => {
                            state = Some(S::Literal);
                            i += 1;
                        }
                        '#' => return Some(i),
                        _ => i += toml_char_len(line, i),
                    }
                }
            }
            Some(S::Basic) => {
                let c = line.as_bytes()[i] as char;
                if c == '\\' {
                    i += 1 + toml_char_len(line, i + 1).min(line.len() - i - 1);
                } else if c == '"' {
                    state = None;
                    i += 1;
                } else {
                    i += toml_char_len(line, i);
                }
            }
            Some(S::Literal) => {
                if line.as_bytes()[i] as char == '\'' {
                    state = None;
                    i += 1;
                } else {
                    i += toml_char_len(line, i);
                }
            }
            Some(S::MlBasic) => {
                if line[i..].starts_with("\"\"\"") {
                    state = None;
                    i += 3;
                } else if line.as_bytes()[i] as char == '\\' {
                    i += 1 + toml_char_len(line, i + 1).min(line.len() - i - 1);
                } else {
                    i += toml_char_len(line, i);
                }
            }
            Some(S::MlLiteral) => {
                if line[i..].starts_with("'''") {
                    state = None;
                    i += 3;
                } else {
                    i += toml_char_len(line, i);
                }
            }
        }
    }
    None
}

/// Byte offset of the first `=` outside any string, if any.
fn find_toml_equals(code: &str) -> Option<usize> {
    #[derive(PartialEq)]
    enum S {
        Basic,
        Literal,
        MlBasic,
        MlLiteral,
    }
    let mut state: Option<S> = None;
    let mut i = 0;
    while i < code.len() {
        match state {
            None => {
                if code[i..].starts_with("\"\"\"") {
                    state = Some(S::MlBasic);
                    i += 3;
                } else if code[i..].starts_with("'''") {
                    state = Some(S::MlLiteral);
                    i += 3;
                } else {
                    let c = code.as_bytes()[i] as char;
                    match c {
                        '"' => {
                            state = Some(S::Basic);
                            i += 1;
                        }
                        '\'' => {
                            state = Some(S::Literal);
                            i += 1;
                        }
                        '=' => return Some(i),
                        _ => i += toml_char_len(code, i),
                    }
                }
            }
            Some(S::Basic) => {
                let c = code.as_bytes()[i] as char;
                if c == '\\' {
                    i += 1 + toml_char_len(code, i + 1).min(code.len() - i - 1);
                } else if c == '"' {
                    state = None;
                    i += 1;
                } else {
                    i += toml_char_len(code, i);
                }
            }
            Some(S::Literal) => {
                if code.as_bytes()[i] as char == '\'' {
                    state = None;
                    i += 1;
                } else {
                    i += toml_char_len(code, i);
                }
            }
            Some(S::MlBasic) => {
                if code[i..].starts_with("\"\"\"") {
                    state = None;
                    i += 3;
                } else if code.as_bytes()[i] as char == '\\' {
                    i += 1 + toml_char_len(code, i + 1).min(code.len() - i - 1);
                } else {
                    i += toml_char_len(code, i);
                }
            }
            Some(S::MlLiteral) => {
                if code[i..].starts_with("'''") {
                    state = None;
                    i += 3;
                } else {
                    i += toml_char_len(code, i);
                }
            }
        }
    }
    None
}

/// Highlight dotted key/table segments: `a."b.c".d` keeps quoted dots
/// inside the segment; separator dots are dimmed.
fn highlight_toml_dotted(
    body: &str,
    out: &mut Vec<HiToken>,
    segment_fg: RatColor,
    segment_mod: Modifier,
    theme: Theme,
) {
    let mut seg_start = 0usize;
    let mut quote: Option<char> = None;
    let mut i = 0;
    // Byte-wise scan is safe: the only interesting bytes (`"`, `'`, `.`)
    // are single-byte and never appear inside multi-byte UTF-8 sequences.
    while i < body.len() {
        let c = body.as_bytes()[i] as char;
        match quote {
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    i += 1;
                }
                '.' => {
                    push_toml(out, &body[seg_start..i], segment_fg, segment_mod);
                    push_toml(out, ".", theme.hint, Modifier::empty());
                    i += 1;
                    seg_start = i;
                }
                _ => i += toml_char_len(body, i),
            },
            Some(q) => {
                if c == '\\' && q == '"' {
                    i += 1 + toml_char_len(body, i + 1).min(body.len() - i - 1);
                } else if c == q {
                    quote = None;
                    i += 1;
                } else {
                    i += toml_char_len(body, i);
                }
            }
        }
    }
    push_toml(out, &body[seg_start..], segment_fg, segment_mod);
}

/// Highlight the value side of `key = <value>`: strings green, numbers and
/// datetimes orange, booleans magenta italic, brackets/commas dimmed.
fn highlight_toml_value(fragment: &str, theme: Theme, out: &mut Vec<HiToken>) {
    let mut i = 0;
    while i < fragment.len() {
        let c = fragment.as_bytes()[i] as char;
        if c.is_whitespace() {
            let mut j = i;
            while j < fragment.len() && (fragment.as_bytes()[j] as char).is_whitespace() {
                j += toml_char_len(fragment, j);
            }
            push_toml(out, &fragment[i..j], theme.fg, Modifier::empty());
            i = j;
        } else if fragment[i..].starts_with("\"\"\"") || fragment[i..].starts_with("'''") {
            let q = &fragment[i..i + 3];
            let end = fragment[i + 3..]
                .find(q)
                .map(|k| i + 3 + k + 3)
                .unwrap_or(fragment.len());
            push_toml(
                out,
                &fragment[i..end],
                theme.syntax_string,
                Modifier::empty(),
            );
            i = end;
        } else if c == '"' {
            // Basic string with `\` escapes; unterminated runs to end of line.
            let mut j = i + 1;
            while j < fragment.len() {
                let d = fragment.as_bytes()[j] as char;
                if d == '\\' {
                    j += 1 + toml_char_len(fragment, j + 1).min(fragment.len() - j - 1);
                } else if d == '"' {
                    j += 1;
                    break;
                } else {
                    j += toml_char_len(fragment, j);
                }
            }
            push_toml(out, &fragment[i..j], theme.syntax_string, Modifier::empty());
            i = j;
        } else if c == '\'' {
            let end = fragment[i + 1..]
                .find('\'')
                .map(|k| i + 1 + k + 1)
                .unwrap_or(fragment.len());
            push_toml(
                out,
                &fragment[i..end],
                theme.syntax_string,
                Modifier::empty(),
            );
            i = end;
        } else if matches!(c, ',' | '[' | ']' | '{' | '}' | '=') {
            push_toml(out, &fragment[i..i + 1], theme.hint, Modifier::empty());
            i += 1;
        } else if c == '.' {
            // A lone dot (dotted-key separator inside a flow value, or a
            // malformed number fragment): keep it dimmed.
            push_toml(out, ".", theme.hint, Modifier::empty());
            i += 1;
        } else {
            // Bare word: booleans, numbers/datetimes, or plain text.
            let mut j = i;
            while j < fragment.len() {
                let d = fragment.as_bytes()[j] as char;
                if d.is_ascii_alphanumeric() || matches!(d, '_' | '+' | '-' | '.' | ':') {
                    j += 1;
                } else {
                    break;
                }
            }
            if j == i {
                push_toml(
                    out,
                    &fragment[i..i + toml_char_len(fragment, i)],
                    theme.fg,
                    Modifier::empty(),
                );
                i += toml_char_len(fragment, i);
                continue;
            }
            let word = &fragment[i..j];
            if word == "true" || word == "false" {
                push_toml(out, word, theme.syntax_keyword, Modifier::ITALIC);
            } else if word == "inf"
                || word == "nan"
                || word == "+inf"
                || word == "-inf"
                || word == "+nan"
                || word == "-nan"
                || word
                    .chars()
                    .next()
                    .is_some_and(|f| f.is_ascii_digit() || f == '+' || f == '-' || f == '.')
            {
                push_toml(out, word, theme.syntax_number, Modifier::empty());
            } else {
                push_toml(out, word, theme.fg, Modifier::empty());
            }
            i = j;
        }
    }
}

/// Hand-rolled single-line TOML highlight (syntect ships no TOML grammar):
/// cyan tables, blue keys, green strings, orange numbers, magenta booleans,
/// dimmed punctuation, italic comments.
fn highlight_toml_line(text: &str, theme: Theme) -> Vec<HiToken> {
    let mut out = Vec::new();
    // Split off the trailing comment first so `#` inside strings survives.
    let (code, comment) = match find_toml_comment_start(text) {
        Some(idx) => (&text[..idx], Some(&text[idx..])),
        None => (text, None),
    };
    if code.trim().is_empty() {
        if code.is_empty() {
            // Pure comment line.
        } else {
            push_toml(&mut out, code, theme.fg, Modifier::empty());
        }
        if let Some(c) = comment {
            push_toml(&mut out, c, theme.syntax_comment, Modifier::ITALIC);
        }
        if out.is_empty() {
            out.push(HiToken {
                text: text.to_string(),
                fg: theme.fg,
                modifier: Modifier::empty(),
            });
        }
        return out;
    }

    let trimmed = code.trim_start();
    if trimmed.starts_with('[') {
        // Table header: `[table]` / `[[array]]`, possibly indented.
        let indent = code.len() - trimmed.len();
        push_toml(&mut out, &code[..indent], theme.fg, Modifier::empty());
        let rest = &code[indent..];
        match (rest.find('['), rest.rfind(']')) {
            (Some(open), Some(close)) if open < close => {
                // `[[array]]` opens/closes with a double bracket.
                let double = rest[open..].starts_with("[[")
                    && close >= 1
                    && rest.as_bytes()[close - 1] == b']';
                let (open_end, inner_end) = if double {
                    (open + 2, close - 1)
                } else {
                    (open + 1, close)
                };
                push_toml(&mut out, &rest[..open_end], theme.hint, Modifier::empty());
                let inner = &rest[open_end..inner_end];
                let inner_trimmed = inner.trim();
                let lead = inner.len() - inner.trim_start().len();
                let trail = inner.len() - inner.trim_end().len();
                push_toml(&mut out, &inner[..lead], theme.fg, Modifier::empty());
                highlight_toml_dotted(
                    inner_trimmed,
                    &mut out,
                    theme.syntax_type,
                    Modifier::BOLD,
                    theme,
                );
                push_toml(
                    &mut out,
                    &inner[inner.len() - trail..],
                    theme.fg,
                    Modifier::empty(),
                );
                push_toml(
                    &mut out,
                    &rest[inner_end..close + 1],
                    theme.hint,
                    Modifier::empty(),
                );
                let tail = &rest[close + 1..];
                if !tail.is_empty() {
                    // Anything after `]` on a valid line is whitespace;
                    // render stray text as plain value content.
                    if tail.trim().is_empty() {
                        push_toml(&mut out, tail, theme.fg, Modifier::empty());
                    } else {
                        highlight_toml_value(tail, theme, &mut out);
                    }
                }
            }
            _ => highlight_toml_value(code, theme, &mut out),
        }
    } else if let Some(eq) = find_toml_equals(code) {
        let left = &code[..eq];
        let left_trimmed = left.trim();
        let lead = left.len() - left.trim_start().len();
        let trail = left.len() - left.trim_end().len();
        push_toml(&mut out, &left[..lead], theme.fg, Modifier::empty());
        highlight_toml_dotted(
            left_trimmed,
            &mut out,
            theme.syntax_function,
            Modifier::empty(),
            theme,
        );
        push_toml(
            &mut out,
            &left[left.len() - trail..],
            theme.fg,
            Modifier::empty(),
        );
        push_toml(&mut out, "=", theme.hint, Modifier::empty());
        highlight_toml_value(&code[eq + 1..], theme, &mut out);
    } else {
        highlight_toml_value(code, theme, &mut out);
    }
    if let Some(c) = comment {
        push_toml(&mut out, c, theme.syntax_comment, Modifier::ITALIC);
    }
    if out.is_empty() {
        out.push(HiToken {
            text: text.to_string(),
            fg: theme.fg,
            modifier: Modifier::empty(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Theme;

    #[test]
    fn markdown_headings_are_bold_function_color() {
        let theme = Theme::tokyo_night();
        for (line, word) in [("# Hello world", "Hello"), ("## Sub head", "Sub")] {
            let toks = highlight_line("README.md", line, theme);
            let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
            assert_eq!(joined, line);
            let head = toks
                .iter()
                .find(|t| t.text.contains(word))
                .expect("heading text token");
            assert_eq!(head.fg, theme.syntax_function, "{toks:?}");
            assert!(
                head.modifier.contains(Modifier::BOLD),
                "heading should be bold: {toks:?}"
            );
        }
    }

    #[test]
    fn markdown_emphasis_code_and_links_are_styled() {
        let theme = Theme::tokyo_night();
        let line = "**bold** *em* `code` [link](https://x.y)";
        let toks = highlight_line("notes.md", line, theme);
        let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, line);
        let bold = toks.iter().find(|t| t.text == "bold").expect("bold token");
        assert!(
            bold.modifier.contains(Modifier::BOLD),
            "bold should be bold: {toks:?}"
        );
        let em = toks.iter().find(|t| t.text == "em").expect("italic token");
        assert!(
            em.modifier.contains(Modifier::ITALIC),
            "italic should be italic: {toks:?}"
        );
        let code = toks.iter().find(|t| t.text == "code").expect("code token");
        assert_eq!(code.fg, theme.syntax_string, "{toks:?}");
        assert!(
            toks.iter()
                .any(|t| t.fg == theme.syntax_type && t.modifier.contains(Modifier::UNDERLINED)),
            "link should be underlined cyan: {toks:?}"
        );
    }

    #[test]
    fn markdown_punctuation_is_dimmed() {
        let theme = Theme::tokyo_night();
        let toks = highlight_line("README.md", "# Title", theme);
        let hash = toks.iter().find(|t| t.text == "#").expect("hash token");
        assert_eq!(hash.fg, theme.hint, "{toks:?}");
    }

    #[test]
    fn markdown_extension_variants_use_markdown_grammar() {
        let theme = Theme::tokyo_night();
        for path in ["notes.markdown", "page.mdx", "doc.mkd"] {
            let toks = highlight_line(path, "# Title", theme);
            assert!(
                toks.iter().any(|t| t.fg == theme.syntax_function),
                "{path} should highlight as Markdown: {toks:?}"
            );
        }
    }

    #[test]
    fn typescript_files_get_keyword_and_comment_colors() {
        // syntect's default set ships no TypeScript/TSX grammar; .ts files
        // must still highlight (via the JavaScript grammar), not render flat.
        let theme = Theme::tokyo_night();
        for path in ["a.ts", "a.tsx", "a.mts", "a.jsx", "a.mjs"] {
            let toks = highlight_line(path, "const x = 1; // hi", theme);
            assert!(
                toks.iter()
                    .any(|t| t.text.contains("const") && t.fg == theme.syntax_keyword),
                "{path}: `const` should carry the keyword color: {toks:?}"
            );
            assert!(
                toks.iter().any(|t| t.fg == theme.syntax_comment),
                "{path}: comment should carry the comment color: {toks:?}"
            );
        }
    }

    #[test]
    fn rust_keywords_get_keyword_color() {
        let theme = Theme::tokyo_night();
        let toks = highlight_line("src/main.rs", "fn main() { let x = 1; }", theme);
        let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, "fn main() { let x = 1; }");
        // `fn` should carry the keyword (magenta) color.
        let fn_tok = toks.iter().find(|t| t.text == "fn").expect("fn token");
        assert_eq!(fn_tok.fg, theme.syntax_keyword);
    }

    #[test]
    fn line_comment_gets_comment_color() {
        let theme = Theme::tokyo_night();
        let toks = highlight_line("a.py", "# hello world", theme);
        assert!(!toks.is_empty());
        assert!(
            toks.iter().all(|t| t.fg == theme.syntax_comment),
            "comment should be uniform: {toks:?}"
        );
    }

    #[test]
    fn unknown_extension_falls_back_to_fg() {
        let theme = Theme::tokyo_night();
        let toks = highlight_line("file.unknownext123", "hello", theme);
        let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, "hello");
    }

    #[test]
    fn file_lines_preserve_every_line() {
        let theme = Theme::default_theme();
        let out = highlight_file_lines("a.rs", &["fn a() {}", "// c", ""], theme);
        assert_eq!(out.len(), 3);
        let joined: String = out[0].iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, "fn a() {}");
    }

    #[test]
    fn very_long_lines_use_plain_foreground() {
        // Multi-KB lines (minified files) must not run regex highlighting:
        // tens of ms per frame while scrolling.
        let theme = Theme::tokyo_night();
        let long = "x".repeat(5000);
        let toks = highlight_line("src/main.rs", &long, theme);
        let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, long);
        assert!(toks.iter().all(|t| t.fg == theme.fg));
    }

    #[test]
    fn highlight_cache_keys_on_theme() {
        // Same line under two themes must not share cached tokens: a custom
        // keyword color must show up only under its own theme.
        let line = "fn main() {}";
        let base = Theme::tokyo_night();
        let custom = Theme {
            syntax_keyword: ratatui::style::Color::Red,
            ..base
        };
        let base_fn = highlight_line("a.rs", line, base);
        let custom_fn = highlight_line("a.rs", line, custom);
        let fg_base = base_fn.iter().find(|t| t.text == "fn").unwrap().fg;
        let fg_custom = custom_fn.iter().find(|t| t.text == "fn").unwrap().fg;
        assert_eq!(fg_base, base.syntax_keyword);
        assert_ne!(
            fg_custom, fg_base,
            "custom theme must not get cached tokens"
        );
        // And the base theme still resolves correctly afterwards.
        let again = highlight_line("a.rs", line, base);
        assert_eq!(again.iter().find(|t| t.text == "fn").unwrap().fg, fg_base);
    }

    #[test]
    fn toml_table_header_is_bold_type_color() {
        let theme = Theme::tokyo_night();
        for (line, name) in [
            ("[workspace]", "workspace"),
            ("[profile.release]", "profile"),
            ("[[bin]]", "bin"),
        ] {
            let toks = highlight_line("Cargo.toml", line, theme);
            let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
            assert_eq!(joined, line, "{toks:?}");
            let head = toks
                .iter()
                .find(|t| t.text == name)
                .expect("table name token");
            assert_eq!(head.fg, theme.syntax_type, "{line}: {toks:?}");
            assert!(
                head.modifier.contains(Modifier::BOLD),
                "{line}: table should be bold: {toks:?}"
            );
            assert!(
                toks.iter()
                    .any(|t| t.fg == theme.hint && t.text.contains('[')),
                "{line}: brackets should be dimmed: {toks:?}"
            );
        }
    }

    #[test]
    fn toml_key_is_function_and_equals_is_dimmed() {
        let theme = Theme::tokyo_night();
        let line = "members = [\"a\", \"b\"]";
        let toks = highlight_line("Cargo.toml", line, theme);
        let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, line, "{toks:?}");
        let key = toks
            .iter()
            .find(|t| t.text == "members")
            .expect("key token");
        assert_eq!(key.fg, theme.syntax_function, "{toks:?}");
        let eq = toks.iter().find(|t| t.text == "=").expect("equals token");
        assert_eq!(eq.fg, theme.hint, "{toks:?}");
        assert!(
            toks.iter()
                .any(|t| t.text == "\"a\"" && t.fg == theme.syntax_string),
            "array strings should be green: {toks:?}"
        );
    }

    #[test]
    fn toml_strings_numbers_and_bools_are_styled() {
        let theme = Theme::tokyo_night();
        let cases = [
            (
                "name = \"tokyo-night\"",
                "\"tokyo-night\"",
                theme.syntax_string,
            ),
            ("path = 'literal'", "'literal'", theme.syntax_string),
            ("count = 42", "42", theme.syntax_number),
            ("ratio = 3.14", "3.14", theme.syntax_number),
            ("hex = 0xDEAD_beef", "0xDEAD_beef", theme.syntax_number),
            (
                "when = 1979-05-27T07:32:00Z",
                "1979-05-27T07:32:00Z",
                theme.syntax_number,
            ),
        ];
        for (line, word, fg) in cases {
            let toks = highlight_line("config.toml", line, theme);
            let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
            assert_eq!(joined, line, "{toks:?}");
            assert!(
                toks.iter().any(|t| t.text == word && t.fg == fg),
                "{line}: {word:?} should be {fg:?}: {toks:?}"
            );
        }
        for word in ["true", "false"] {
            let line = format!("enabled = {word}");
            let toks = highlight_line("config.toml", &line, theme);
            let tok = toks.iter().find(|t| t.text == word).expect("bool token");
            assert_eq!(tok.fg, theme.syntax_keyword, "{toks:?}");
            assert!(
                tok.modifier.contains(Modifier::ITALIC),
                "bool should be italic: {toks:?}"
            );
        }
    }

    #[test]
    fn toml_comments_are_italic_and_hash_in_string_survives() {
        let theme = Theme::tokyo_night();
        let toks = highlight_line("Cargo.toml", "# hello world", theme);
        assert!(
            toks.iter().all(|t| t.fg == theme.syntax_comment),
            "full-line comment should be uniform: {toks:?}"
        );
        assert!(
            toks.iter().any(|t| t.modifier.contains(Modifier::ITALIC)),
            "comment should be italic: {toks:?}"
        );
        let line = "name = \"a#b\"  # trailing";
        let toks = highlight_line("Cargo.toml", line, theme);
        let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, line, "{toks:?}");
        let value = toks
            .iter()
            .find(|t| t.text == "\"a#b\"")
            .expect("string token");
        assert_eq!(value.fg, theme.syntax_string, "{toks:?}");
        let comment = toks
            .iter()
            .find(|t| t.text.starts_with('#'))
            .expect("comment token");
        assert_eq!(comment.fg, theme.syntax_comment, "{toks:?}");
    }

    #[test]
    fn toml_dotted_keys_and_cargo_lock_name_highlight() {
        let theme = Theme::tokyo_night();
        let toks = highlight_line("config.toml", "a.b = 1", theme);
        for seg in ["a", "b"] {
            assert!(
                toks.iter()
                    .any(|t| t.text == seg && t.fg == theme.syntax_function),
                "dotted key segment {seg:?} should be blue: {toks:?}"
            );
        }
        assert!(
            toks.iter().any(|t| t.text == "." && t.fg == theme.hint),
            "dotted key separator should be dimmed: {toks:?}"
        );
        // Cargo.lock is TOML content without the extension.
        let toks = highlight_line("Cargo.lock", "version = \"3\"", theme);
        assert!(
            toks.iter()
                .any(|t| t.text == "\"3\"" && t.fg == theme.syntax_string),
            "Cargo.lock should highlight as TOML: {toks:?}"
        );
        let out = highlight_file_lines("Cargo.toml", &["[workspace]", "members = []"], theme);
        assert_eq!(out.len(), 2);
        let joined: String = out[0].iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, "[workspace]");
    }
}
