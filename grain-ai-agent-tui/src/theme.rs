//! Theme model + built-in presets + user-theme loader.
//!
//! Mirrors [ratatui-themes](https://github.com/ricardodantas/ratatui-themes)
//! with semantic slots: `accent`, `secondary`, `bg`, `fg`, `subdued`,
//! `muted`, `selection`, `error`, `warning`, `success`, `info`, `surface`.
//!
//! `bg` is the main terminal background — applied every frame via a
//! ratatui `Clear` widget so the entire terminal area (not just overlay
//! cards) honours the active theme.
//!
//! User themes live in `<themes_dir>/<name>.toml` (default
//! `<workspace>/.grain/themes`). Files that fail to parse are skipped
//! with a warning — a typo'd hex shouldn't lock the user out.

use std::path::Path;

use ratatui::style::Color;
use serde::Deserialize;

/// One theme: name + palette. `source` lets the picker badge built-in
/// vs user-defined entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub source: ThemeSource,
    pub palette: Palette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    BuiltIn,
    User,
}

/// Color slots used by [`crate::ui`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub secondary: Color,
    /// Main terminal background. Set once-per-frame via a full-screen
    /// `Clear` widget so the terminal background matches the theme.
    pub bg: Color,
    pub fg: Color,
    /// Between `fg` and `muted` — secondary text that should be legible
    /// but not compete with primary content (timestamps, metadata).
    pub subdued: Color,
    pub muted: Color,
    /// Selection / highlight background (picker rows, focused palette
    /// entries, transcript text selection).
    pub selection: Color,
    pub error: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
    /// Background for overlay cards (theme picker, help, doctor, etc.).
    pub surface: Color,
}

// ---------------------------------------------------------------------------
// Built-in presets — aligned with ratatui-themes 0.1 (15 themes)
// ---------------------------------------------------------------------------

