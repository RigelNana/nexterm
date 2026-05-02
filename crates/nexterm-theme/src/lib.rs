//! # nexterm-theme
//!
//! Theme engine: 16-color + 256-color + true-color palettes, dynamic opacity.
//! Provides both serializable `Theme` (hex strings) and `ResolvedTheme` (f32 RGBA).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Serializable theme definition (TOML-friendly)
// ---------------------------------------------------------------------------

/// A complete color theme (hex string format for serialization).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub colors: ThemeColors,
    pub cursor: CursorColors,
    pub selection: SelectionColors,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeColors {
    pub foreground: String,
    pub background: String,
    /// ANSI 16 colors: [black, red, green, yellow, blue, magenta, cyan, white, bright variants...]
    pub ansi: [String; 16],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorColors {
    pub foreground: String,
    pub background: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionColors {
    pub foreground: Option<String>,
    pub background: String,
}

impl Default for Theme {
    fn default() -> Self {
        catppuccin_mocha()
    }
}

/// Load a theme from a TOML string.
pub fn load_theme_toml(content: &str) -> anyhow::Result<Theme> {
    let theme: Theme = toml::from_str(content)?;
    Ok(theme)
}

// ---------------------------------------------------------------------------
// Resolved theme (GPU-ready f32 RGBA)
// ---------------------------------------------------------------------------

/// Pre-resolved theme colors for direct GPU use.
#[derive(Debug, Clone)]
pub struct ResolvedTheme {
    pub name: String,
    pub fg: [f32; 4],
    pub bg: [f32; 4],
    pub cursor_fg: [f32; 4],
    pub cursor_bg: [f32; 4],
    pub selection_fg: Option<[f32; 4]>,
    pub selection_bg: [f32; 4],
    /// ANSI 16-color palette.
    pub ansi: [[f32; 3]; 16],
    /// Tab bar background.
    pub tab_bar_bg: [f32; 4],
    /// Tab bar active tab background.
    pub tab_active_bg: [f32; 4],
}

impl ResolvedTheme {
    /// Resolve a serializable Theme into GPU-ready colors.
    pub fn from_theme(theme: &Theme) -> Self {
        let fg = hex_to_rgba(&theme.colors.foreground);
        let bg = hex_to_rgba(&theme.colors.background);
        let cursor_fg = hex_to_rgba(&theme.cursor.foreground);
        let cursor_bg = hex_to_rgba(&theme.cursor.background);
        let selection_fg = theme.selection.foreground.as_ref().map(|s| hex_to_rgba(s));
        let selection_bg = hex_to_rgba(&theme.selection.background);

        let mut ansi = [[0.0f32; 3]; 16];
        for (i, hex) in theme.colors.ansi.iter().enumerate() {
            let c = hex_to_rgba(hex);
            ansi[i] = [c[0], c[1], c[2]];
        }

        // Derive tab bar colors: slightly darker than bg
        let tab_bar_bg = darken(bg, 0.7);
        let tab_active_bg = lighten(bg, 1.3);

        Self {
            name: theme.name.clone(),
            fg,
            bg,
            cursor_fg,
            cursor_bg,
            selection_fg,
            selection_bg,
            ansi,
            tab_bar_bg,
            tab_active_bg,
        }
    }
}

impl Default for ResolvedTheme {
    fn default() -> Self {
        Self::from_theme(&Theme::default())
    }
}

// ---------------------------------------------------------------------------
// Built-in themes
// ---------------------------------------------------------------------------

/// Look up a built-in theme by name.
pub fn builtin_theme(name: &str) -> Option<Theme> {
    match name.to_lowercase().replace(['-', '_', ' '], "") {
        s if s.contains("catppuccin") && s.contains("mocha") => Some(catppuccin_mocha()),
        s if s.contains("dracula") => Some(dracula()),
        s if s.contains("nord") => Some(nord()),
        s if s.contains("solarized") && s.contains("dark") => Some(solarized_dark()),
        s if s.contains("onedark") || s.contains("one") && s.contains("dark") => Some(one_dark()),
        s if s.contains("gruvbox") => Some(gruvbox_dark()),
        _ => None,
    }
}

/// List names of all built-in themes.
pub fn builtin_theme_names() -> &'static [&'static str] {
    &[
        "catppuccin-mocha",
        "dracula",
        "nord",
        "solarized-dark",
        "one-dark",
        "gruvbox-dark",
    ]
}

pub fn catppuccin_mocha() -> Theme {
    Theme {
        name: "catppuccin-mocha".into(),
        colors: ThemeColors {
            foreground: "#cdd6f4".into(),
            background: "#1e1e2e".into(),
            ansi: [
                "#45475a".into(),
                "#f38ba8".into(),
                "#a6e3a1".into(),
                "#f9e2af".into(),
                "#89b4fa".into(),
                "#f5c2e7".into(),
                "#94e2d5".into(),
                "#bac2de".into(),
                "#585b70".into(),
                "#f38ba8".into(),
                "#a6e3a1".into(),
                "#f9e2af".into(),
                "#89b4fa".into(),
                "#f5c2e7".into(),
                "#94e2d5".into(),
                "#a6adc8".into(),
            ],
        },
        cursor: CursorColors {
            foreground: "#1e1e2e".into(),
            background: "#f5e0dc".into(),
        },
        selection: SelectionColors {
            foreground: None,
            background: "#585b70".into(),
        },
    }
}

pub fn dracula() -> Theme {
    Theme {
        name: "dracula".into(),
        colors: ThemeColors {
            foreground: "#f8f8f2".into(),
            background: "#282a36".into(),
            ansi: [
                "#21222c".into(),
                "#ff5555".into(),
                "#50fa7b".into(),
                "#f1fa8c".into(),
                "#bd93f9".into(),
                "#ff79c6".into(),
                "#8be9fd".into(),
                "#f8f8f2".into(),
                "#6272a4".into(),
                "#ff6e6e".into(),
                "#69ff94".into(),
                "#ffffa5".into(),
                "#d6acff".into(),
                "#ff92df".into(),
                "#a4ffff".into(),
                "#ffffff".into(),
            ],
        },
        cursor: CursorColors {
            foreground: "#282a36".into(),
            background: "#f8f8f2".into(),
        },
        selection: SelectionColors {
            foreground: None,
            background: "#44475a".into(),
        },
    }
}

pub fn nord() -> Theme {
    Theme {
        name: "nord".into(),
        colors: ThemeColors {
            foreground: "#d8dee9".into(),
            background: "#2e3440".into(),
            ansi: [
                "#3b4252".into(),
                "#bf616a".into(),
                "#a3be8c".into(),
                "#ebcb8b".into(),
                "#81a1c1".into(),
                "#b48ead".into(),
                "#88c0d0".into(),
                "#e5e9f0".into(),
                "#4c566a".into(),
                "#bf616a".into(),
                "#a3be8c".into(),
                "#ebcb8b".into(),
                "#81a1c1".into(),
                "#b48ead".into(),
                "#8fbcbb".into(),
                "#eceff4".into(),
            ],
        },
        cursor: CursorColors {
            foreground: "#2e3440".into(),
            background: "#d8dee9".into(),
        },
        selection: SelectionColors {
            foreground: None,
            background: "#434c5e".into(),
        },
    }
}

pub fn solarized_dark() -> Theme {
    Theme {
        name: "solarized-dark".into(),
        colors: ThemeColors {
            foreground: "#839496".into(),
            background: "#002b36".into(),
            ansi: [
                "#073642".into(),
                "#dc322f".into(),
                "#859900".into(),
                "#b58900".into(),
                "#268bd2".into(),
                "#d33682".into(),
                "#2aa198".into(),
                "#eee8d5".into(),
                "#002b36".into(),
                "#cb4b16".into(),
                "#586e75".into(),
                "#657b83".into(),
                "#839496".into(),
                "#6c71c4".into(),
                "#93a1a1".into(),
                "#fdf6e3".into(),
            ],
        },
        cursor: CursorColors {
            foreground: "#002b36".into(),
            background: "#839496".into(),
        },
        selection: SelectionColors {
            foreground: None,
            background: "#073642".into(),
        },
    }
}

pub fn one_dark() -> Theme {
    Theme {
        name: "one-dark".into(),
        colors: ThemeColors {
            foreground: "#abb2bf".into(),
            background: "#282c34".into(),
            ansi: [
                "#3f4451".into(),
                "#e06c75".into(),
                "#98c379".into(),
                "#e5c07b".into(),
                "#61afef".into(),
                "#c678dd".into(),
                "#56b6c2".into(),
                "#abb2bf".into(),
                "#4f5666".into(),
                "#be5046".into(),
                "#7a9f60".into(),
                "#d19a66".into(),
                "#3b84c0".into(),
                "#9a52af".into(),
                "#3c909b".into(),
                "#828997".into(),
            ],
        },
        cursor: CursorColors {
            foreground: "#282c34".into(),
            background: "#528bff".into(),
        },
        selection: SelectionColors {
            foreground: None,
            background: "#3e4452".into(),
        },
    }
}

pub fn gruvbox_dark() -> Theme {
    Theme {
        name: "gruvbox-dark".into(),
        colors: ThemeColors {
            foreground: "#ebdbb2".into(), // light1
            background: "#282828".into(), // dark0
            ansi: [
                // normal:  black,       red,         green,       yellow,      blue,        purple,      aqua,        white
                "#282828".into(),
                "#cc241d".into(),
                "#98971a".into(),
                "#d79921".into(),
                "#458588".into(),
                "#b16286".into(),
                "#689d6a".into(),
                "#a89984".into(),
                // bright:  black,       red,         green,       yellow,      blue,        purple,      aqua,        white
                "#928374".into(),
                "#fb4934".into(),
                "#b8bb26".into(),
                "#fabd2f".into(),
                "#83a598".into(),
                "#d3869b".into(),
                "#8ec07c".into(),
                "#ebdbb2".into(),
            ],
        },
        cursor: CursorColors {
            foreground: "#282828".into(),
            background: "#ebdbb2".into(),
        },
        selection: SelectionColors {
            foreground: None,
            background: "#504945".into(), // dark2
        },
    }
}

// ---------------------------------------------------------------------------
// Color utilities
// ---------------------------------------------------------------------------

/// Parse a CSS hex color (#RGB, #RRGGBB, #RRGGBBAA) to [f32; 4].
pub fn hex_to_rgba(hex: &str) -> [f32; 4] {
    let hex = hex.trim_start_matches('#');
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[1..2], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[2..3], 16).unwrap_or(0);
            [
                (r * 17) as f32 / 255.0,
                (g * 17) as f32 / 255.0,
                (b * 17) as f32 / 255.0,
                1.0,
            ]
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
            [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
            let a = u8::from_str_radix(&hex[6..8], 16).unwrap_or(255);
            [
                r as f32 / 255.0,
                g as f32 / 255.0,
                b as f32 / 255.0,
                a as f32 / 255.0,
            ]
        }
        _ => [1.0, 0.0, 1.0, 1.0], // magenta fallback
    }
}

fn darken(color: [f32; 4], factor: f32) -> [f32; 4] {
    [
        (color[0] * factor).clamp(0.0, 1.0),
        (color[1] * factor).clamp(0.0, 1.0),
        (color[2] * factor).clamp(0.0, 1.0),
        color[3],
    ]
}

fn lighten(color: [f32; 4], factor: f32) -> [f32; 4] {
    [
        (color[0] * factor).clamp(0.0, 1.0),
        (color[1] * factor).clamp(0.0, 1.0),
        (color[2] * factor).clamp(0.0, 1.0),
        color[3],
    ]
}
