//! Tunable look & layout for the ZenOS shell — colors, sizes, magnification,
//! dock contents, keybind codes, decoration metrics, text styling.
//!
//! This is the "knobs" file: change a number/color here, not in `backend.rs`.
//! GLSL shader sources live in `shaders.rs`.

// --- Colors -------------------------------------------------------------
/// Background clear color (matches the old wgpu clear).
pub const CLEAR: [f32; 4] = [0.08, 0.08, 0.08, 1.0];
// Slightly translucent UI (fake glass; true backdrop blur is a later pass).
pub const BAR_COLOR: [f32; 4] = [0.16, 0.16, 0.18, 0.70];
// macOS-style dock: very transparent neutral body (blur dominates, not a milky
// tint) + a barely-there hairline rim (not a bright outline).
pub const DOCK_COLOR: [f32; 4] = [0.86, 0.87, 0.91, 0.12];
pub const DOCK_BORDER_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.18];
pub const DOCK_BORDER_W: f32 = 1.0;
/// Thin vertical separator between dock app groups.
pub const SEP_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.20];

// --- Layout -------------------------------------------------------------
pub const BAR_H: i32 = 30;
pub const DOCK_H: i32 = 64;
pub const DOCK_MARGIN: i32 = 14; // gap from the bottom of the screen
pub const ICON_SIZE: i32 = 50;
pub const ICON_GAP: i32 = 10;
pub const DOCK_PAD_X: i32 = 14; // dock side padding (left of first icon)
pub const DOCK_PAD_Y: i32 = (DOCK_H - ICON_SIZE) / 2;

// --- Magnification ------------------------------------------------------
/// Hover magnification (macOS-style): icon under the cursor scales up to MAG_MAX,
/// falling off over MAG_RADIUS px. Icons grow upward from the dock baseline.
pub const MAG_MAX: f32 = 1.45;
pub const MAG_RADIUS: f32 = 110.0;
/// Icon corner radius as a fraction of icon size (squircle-ish mask).
pub const ICON_RADIUS_FRAC: f32 = 0.23;

// --- Power menu (top-left bar button + dropdown) ------------------------
pub const POWER_BTN_X: i32 = 6;
pub const POWER_BTN_W: i32 = 40;
pub const POWER_BTN_BG: [f32; 4] = [1.0, 1.0, 1.0, 0.10];
pub const POWER_BTN_RADIUS: f32 = 8.0;
/// Glyph drawn in the button. Falls back to a bare pill if the font lacks it.
pub const POWER_GLYPH: &str = "\u{23FB}"; // ⏻ power symbol
pub const POWER_GLYPH_PX: f32 = 18.0;
pub const POWER_GLYPH_COLOR: [f32; 4] = [0.92, 0.92, 0.92, 1.0];
/// Dropdown items, top to bottom. Index drives the action in the click handler.
pub const POWER_ITEMS: [&str; 2] = ["Restart", "Shut Down"];
pub const MENU_X: i32 = 6;
pub const MENU_GAP: i32 = 4; // gap below the bar
pub const MENU_W: i32 = 168;
pub const MENU_ITEM_H: i32 = 26;
pub const MENU_PAD: i32 = 4;
pub const MENU_RADIUS: f32 = 10.0;
pub const MENU_ITEM_RADIUS: f32 = 6.0;
pub const MENU_BG: [f32; 4] = [1.0, 1.0, 1.0, 0.98]; // white panel
pub const MENU_HOVER: [f32; 4] = [0.0, 0.533, 1.0, 1.0]; // rgb(0,136,255) selection blue
pub const MENU_TEXT: [f32; 4] = [0.09, 0.09, 0.09, 1.0]; // rgb(23,23,23)
pub const MENU_TEXT_HOVER: [f32; 4] = [1.0, 1.0, 1.0, 1.0]; // white on selection
pub const MENU_ITEM_PX: f32 = 13.0;

// --- Window manipulation ------------------------------------------------
/// Grab band (px) inside a window's edges that starts an interactive resize.
pub const RESIZE_BORDER: i32 = 8;
/// Minimum interactive size, so a window can't be shrunk to nothing.
pub const WIN_MIN_W: i32 = 120;
pub const WIN_MIN_H: i32 = 80;