pub fn builtin_themes() -> Vec<Theme> {
    vec![
        // --- grain default (dark, purple accent) ---
        builtin(
            "default",
            Palette {
                accent: rgb(0xc6, 0x78, 0xdd),
                secondary: rgb(0x56, 0xb6, 0xc2),
                bg: rgb(0x0e, 0x0f, 0x16),
                fg: rgb(0xeb, 0xeb, 0xeb),
                subdued: rgb(0xab, 0xad, 0xb8),
                muted: rgb(0x6c, 0x70, 0x86),
                selection: rgb(0x2a, 0x2a, 0x3e),
                error: rgb(0xe0, 0x6c, 0x75),
                warning: rgb(0xe5, 0xc0, 0x7b),
                success: rgb(0x98, 0xc3, 0x79),
                info: rgb(0x61, 0xaf, 0xef),
                surface: rgb(0x1e, 0x1e, 0x2e),
            },
        ),
        // --- Dracula ---
        builtin(
            "dracula",
            Palette {
                accent: rgb(0xbd, 0x93, 0xf9),
                secondary: rgb(0x62, 0x72, 0xa4),
                bg: rgb(0x28, 0x2a, 0x36),
                fg: rgb(0xf8, 0xf8, 0xf2),
                subdued: rgb(0xad, 0xb5, 0xcb),
                muted: rgb(0x62, 0x72, 0xa4),
                selection: rgb(0x44, 0x47, 0x5a),
                error: rgb(0xff, 0x55, 0x55),
                warning: rgb(0xff, 0xb8, 0x6c),
                success: rgb(0x50, 0xfa, 0x7b),
                info: rgb(0x8b, 0xe9, 0xfd),
                surface: rgb(0x21, 0x22, 0x2c),
            },
        ),
        // --- One Dark Pro ---
        builtin(
            "one-dark-pro",
            Palette {
                accent: rgb(0x61, 0xaf, 0xef),
                secondary: rgb(0xc6, 0x78, 0xdd),
                bg: rgb(0x28, 0x2c, 0x34),
                fg: rgb(0xab, 0xb2, 0xbf),
                subdued: rgb(0x84, 0x8b, 0x98),
                muted: rgb(0x5c, 0x63, 0x70),
                selection: rgb(0x3e, 0x44, 0x52),
                error: rgb(0xe0, 0x6c, 0x75),
                warning: rgb(0xe5, 0xc0, 0x7b),
                success: rgb(0x98, 0xc3, 0x79),
                info: rgb(0x56, 0xb6, 0xc2),
                surface: rgb(0x21, 0x25, 0x2b),
            },
        ),
        // --- Nord ---
        builtin(
            "nord",
            Palette {
                accent: rgb(0x88, 0xc0, 0xd0),
                secondary: rgb(0x81, 0xa1, 0xc1),
                bg: rgb(0x2e, 0x34, 0x40),
                fg: rgb(0xec, 0xef, 0xf4),
                subdued: rgb(0x9c, 0xa2, 0xaf),
                muted: rgb(0x4c, 0x56, 0x6a),
                selection: rgb(0x43, 0x4c, 0x5e),
                error: rgb(0xbf, 0x61, 0x6a),
                warning: rgb(0xeb, 0xcb, 0x8b),
                success: rgb(0xa3, 0xbe, 0x8c),
                info: rgb(0x5e, 0x81, 0xac),
                surface: rgb(0x3b, 0x42, 0x52),
            },
        ),
        // --- Catppuccin Mocha ---
        builtin(
            "catppuccin-mocha",
            Palette {
                accent: rgb(0xcb, 0xa6, 0xf7),
                secondary: rgb(0xf5, 0xc2, 0xe7),
                bg: rgb(0x1e, 0x1e, 0x2e),
                fg: rgb(0xcd, 0xd6, 0xf4),
                subdued: rgb(0x9c, 0xa3, 0xbd),
                muted: rgb(0x6c, 0x70, 0x86),
                selection: rgb(0x31, 0x32, 0x44),
                error: rgb(0xf3, 0x8b, 0xa8),
                warning: rgb(0xfa, 0xb3, 0x87),
                success: rgb(0xa6, 0xe3, 0xa1),
                info: rgb(0x89, 0xb4, 0xfa),
                surface: rgb(0x18, 0x18, 0x25),
            },
        ),
        // --- Catppuccin Latte (light) ---
        builtin(
            "catppuccin-latte",
            Palette {
                accent: rgb(0x1e, 0x66, 0xf5),
                secondary: rgb(0xea, 0x76, 0xcb),
                bg: rgb(0xef, 0xf1, 0xf5),
                fg: rgb(0x4c, 0x4f, 0x69),
                subdued: rgb(0x7c, 0x7f, 0x93),
                muted: rgb(0x8c, 0x8f, 0xa1),
                selection: rgb(0xcc, 0xd0, 0xda),
                error: rgb(0xd2, 0x0f, 0x39),
                warning: rgb(0xdf, 0x8e, 0x1d),
                success: rgb(0x40, 0xa0, 0x2b),
                info: rgb(0x17, 0x92, 0x99),
                surface: rgb(0xe6, 0xe9, 0xef),
            },
        ),
        // --- Gruvbox Dark ---
        builtin(
            "gruvbox-dark",
            Palette {
                accent: rgb(0xfa, 0xbd, 0x2f),
                secondary: rgb(0xd3, 0x86, 0x9b),
                bg: rgb(0x28, 0x28, 0x28),
                fg: rgb(0xeb, 0xdb, 0xb2),
                subdued: rgb(0xbe, 0xaf, 0x93),
                muted: rgb(0x92, 0x83, 0x74),
                selection: rgb(0x50, 0x49, 0x45),
                error: rgb(0xfb, 0x49, 0x34),
                warning: rgb(0xfe, 0x80, 0x19),
                success: rgb(0xb8, 0xbb, 0x26),
                info: rgb(0x83, 0xa5, 0x98),
                surface: rgb(0x32, 0x30, 0x2f),
            },
        ),
        // --- Gruvbox Light ---
        builtin(
            "gruvbox-light",
            Palette {
                accent: rgb(0xb5, 0x76, 0x14),
                secondary: rgb(0x8f, 0x3f, 0x71),
                bg: rgb(0xfb, 0xf1, 0xc7),
                fg: rgb(0x3c, 0x38, 0x36),
                subdued: rgb(0x5c, 0x54, 0x4d),
                muted: rgb(0x7c, 0x6f, 0x64),
                selection: rgb(0xd5, 0xc4, 0xa1),
                error: rgb(0x9d, 0x00, 0x06),
                warning: rgb(0xaf, 0x3a, 0x03),
                success: rgb(0x79, 0x74, 0x0e),
                info: rgb(0x45, 0x85, 0x88),
                surface: rgb(0xeb, 0xdb, 0xb2),
            },
        ),
        // --- Tokyo Night ---
        builtin(
            "tokyo-night",
            Palette {
                accent: rgb(0x7a, 0xa2, 0xf7),
                secondary: rgb(0xbb, 0x9a, 0xf7),
                bg: rgb(0x1a, 0x1b, 0x26),
                fg: rgb(0xc0, 0xca, 0xf5),
                subdued: rgb(0x8b, 0x95, 0xbf),
                muted: rgb(0x56, 0x5f, 0x89),
                selection: rgb(0x33, 0x35, 0x54),
                error: rgb(0xf7, 0x76, 0x8e),
                warning: rgb(0xe0, 0xaf, 0x68),
                success: rgb(0x9e, 0xce, 0x6a),
                info: rgb(0x7d, 0xcf, 0xff),
                surface: rgb(0x24, 0x28, 0x3b),
            },
        ),
        // --- Solarized Dark ---
        builtin(
            "solarized-dark",
            Palette {
                accent: rgb(0x26, 0x8b, 0xd2),
                secondary: rgb(0x2a, 0xa1, 0x98),
                bg: rgb(0x00, 0x2b, 0x36),
                fg: rgb(0x93, 0xa1, 0xa1),
                subdued: rgb(0x76, 0x88, 0x8b),
                muted: rgb(0x58, 0x6e, 0x75),
                selection: rgb(0x07, 0x3d, 0x42),
                error: rgb(0xdc, 0x32, 0x2f),
                warning: rgb(0xb5, 0x89, 0x00),
                success: rgb(0x85, 0x99, 0x00),
                info: rgb(0x6c, 0x71, 0xc4),
                surface: rgb(0x00, 0x2b, 0x36),
            },
        ),
        // --- Solarized Light ---
        builtin(
            "solarized-light",
            Palette {
                accent: rgb(0x26, 0x8b, 0xd2),
                secondary: rgb(0x2a, 0xa1, 0x98),
                bg: rgb(0xfd, 0xf6, 0xe3),
                fg: rgb(0x58, 0x6e, 0x75),
                subdued: rgb(0x6c, 0x7a, 0x80),
                muted: rgb(0x83, 0x94, 0x96),
                selection: rgb(0xee, 0xe8, 0xd5),
                error: rgb(0xdc, 0x32, 0x2f),
                warning: rgb(0xb5, 0x89, 0x00),
                success: rgb(0x85, 0x99, 0x00),
                info: rgb(0x6c, 0x71, 0xc4),
                surface: rgb(0xee, 0xe8, 0xd5),
            },
        ),
        // --- Monokai Pro ---
        builtin(
            "monokai-pro",
            Palette {
                accent: rgb(0xff, 0x61, 0x86),
                secondary: rgb(0xfc, 0x98, 0x67),
                bg: rgb(0x2d, 0x2a, 0x2e),
                fg: rgb(0xfc, 0xf1, 0xda),
                subdued: rgb(0x93, 0x91, 0x8d),
                muted: rgb(0x72, 0x70, 0x72),
                selection: rgb(0x4a, 0x46, 0x4c),
                error: rgb(0xff, 0x61, 0x86),
                warning: rgb(0xfc, 0x98, 0x67),
                success: rgb(0xa9, 0xdc, 0x76),
                info: rgb(0x78, 0xd8, 0xec),
                surface: rgb(0x22, 0x1f, 0x22),
            },
        ),
        // --- Rosé Pine ---
        builtin(
            "rose-pine",
            Palette {
                accent: rgb(0xc4, 0xa7, 0xe7),
                secondary: rgb(0xeb, 0xbc, 0xba),
                bg: rgb(0x19, 0x17, 0x24),
                fg: rgb(0xe0, 0xde, 0xf4),
                subdued: rgb(0x98, 0x95, 0xa8),
                muted: rgb(0x6e, 0x6a, 0x86),
                selection: rgb(0x26, 0x23, 0x3a),
                error: rgb(0xeb, 0x6f, 0x92),
                warning: rgb(0xf6, 0xc1, 0x77),
                success: rgb(0x31, 0x74, 0x8f),
                info: rgb(0x9c, 0xcf, 0xd8),
                surface: rgb(0x1f, 0x1d, 0x2e),
            },
        ),
        // --- Kanagawa ---
        builtin(
            "kanagawa",
            Palette {
                accent: rgb(0x7e, 0x9c, 0xd8),
                secondary: rgb(0x95, 0x7f, 0xb8),
                bg: rgb(0x1f, 0x1f, 0x28),
                fg: rgb(0xdc, 0xd7, 0xba),
                subdued: rgb(0xc8, 0xc0, 0x93),
                muted: rgb(0x72, 0x72, 0x7b),
                selection: rgb(0x36, 0x38, 0x46),
                error: rgb(0xe4, 0x68, 0x76),
                warning: rgb(0xff, 0xa0, 0x66),
                success: rgb(0x98, 0xbb, 0x6c),
                info: rgb(0x7f, 0xb4, 0xca),
                surface: rgb(0x16, 0x16, 0x1d),
            },
        ),
        // --- Everforest ---
        builtin(
            "everforest",
            Palette {
                accent: rgb(0xd3, 0xc6, 0xaa),
                secondary: rgb(0xe6, 0x98, 0x75),
                bg: rgb(0x27, 0x2e, 0x33),
                fg: rgb(0xd3, 0xc6, 0xaa),
                subdued: rgb(0x9d, 0xa9, 0xa0),
                muted: rgb(0x7a, 0x84, 0x78),
                selection: rgb(0x3a, 0x45, 0x4c),
                error: rgb(0xe6, 0x7e, 0x80),
                warning: rgb(0xdb, 0xbc, 0x7f),
                success: rgb(0xa7, 0xc0, 0x80),
                info: rgb(0x7f, 0xbb, 0xb3),
                surface: rgb(0x2d, 0x35, 0x3b),
            },
        ),
        // --- Cyberpunk ---
        builtin(
            "cyberpunk",
            Palette {
                accent: rgb(0xf7, 0x06, 0x9e),
                secondary: rgb(0x00, 0xdf, 0xff),
                bg: rgb(0x0c, 0x0c, 0x14),
                fg: rgb(0xe0, 0xe0, 0xe0),
                subdued: rgb(0x99, 0x99, 0xaa),
                muted: rgb(0x55, 0x55, 0x66),
                selection: rgb(0x22, 0x22, 0x44),
                error: rgb(0xff, 0x20, 0x55),
                warning: rgb(0xff, 0xdd, 0x44),
                success: rgb(0x00, 0xff, 0x99),
                info: rgb(0x00, 0xdf, 0xff),
                surface: rgb(0x15, 0x15, 0x25),
            },
        ),
    ]
}

