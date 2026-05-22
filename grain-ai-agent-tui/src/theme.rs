//! Theme model + built-in presets + user-theme loader.
//!
//! Schema mirrors [ratatui-themes](https://github.com/ricardodantas/ratatui-themes):
//! ten core slots (`accent`, `secondary`, `bg` (unused for now),
//! `fg`, `muted`, `selection` (unused), `error`, `warning`, `success`,
//! `info`). The renderer in [`crate::ui`] only paints text/border
//! colors today — `bg` / `selection` are accepted in TOML but ignored,
//! reserved for future widgets.
//!
//! User themes live in `<themes_dir>/<name>.toml` (default
//! `<workspace>/.grain/themes`). Files that fail to parse are skipped
//! with a warning rather than failing startup — a typo'd hex shouldn't
//! lock the user out of their agent.

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

/// Color slots used by [`crate::ui`]. All required for built-in
/// presets; user TOML files must specify every slot too — better to
/// fail loudly than to inherit half a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub secondary: Color,
    pub fg: Color,
    /// Between `fg` and `muted` — secondary text that should be legible
    /// but not compete with primary content (timestamps, metadata).
    pub subdued: Color,
    pub muted: Color,
    pub error: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
    /// Background color for overlay cards (theme picker, help, etc.).
    /// Keeps popups visually distinct from the main transcript area
    /// which uses the terminal's default background.
    pub surface: Color,
}

// ---------------------------------------------------------------------------
// Built-in presets
// ---------------------------------------------------------------------------

/// Canonical built-ins shipped with the binary. Hex values pulled from
/// the well-known palettes that ratatui-themes catalogs.
///
/// The first entry is the default — `AppState::new` picks index 0 when
/// the CLI doesn't request a specific theme.
pub fn builtin_themes() -> Vec<Theme> {
    vec![
        builtin("default", Palette {
            accent: rgb(0xc6, 0x78, 0xdd),
            secondary: rgb(0x56, 0xb6, 0xc2),
            fg: rgb(0xeb, 0xeb, 0xeb),
            subdued: rgb(0xab, 0xad, 0xb8),
            muted: rgb(0x6c, 0x70, 0x86),
            error: rgb(0xe0, 0x6c, 0x75),
            warning: rgb(0xe5, 0xc0, 0x7b),
            success: rgb(0x98, 0xc3, 0x79),
            info: rgb(0x61, 0xaf, 0xef),
            surface: rgb(0x1e, 0x1e, 0x2e),
        }),
        builtin("dracula", Palette {
            accent: rgb(0xbd, 0x93, 0xf9),
            secondary: rgb(0x62, 0x72, 0xa4),
            fg: rgb(0xf8, 0xf8, 0xf2),
            subdued: rgb(0xad, 0xb5, 0xcb),
            muted: rgb(0x62, 0x72, 0xa4),
            error: rgb(0xff, 0x55, 0x55),
            warning: rgb(0xff, 0xb8, 0x6c),
            success: rgb(0x50, 0xfa, 0x7b),
            info: rgb(0x8b, 0xe9, 0xfd),
            surface: rgb(0x21, 0x22, 0x2c),
        }),
        builtin("nord", Palette {
            accent: rgb(0x88, 0xc0, 0xd0),
            secondary: rgb(0x81, 0xa1, 0xc1),
            fg: rgb(0xec, 0xef, 0xf4),
            subdued: rgb(0x9c, 0xa2, 0xaf),
            muted: rgb(0x4c, 0x56, 0x6a),
            error: rgb(0xbf, 0x61, 0x6a),
            warning: rgb(0xeb, 0xcb, 0x8b),
            success: rgb(0xa3, 0xbe, 0x8c),
            info: rgb(0x5e, 0x81, 0xac),
            surface: rgb(0x2e, 0x34, 0x40),
        }),
        builtin("gruvbox-dark", Palette {
            accent: rgb(0xfa, 0xbd, 0x2f),
            secondary: rgb(0xd6, 0x5d, 0x0e),
            fg: rgb(0xeb, 0xdb, 0xb2),
            subdued: rgb(0xbe, 0xaf, 0x93),
            muted: rgb(0x92, 0x83, 0x74),
            error: rgb(0xfb, 0x49, 0x34),
            warning: rgb(0xfe, 0x80, 0x19),
            success: rgb(0xb8, 0xbb, 0x26),
            info: rgb(0x83, 0xa5, 0x98),
            surface: rgb(0x32, 0x30, 0x2f),
        }),
        builtin("gruvbox-light", Palette {
            accent: rgb(0xd7, 0x99, 0x21),
            secondary: rgb(0xaf, 0x3a, 0x03),
            fg: rgb(0x3c, 0x38, 0x36),
            subdued: rgb(0x5c, 0x54, 0x4d),
            muted: rgb(0x7c, 0x6f, 0x64),
            error: rgb(0xcc, 0x24, 0x1d),
            warning: rgb(0xd6, 0x5d, 0x0e),
            success: rgb(0x98, 0x97, 0x1a),
            info: rgb(0x45, 0x85, 0x88),
            surface: rgb(0xeb, 0xdb, 0xb2),
        }),
        builtin("tokyo-night", Palette {
            accent: rgb(0x7a, 0xa2, 0xf7),
            secondary: rgb(0xbb, 0x9a, 0xf7),
            fg: rgb(0xc0, 0xca, 0xf5),
            subdued: rgb(0x8b, 0x95, 0xbf),
            muted: rgb(0x56, 0x5f, 0x89),
            error: rgb(0xf7, 0x76, 0x8e),
            warning: rgb(0xe0, 0xaf, 0x68),
            success: rgb(0x9e, 0xce, 0x6a),
            info: rgb(0x7d, 0xcf, 0xff),
            surface: rgb(0x1a, 0x1b, 0x26),
        }),
        builtin("catppuccin-mocha", Palette {
            accent: rgb(0xcb, 0xa6, 0xf7),
            secondary: rgb(0xf5, 0xc2, 0xe7),
            fg: rgb(0xcd, 0xd6, 0xf4),
            subdued: rgb(0x9c, 0xa3, 0xbd),
            muted: rgb(0x6c, 0x70, 0x86),
            error: rgb(0xf3, 0x8b, 0xa8),
            warning: rgb(0xfa, 0xb3, 0x87),
            success: rgb(0xa6, 0xe3, 0xa1),
            info: rgb(0x89, 0xb4, 0xfa),
            surface: rgb(0x18, 0x18, 0x25),
        }),
        builtin("solarized-dark", Palette {
            accent: rgb(0x26, 0x8b, 0xd2),
            secondary: rgb(0x2a, 0xa1, 0x98),
            fg: rgb(0x93, 0xa1, 0xa1),
            subdued: rgb(0x76, 0x88, 0x8b),
            muted: rgb(0x58, 0x6e, 0x75),
            error: rgb(0xdc, 0x32, 0x2f),
            warning: rgb(0xb5, 0x89, 0x00),
            success: rgb(0x85, 0x99, 0x00),
            info: rgb(0x6c, 0x71, 0xc4),
            surface: rgb(0x00, 0x2b, 0x36),
        }),
        builtin("one-dark-pro", Palette {
            accent: rgb(0x61, 0xaf, 0xef),
            secondary: rgb(0xc6, 0x78, 0xdd),
            fg: rgb(0xab, 0xb2, 0xbf),
            subdued: rgb(0x84, 0x8b, 0x98),
            muted: rgb(0x5c, 0x63, 0x70),
            error: rgb(0xe0, 0x6c, 0x75),
            warning: rgb(0xe5, 0xc0, 0x7b),
            success: rgb(0x98, 0xc3, 0x79),
            info: rgb(0x56, 0xb6, 0xc2),
            surface: rgb(0x21, 0x25, 0x2b),
        }),
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
    /// Optional — defaults to midpoint between `fg` and `muted` so
    /// older user themes still load.
    #[serde(default)]
    subdued: Option<String>,
    /// Optional — defaults to the same color as `muted` so older
    /// user themes (written before `surface` existed) still load.
    #[serde(default)]
    surface: Option<String>,
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
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: ThemeFile = toml::from_str(&text).map_err(|e| e.to_string())?;
    let fg = parse_color(&file.palette.fg)?;
    let muted = parse_color(&file.palette.muted)?;
    let subdued = match file.palette.subdued.as_deref() {
        Some(s) => parse_color(s)?,
        None => midpoint_color(fg, muted),
    };
    let surface = match file.palette.surface.as_deref() {
        Some(s) => parse_color(s)?,
        None => muted,
    };
    Ok(Theme {
        name: file.name,
        source: ThemeSource::User,
        palette: Palette {
            accent: parse_color(&file.palette.accent)?,
            secondary: parse_color(&file.palette.secondary)?,
            fg,
            subdued,
            muted,
            error: parse_color(&file.palette.error)?,
            warning: parse_color(&file.palette.warning)?,
            success: parse_color(&file.palette.success)?,
            info: parse_color(&file.palette.info)?,
            surface,
        },
    })
}

/// Average two colors component-wise. Used as the default `subdued`
/// when a user theme omits it.
fn midpoint_color(a: Color, b: Color) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => Color::Rgb(
            ((r1 as u16 + r2 as u16) / 2) as u8,
            ((g1 as u16 + g2 as u16) / 2) as u8,
            ((b1 as u16 + b2 as u16) / 2) as u8,
        ),
        _ => a,
    }
}