// --- Radii / cursor -----------------------------------------------------
pub const BAR_RADIUS: f32 = 0.0;
pub const DOCK_RADIUS: f32 = 20.0;
pub const CURSOR_SIZE: i32 = 12;
pub const CURSOR_COLOR: [f32; 4] = [0.92, 0.92, 0.92, 1.0];

// --- Dock contents ------------------------------------------------------
/// Dock width hugs its content (macOS-style), not a fixed bar.
pub fn dock_width(n: usize) -> i32 {
    let n = n as i32;
    if n == 0 {
        return 2 * DOCK_PAD_X;
    }
    2 * DOCK_PAD_X + n * ICON_SIZE + (n - 1) * ICON_GAP
}

/// A dock entry: the binary to spawn on click + candidate icon paths (first that
/// exists wins; none -> a colored placeholder square using `placeholder`).
pub struct DockApp {
    pub exec: &'static str,
    /// Icon PNG embedded in the binary (works regardless of CWD/install).
    pub icon: &'static [u8],
    pub placeholder: [f32; 4],
    /// Draw a group separator immediately before this icon.
    pub sep_before: bool,
}
pub const DOCK_APPS: &[DockApp] = &[
    DockApp {
        exec: "thunar",
        icon: include_bytes!("../assets/icons/Finder.png"),
        placeholder: [0.20, 0.55, 0.95, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "firefox",
        icon: include_bytes!("../assets/icons/Safari.png"),
        placeholder: [0.20, 0.55, 0.95, 0.9],
        sep_before: true,
    },
    DockApp {
        exec: "gnome-calendar",
        icon: include_bytes!("../assets/icons/Calendar.png"),
        placeholder: [0.90, 0.30, 0.25, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "gnome-text-editor",
        icon: include_bytes!("../assets/icons/Notes.png"),
        placeholder: [0.95, 0.80, 0.25, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/Maps.png"),
        placeholder: [0.45, 0.75, 0.40, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "gnome-calculator",
        icon: include_bytes!("../assets/icons/Calculator.png"),
        placeholder: [0.55, 0.58, 0.66, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/Settings.png"),
        placeholder: [0.55, 0.58, 0.66, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/App Store.png"),
        placeholder: [0.20, 0.55, 0.95, 0.9],
        sep_before: false,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/Terminal.png"),
        placeholder: [0.20, 0.22, 0.28, 0.9],
        sep_before: true,
    },
    DockApp {
        exec: "foot",
        icon: include_bytes!("../assets/icons/Trash Full.png"),
        placeholder: [0.55, 0.58, 0.66, 0.9],
        sep_before: true,
    },
];

// --- Keybinds -----------------------------------------------------------
/// xkb keycodes (evdev + 8). smithay's Keycode is xkb-space.
pub const RAW_KEY_ESC: u32 = 9; // evdev KEY_ESC 1 + XKB offset
pub const RAW_KEY_F1: u32 = 67; // evdev KEY_F1 59 + XKB offset
pub const RAW_KEY_F2: u32 = 68; // evdev KEY_F2 60 + XKB offset
/// Left mouse button (evdev BTN_LEFT).
pub const BTN_LEFT: u32 = 0x110;

// --- Server-side decorations (macOS-style) ------------------------------
/// Titlebar height in px. Drawn above each toplevel's surface.
pub const TITLEBAR_H: i32 = 28;
pub const TITLEBAR_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 1.0]; // full white
pub const TITLEBAR_RADIUS: f32 = 10.0;
/// Traffic-light buttons (close/min/max), left-aligned.
pub const LIGHT_DIA: i32 = 13;
pub const LIGHT_MARGIN: i32 = 12; // left padding to the first light
pub const LIGHT_SPACING: i32 = 20; // distance between light left-edges
pub const LIGHT_CLOSE: [f32; 4] = [1.0, 0.37, 0.34, 1.0]; // #FF5F57
pub const LIGHT_MIN: [f32; 4] = [1.0, 0.74, 0.18, 1.0]; // #FEBC2E
pub const LIGHT_MAX: [f32; 4] = [0.16, 0.78, 0.25, 1.0]; // #28C840

// --- Text styling -------------------------------------------------------
pub const BAR_TEXT_PX: f32 = 18.0;
pub const BAR_TEXT_COLOR: [f32; 4] = [0.92, 0.92, 0.92, 1.0];
pub const TITLE_PX: f32 = 16.0;
pub const TITLE_COLOR: [f32; 4] = [0.15, 0.15, 0.16, 1.0];