fn builtin(name: &str, palette: Palette) -> Theme {
    Theme {
        name: name.to_string(),
        source: ThemeSource::BuiltIn,
        palette,
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

// ---------------------------------------------------------------------------
// User-theme TOML loader
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ThemeFile {
    name: String,
    palette: PaletteFile,
}

#[derive(Debug, Deserialize)]
struct PaletteFile {
    accent: String,
    secondary: String,
    fg: String,
    muted: String,
    error: String,
    warning: String,
    success: String,
    info: String,
    /// Optional — defaults to the same colour as `muted` so older
    /// user themes still load.
    #[serde(default)]
    subdued: Option<String>,
    /// Optional — defaults to the same colour as `muted`.
    #[serde(default)]
    bg: Option<String>,
    /// Optional — defaults to `bg` (or `muted` if `bg` is also absent).
    #[serde(default)]
    surface: Option<String>,
    /// Optional — defaults to a dimmed variant of `bg`.
    #[serde(default)]
    selection: Option<String>,
}

/// Scan `dir` for `*.toml` files, parse each as a [`ThemeFile`], and
/// return the successful themes plus per-file diagnostic strings.
///
/// Missing or empty directory → returns `(vec![], vec![])`. Single
/// broken file → that file's error message goes into the warnings
/// vector and the loader keeps going.
pub fn load_user_themes(dir: &Path) -> (Vec<Theme>, Vec<String>) {
    let mut themes = Vec::new();
    let mut warnings = Vec::new();

    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (themes, warnings),
        Err(e) => {
            warnings.push(format!("themes dir {}: {e}", dir.display()));
            return (themes, warnings);
        }
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        match load_theme_file(&path) {
            Ok(theme) => themes.push(theme),
            Err(e) => warnings.push(format!("theme {}: {e}", path.display())),
        }
    }

    // Stable ordering — useful for tests and predictable picker UX.
    themes.sort_by(|a, b| a.name.cmp(&b.name));
    (themes, warnings)
}

fn load_theme_file(path: &Path) -> Result<Theme, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("read {path}: {e}", path = path.display()))?;
    let file: ThemeFile =
        toml::from_str(&raw).map_err(|e| format!("parse {path}: {e}", path = path.display()))?;
    let subdued = file
        .palette
        .subdued
        .as_deref()
        .map(parse_color)
        .transpose()?
        .unwrap_or_else(|| {
            // midpoint between fg and muted
            let fg = parse_color(&file.palette.fg).unwrap_or(Color::White);
            let muted = parse_color(&file.palette.muted).unwrap_or(Color::Gray);
            midpoint(fg, muted)
        });
    let bg = file
        .palette
        .bg
        .as_deref()
        .map(parse_color)
        .transpose()?
        .unwrap_or_else(|| parse_color(&file.palette.muted).unwrap_or(Color::Black));

    let surface = file
        .palette
        .surface
        .as_deref()
        .map(parse_color)
        .transpose()?
        .unwrap_or(bg);

    let selection = file
        .palette
        .selection
        .as_deref()
        .map(parse_color)
        .transpose()?
        .unwrap_or_else(|| dim(bg, 0.15));

    Ok(Theme {
        name: file.name,
        source: ThemeSource::User,
        palette: Palette {
            accent: parse_color(&file.palette.accent)?,
            secondary: parse_color(&file.palette.secondary)?,
            bg,
            fg: parse_color(&file.palette.fg)?,
            subdued,
            muted: parse_color(&file.palette.muted)?,
            selection,
            error: parse_color(&file.palette.error)?,
            warning: parse_color(&file.palette.warning)?,
            success: parse_color(&file.palette.success)?,
            info: parse_color(&file.palette.info)?,
            surface,
        },
    })
}