/// Parse `#rrggbb` or `#rgb`. ANSI names (`"red"`, `"cyan"`) are not
/// supported — built-ins already cover the common palettes, and forcing
/// hex keeps user files self-documenting.
fn parse_color(s: &str) -> Result<Color, String> {
    let hex = s.strip_prefix('#').ok_or_else(|| {
        format!("color '{s}' must start with '#' (e.g. \"#bd93f9\")")
    })?;
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16)
                .map_err(|_| format!("invalid hex '{s}'"))?;
            let g = u8::from_str_radix(&hex[2..4], 16)
                .map_err(|_| format!("invalid hex '{s}'"))?;
            let b = u8::from_str_radix(&hex[4..6], 16)
                .map_err(|_| format!("invalid hex '{s}'"))?;
            Ok(Color::Rgb(r, g, b))
        }
        3 => {
            // #rgb → #rrggbb (each nibble doubled, as in CSS).
            let r = u8::from_str_radix(&hex[0..1], 16)
                .map_err(|_| format!("invalid hex '{s}'"))?;
            let g = u8::from_str_radix(&hex[1..2], 16)
                .map_err(|_| format!("invalid hex '{s}'"))?;
            let b = u8::from_str_radix(&hex[2..3], 16)
                .map_err(|_| format!("invalid hex '{s}'"))?;
            Ok(Color::Rgb(r * 17, g * 17, b * 17))
        }
        _ => Err(format!(
            "color '{s}' must be 3 or 6 hex digits after '#'"
        )),
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
        assert_eq!(parse_color("#bd93f9").unwrap(), Color::Rgb(0xbd, 0x93, 0xf9));
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