fn midpoint(a: Color, b: Color) -> Color {
    let Color::Rgb(r1, g1, b1) = a else {
        return b;
    };
    let Color::Rgb(r2, g2, b2) = b else {
        return a;
    };
    Color::Rgb(
        r1.saturating_add(r2) / 2,
        g1.saturating_add(g2) / 2,
        b1.saturating_add(b2) / 2,
    )
}

fn dim(c: Color, factor: f32) -> Color {
    let Color::Rgb(r, g, b) = c else {
        return c;
    };
    Color::Rgb(
        (r as f32 * (1.0 - factor)) as u8,
        (g as f32 * (1.0 - factor)) as u8,
        (b as f32 * (1.0 - factor)) as u8,
    )
}

fn parse_color(s: &str) -> Result<Color, String> {
    let s = s.trim();
    if !s.starts_with('#') {
        return Err(format!("color '{s}' must start with '#'"));
    }
    let hex = &s[1..];
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).map_err(|_| format!("invalid hex '{s}'"))?;
            let g = u8::from_str_radix(&hex[2..4], 16).map_err(|_| format!("invalid hex '{s}'"))?;
            let b = u8::from_str_radix(&hex[4..6], 16).map_err(|_| format!("invalid hex '{s}'"))?;
            Ok(Color::Rgb(r, g, b))
        }
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).map_err(|_| format!("invalid hex '{s}'"))?;
            let g = u8::from_str_radix(&hex[1..2], 16).map_err(|_| format!("invalid hex '{s}'"))?;
            let b = u8::from_str_radix(&hex[2..3], 16).map_err(|_| format!("invalid hex '{s}'"))?;
            Ok(Color::Rgb(r * 17, g * 17, b * 17))
        }
        _ => Err(format!("color '{s}' must be 3 or 6 hex digits after '#'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn builtin_default_is_first_and_present() {
        let ts = builtin_themes();
        assert!(!ts.is_empty());
        assert_eq!(ts[0].name, "default");
        assert!(ts.iter().all(|t| t.source == ThemeSource::BuiltIn));
    }

    #[test]
    fn builtin_count_matches_ratatui_themes_catalog() {
        let ts = builtin_themes();
        // 15 ratatui-themes + 1 grain "default"
        assert_eq!(ts.len(), 16);
    }

    #[test]
    fn builtin_named_presets_match_upstream_accents() {
        let ts = builtin_themes();
        let by_name: std::collections::HashMap<_, _> =
            ts.iter().map(|t| (t.name.as_str(), &t.palette)).collect();
        assert_eq!(by_name["dracula"].accent, Color::Rgb(0xbd, 0x93, 0xf9));
        assert_eq!(by_name["nord"].accent, Color::Rgb(0x88, 0xc0, 0xd0));
        assert_eq!(by_name["gruvbox-dark"].accent, Color::Rgb(0xfa, 0xbd, 0x2f));
        assert_eq!(by_name["tokyo-night"].accent, Color::Rgb(0x7a, 0xa2, 0xf7));
    }

    #[test]
    fn parse_color_rejects_missing_hash() {
        assert!(parse_color("bd93f9").is_err());
    }

    #[test]
    fn parse_color_accepts_three_and_six_digit_hex() {
        assert_eq!(parse_color("#abc").unwrap(), Color::Rgb(170, 187, 204));
        assert_eq!(
            parse_color("#bd93f9").unwrap(),
            Color::Rgb(0xbd, 0x93, 0xf9)
        );
    }

    #[test]
    fn load_user_themes_handles_missing_dir() {
        let (ts, ws) = load_user_themes(std::path::Path::new("/tmp/grain-no-such-dir-12345"));
        assert!(ts.is_empty());
        assert!(ws.is_empty());
    }

    #[test]
    fn load_user_themes_skips_broken_file_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("good.toml"), GOOD_TOML).unwrap();
        fs::write(tmp.path().join("bad.toml"), "this is not toml = =").unwrap();

        let (ts, ws) = load_user_themes(tmp.path());
        assert_eq!(ts.len(), 1, "good theme loaded");
        assert_eq!(ts[0].name, "myvibes");
        assert!(matches!(ts[0].source, ThemeSource::User));
        assert_eq!(ws.len(), 1, "one warning for bad file");
        assert!(ws[0].contains("bad.toml"));
    }

    const GOOD_TOML: &str = r##"
name = "myvibes"
[palette]
accent = "#bd93f9"
secondary = "#6272a4"
fg = "#f8f8f2"
muted = "#6272a4"
error = "#ff5555"
warning = "#ffb86c"
success = "#50fa7b"
info = "#8be9fd"
"##;
}
