//! User settings: appearance, behaviour and keybindings.
//!
//! Settings are stored as TOML in `~/.config/sagenote/settings.toml` and are
//! written atomically (temp file + rename within the destination directory).
//! A missing, unreadable or partially-valid file falls back to defaults so a
//! corrupted settings file never blocks startup.
//!
//! The keybinding model follows Jupyter: an *edit mode* (a cell editor has
//! keyboard focus) and a *command mode* (no editor focused, one cell
//! selected).  Bindings are stored as human-readable strings, e.g.
//! `"shift+enter"`, `"ctrl+shift+s"`, or the two-press sequence `"d d"`, and
//! can be re-recorded from the settings dialog.  An empty string disables a
//! binding.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};

use sagenote::platform::home_dir;

// ---------------------------------------------------------------------------
// Bundled typefaces (IBM Plex, the Carbon typeface)
// ---------------------------------------------------------------------------

/// Family names for the bundled faces.  Proportional and Monospace are egui's
/// built-in families, whose first font we replace with IBM Plex.
pub const PLEX_SANS_MEDIUM: &str = "IBM Plex Sans Medium";
pub const PLEX_SANS_SEMIBOLD: &str = "IBM Plex Sans SemiBold";

/// Install IBM Plex as egui's default font set.
///
/// egui's built-in proportional font is Ubuntu-Light (300), which looks thin
/// and washed out at Carbon's 14 px body size.  Bundling Plex makes the UI
/// identical on Linux and macOS, independent of what fonts the system has.
/// The built-in emoji fallback is kept after Plex in both families.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "plex_sans_text".into(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/IBMPlexSans-Text.ttf"
        ))),
    );
    fonts.font_data.insert(
        "plex_sans_regular".into(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/IBMPlexSans-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "plex_sans_medium".into(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/IBMPlexSans-Medium.ttf"
        ))),
    );
    fonts.font_data.insert(
        "plex_sans_semibold".into(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/IBMPlexSans-SemiBold.ttf"
        ))),
    );
    fonts.font_data.insert(
        "plex_mono_regular".into(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/IBMPlexMono-Regular.ttf"
        ))),
    );

    // Highest priority, keeping egui's emoji/symbol fallbacks after it.
    // `Plex Sans Text` is the optical size designed for UI copy; Regular is
    // kept as a fallback, then egui's emoji fonts.
    let proportional = fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default();
    proportional.insert(0, "plex_sans_text".into());
    proportional.insert(1, "plex_sans_regular".into());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "plex_mono_regular".into());
    fonts.families.insert(
        egui::FontFamily::Name(PLEX_SANS_MEDIUM.into()),
        vec!["plex_sans_medium".into()],
    );
    fonts.families.insert(
        egui::FontFamily::Name(PLEX_SANS_SEMIBOLD.into()),
        vec!["plex_sans_semibold".into()],
    );

    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------------------
// Binding strings
// ---------------------------------------------------------------------------

/// A single key press with modifiers, as captured from egui input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyPress {
    pub key: egui::Key,
    pub modifiers: egui::Modifiers,
}

/// Parse a binding string such as `"ctrl+shift+s"` or `"d d"` into presses.
///
/// Returns `None` for the empty string (unbound) and for anything that does
/// not parse; callers fall back to the default binding in that case.
pub fn parse_binding(text: &str) -> Option<Vec<KeyPress>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut presses = Vec::new();
    for press in text.split_whitespace() {
        presses.push(parse_press(press)?);
    }
    Some(presses)
}

/// Format presses back into their canonical binding string.
pub fn format_binding(presses: &[KeyPress]) -> String {
    presses
        .iter()
        .map(|press| format_press(*press))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pretty, user-facing rendering of a binding string, e.g. `D D`,
/// `Ctrl+Enter`, `Shift+Enter`.
pub fn binding_display(binding: &str) -> String {
    match parse_binding(binding) {
        Some(presses) if !presses.is_empty() => presses
            .iter()
            .map(|press| display_press(*press))
            .collect::<Vec<_>>()
            .join(" "),
        _ => "—".to_owned(),
    }
}

fn parse_press(text: &str) -> Option<KeyPress> {
    let mut modifiers = egui::Modifiers::default();
    let mut parts = text.split('+');
    let key_name = loop {
        let part = parts.next()?;
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" | "option" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "cmd" | "command" | "meta" => modifiers.command = true,
            other => break other.to_ascii_lowercase(),
        }
    };
    if parts.next().is_some() {
        return None;
    }
    let key = key_from_name(&key_name)?;
    Some(KeyPress { key, modifiers })
}

fn format_press(press: KeyPress) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if press.modifiers.ctrl {
        parts.push("ctrl");
    }
    if press.modifiers.alt {
        parts.push("alt");
    }
    if press.modifiers.shift {
        parts.push("shift");
    }
    if press.modifiers.command || press.modifiers.mac_cmd {
        parts.push("cmd");
    }
    parts.push(key_name(press.key));
    parts.join("+")
}

fn display_press(press: KeyPress) -> String {
    let mut parts: Vec<String> = Vec::new();
    if press.modifiers.ctrl {
        parts.push("Ctrl".into());
    }
    if press.modifiers.alt {
        parts.push("Alt".into());
    }
    if press.modifiers.shift {
        parts.push("Shift".into());
    }
    if press.modifiers.command || press.modifiers.mac_cmd {
        // COMMAND is ⌘ on macOS and Ctrl elsewhere.
        parts.push(if cfg!(target_os = "macos") {
            "Cmd".into()
        } else {
            "Ctrl".into()
        });
    }
    parts.push(display_key(press.key));
    parts.join("+")
}

fn display_key(key: egui::Key) -> String {
    match key {
        egui::Key::ArrowUp => "↑".into(),
        egui::Key::ArrowDown => "↓".into(),
        egui::Key::ArrowLeft => "←".into(),
        egui::Key::ArrowRight => "→".into(),
        egui::Key::Escape => "Esc".into(),
        egui::Key::Enter => "Enter".into(),
        egui::Key::Space => "Space".into(),
        egui::Key::Backspace => "Backspace".into(),
        egui::Key::Tab => "Tab".into(),
        other => {
            let name = key_name(other);
            if name.len() == 1 {
                name.to_uppercase()
            } else {
                name.to_owned()
            }
        }
    }
}

/// Every key that can appear in a binding, with its canonical lowercase name.
const KEY_TABLE: &[(&str, egui::Key)] = &[
    ("arrowdown", egui::Key::ArrowDown),
    ("arrowleft", egui::Key::ArrowLeft),
    ("arrowright", egui::Key::ArrowRight),
    ("arrowup", egui::Key::ArrowUp),
    ("backslash", egui::Key::Backslash),
    ("backspace", egui::Key::Backspace),
    ("backtick", egui::Key::Backtick),
    ("closebracket", egui::Key::CloseBracket),
    ("closecurlybracket", egui::Key::CloseCurlyBracket),
    ("colon", egui::Key::Colon),
    ("comma", egui::Key::Comma),
    ("copy", egui::Key::Copy),
    ("cut", egui::Key::Cut),
    ("delete", egui::Key::Delete),
    ("end", egui::Key::End),
    ("enter", egui::Key::Enter),
    ("equals", egui::Key::Equals),
    ("escape", egui::Key::Escape),
    ("exclamationmark", egui::Key::Exclamationmark),
    ("home", egui::Key::Home),
    ("insert", egui::Key::Insert),
    ("minus", egui::Key::Minus),
    ("opencurlybracket", egui::Key::OpenCurlyBracket),
    ("openbracket", egui::Key::OpenBracket),
    ("pagedown", egui::Key::PageDown),
    ("pageup", egui::Key::PageUp),
    ("paste", egui::Key::Paste),
    ("period", egui::Key::Period),
    ("pipe", egui::Key::Pipe),
    ("plus", egui::Key::Plus),
    ("questionmark", egui::Key::Questionmark),
    ("quote", egui::Key::Quote),
    ("semicolon", egui::Key::Semicolon),
    ("slash", egui::Key::Slash),
    ("space", egui::Key::Space),
    ("tab", egui::Key::Tab),
    // Digits and letters are generated below; they are one character long.
    ("0", egui::Key::Num0),
    ("1", egui::Key::Num1),
    ("2", egui::Key::Num2),
    ("3", egui::Key::Num3),
    ("4", egui::Key::Num4),
    ("5", egui::Key::Num5),
    ("6", egui::Key::Num6),
    ("7", egui::Key::Num7),
    ("8", egui::Key::Num8),
    ("9", egui::Key::Num9),
    ("a", egui::Key::A),
    ("b", egui::Key::B),
    ("c", egui::Key::C),
    ("d", egui::Key::D),
    ("e", egui::Key::E),
    ("f", egui::Key::F),
    ("g", egui::Key::G),
    ("h", egui::Key::H),
    ("i", egui::Key::I),
    ("j", egui::Key::J),
    ("k", egui::Key::K),
    ("l", egui::Key::L),
    ("m", egui::Key::M),
    ("n", egui::Key::N),
    ("o", egui::Key::O),
    ("p", egui::Key::P),
    ("q", egui::Key::Q),
    ("r", egui::Key::R),
    ("s", egui::Key::S),
    ("t", egui::Key::T),
    ("u", egui::Key::U),
    ("v", egui::Key::V),
    ("w", egui::Key::W),
    ("x", egui::Key::X),
    ("y", egui::Key::Y),
    ("z", egui::Key::Z),
];

fn key_name(key: egui::Key) -> &'static str {
    KEY_TABLE
        .iter()
        .find(|(_, candidate)| *candidate == key)
        .map(|(name, _)| *name)
        .unwrap_or("unknown")
}

fn key_from_name(name: &str) -> Option<egui::Key> {
    KEY_TABLE
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, key)| *key)
}

// ---------------------------------------------------------------------------
// Keybindings
// ---------------------------------------------------------------------------

/// Remappable bindings, keyed by action id (see [`BINDING_META`]).
///
/// Stored as a map of action id → binding string.  An id absent from the map
/// uses the Jupyter default; an empty string disables the binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Keybindings(pub BTreeMap<String, String>);

/// One row of the settings dialog's shortcut table (and the shortcuts help).
pub struct BindingMeta {
    /// Action id; the key in [`Keybindings`].
    pub id: &'static str,
    /// Group heading in the settings dialog.
    pub group: &'static str,
    /// Human-readable action label.
    pub label: &'static str,
}

/// All remappable actions, in display order, grouped as in the settings
/// dialog.  The `General` group works in both modes; `Running cells` bindings
/// work in edit and command mode; the rest are mode-specific.
pub const BINDING_META: &[BindingMeta] = &[
    BindingMeta {
        id: "run_and_advance",
        group: "Running cells",
        label: "Run cell and select the next cell",
    },
    BindingMeta {
        id: "run_in_place",
        group: "Running cells",
        label: "Run cell in place",
    },
    BindingMeta {
        id: "run_and_insert_below",
        group: "Running cells",
        label: "Run cell and insert a cell below",
    },
    BindingMeta {
        id: "command_mode",
        group: "Edit mode",
        label: "Leave the editor (switch to command mode)",
    },
    BindingMeta {
        id: "complete",
        group: "Edit mode",
        label: "Complete the name at the caret",
    },
    BindingMeta {
        id: "show_docs",
        group: "Edit mode",
        label: "Show documentation for the name at the caret",
    },
    BindingMeta {
        id: "completion_next",
        group: "Completion",
        label: "Select the next completion candidate",
    },
    BindingMeta {
        id: "completion_prev",
        group: "Completion",
        label: "Select the previous completion candidate",
    },
    BindingMeta {
        id: "save",
        group: "General",
        label: "Save notebook",
    },
    BindingMeta {
        id: "save_as",
        group: "General",
        label: "Save notebook as…",
    },
    BindingMeta {
        id: "new_notebook",
        group: "General",
        label: "New notebook",
    },
    BindingMeta {
        id: "open_notebook",
        group: "General",
        label: "Open notebook",
    },
    BindingMeta {
        id: "open_settings",
        group: "General",
        label: "Open settings",
    },
    BindingMeta {
        id: "show_shortcuts",
        group: "General",
        label: "Show keyboard shortcuts",
    },
    BindingMeta {
        id: "toggle_inspector",
        group: "General",
        label: "Toggle the variables panel",
    },
    BindingMeta {
        id: "interrupt",
        group: "General",
        label: "Interrupt Sage",
    },
    BindingMeta {
        id: "restart_sage",
        group: "General",
        label: "Restart Sage",
    },
    BindingMeta {
        id: "quit",
        group: "General",
        label: "Quit",
    },
    BindingMeta {
        id: "edit_selected",
        group: "Command mode",
        label: "Enter the editor",
    },
    BindingMeta {
        id: "select_prev",
        group: "Command mode",
        label: "Select the previous cell",
    },
    BindingMeta {
        id: "select_next",
        group: "Command mode",
        label: "Select the next cell",
    },
    BindingMeta {
        id: "insert_above",
        group: "Command mode",
        label: "Insert a cell above",
    },
    BindingMeta {
        id: "insert_below",
        group: "Command mode",
        label: "Insert a cell below",
    },
    BindingMeta {
        id: "delete_cell",
        group: "Command mode",
        label: "Delete the selected cell (press twice)",
    },
    BindingMeta {
        id: "undo_delete",
        group: "Command mode",
        label: "Undo the last cell deletion",
    },
    BindingMeta {
        id: "cut_cell",
        group: "Command mode",
        label: "Cut the selected cell",
    },
    BindingMeta {
        id: "copy_cell",
        group: "Command mode",
        label: "Copy the selected cell",
    },
    BindingMeta {
        id: "paste_below",
        group: "Command mode",
        label: "Paste the cell below",
    },
    BindingMeta {
        id: "paste_above",
        group: "Command mode",
        label: "Paste the cell above",
    },
    BindingMeta {
        id: "interrupt_kernel",
        group: "Command mode",
        label: "Interrupt Sage (press twice)",
    },
    BindingMeta {
        id: "restart_kernel",
        group: "Command mode",
        label: "Restart Sage (press twice)",
    },
    BindingMeta {
        id: "show_shortcuts_command",
        group: "Command mode",
        label: "Show keyboard shortcuts",
    },
];

/// The Jupyter-style default for one action id.  `"cmd"` is ⌘ on macOS and
/// Ctrl elsewhere; `"ctrl"` is the literal Control key, matching Jupyter.
pub fn default_binding(id: &str) -> &'static str {
    match id {
        // General
        "save" => "cmd+s",
        "save_as" => "cmd+shift+s",
        "new_notebook" => "cmd+n",
        "open_notebook" => "cmd+o",
        "quit" => "cmd+q",
        "open_settings" => "cmd+comma",
        "toggle_inspector" => "cmd+i",
        "interrupt" => "cmd+period",
        "restart_sage" => "",
        "show_shortcuts" => "cmd+h",
        // Running cells
        "run_and_advance" => "shift+enter",
        "run_in_place" => "ctrl+enter",
        "run_and_insert_below" => "alt+enter",
        // Edit mode
        "command_mode" => "escape",
        "complete" => "tab",
        "show_docs" => "ctrl+space",
        "completion_next" => "ctrl+n",
        "completion_prev" => "ctrl+p",
        // Command mode
        "edit_selected" => "enter",
        "select_prev" => "arrowup",
        "select_next" => "arrowdown",
        "insert_above" => "a",
        "insert_below" => "b",
        "delete_cell" => "d d",
        "undo_delete" => "z",
        "cut_cell" => "x",
        "copy_cell" => "c",
        "paste_below" => "v",
        "paste_above" => "shift+v",
        "interrupt_kernel" => "i i",
        "restart_kernel" => "0 0",
        "show_shortcuts_command" => "h",
        _ => "",
    }
}

impl Keybindings {
    /// The full set of Jupyter-style defaults.
    pub fn jupyter() -> Self {
        let mut map = BTreeMap::new();
        for meta in BINDING_META {
            map.insert(meta.id.to_owned(), default_binding(meta.id).to_owned());
        }
        Self(map)
    }

    /// The effective binding for `id`: the user's value if valid (or
    /// deliberately unbound), else the Jupyter default.
    pub fn get(&self, id: &str) -> &str {
        if let Some(value) = self.0.get(id) {
            let trimmed = value.trim();
            if trimmed.is_empty() || parse_binding(value).is_some() {
                return value;
            }
        }
        default_binding(id)
    }

    /// Set (override) the binding for `id`.
    pub fn set(&mut self, id: &str, value: impl Into<String>) {
        self.0.insert(id.to_owned(), value.into());
    }
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::jupyter()
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// UI colour scheme.  `System` follows the OS preference.  The default is
/// Light: Carbon's light palette is the reference implementation and is the
/// clearest starting point for a notebook.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    System,
    #[default]
    Light,
    Dark,
}

// ---------------------------------------------------------------------------
// Carbon Design System palette
// ---------------------------------------------------------------------------
//
// Tokens from IBM Carbon v11.  Keeping them in one module is what keeps the
// app consistent: panels, buttons, badges and output text are drawn from the
// same scale instead of ad-hoc greys.  Carbon is deliberately square (0
// corner radius) and uses 8 px spacing, which is applied in `Settings::apply`.

/// Raw Carbon grey/blue/red/green/yellow tokens.
pub mod carbon {
    use eframe::egui::Color32;

    pub const WHITE: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
    pub const GRAY_10: Color32 = Color32::from_rgb(0xf4, 0xf4, 0xf4);
    pub const GRAY_20: Color32 = Color32::from_rgb(0xe0, 0xe0, 0xe0);
    pub const GRAY_30: Color32 = Color32::from_rgb(0xc6, 0xc6, 0xc6);
    pub const GRAY_40: Color32 = Color32::from_rgb(0xa8, 0xa8, 0xa8);
    pub const GRAY_50: Color32 = Color32::from_rgb(0x8d, 0x8d, 0x8d);
    pub const GRAY_60: Color32 = Color32::from_rgb(0x6f, 0x6f, 0x6f);
    pub const GRAY_70: Color32 = Color32::from_rgb(0x52, 0x52, 0x52);
    pub const GRAY_80: Color32 = Color32::from_rgb(0x39, 0x39, 0x39);
    pub const GRAY_90: Color32 = Color32::from_rgb(0x26, 0x26, 0x26);
    pub const GRAY_100: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);

    pub const BLUE_20: Color32 = Color32::from_rgb(0xd0, 0xe2, 0xff);
    pub const BLUE_30: Color32 = Color32::from_rgb(0xa6, 0xc8, 0xff);
    pub const BLUE_40: Color32 = Color32::from_rgb(0x78, 0xa9, 0xff);
    pub const BLUE_50: Color32 = Color32::from_rgb(0x45, 0x89, 0xff);
    pub const BLUE_60: Color32 = Color32::from_rgb(0x0f, 0x62, 0xfe);
    pub const BLUE_70: Color32 = Color32::from_rgb(0x00, 0x43, 0xce);
    pub const BLUE_80: Color32 = Color32::from_rgb(0x00, 0x2d, 0x9c);

    pub const RED_10: Color32 = Color32::from_rgb(0xff, 0xf1, 0xf1);
    pub const RED_50: Color32 = Color32::from_rgb(0xfa, 0x4d, 0x56);
    pub const RED_60: Color32 = Color32::from_rgb(0xda, 0x1e, 0x28);

    pub const GREEN_10: Color32 = Color32::from_rgb(0xde, 0xfb, 0xe6);
    pub const GREEN_40: Color32 = Color32::from_rgb(0x42, 0xbe, 0x65);
    pub const GREEN_60: Color32 = Color32::from_rgb(0x19, 0x80, 0x38);

    pub const YELLOW_10: Color32 = Color32::from_rgb(0xfc, 0xf4, 0xd6);
    pub const YELLOW_30: Color32 = Color32::from_rgb(0xf1, 0xc2, 0x1b);
    pub const YELLOW_70: Color32 = Color32::from_rgb(0x68, 0x4e, 0x00);
}

/// Semantic Carbon colours for one theme.  UI code reads these instead of
/// hard-coding hex values, so a token change is one edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarbonPalette {
    pub dark_mode: bool,
    pub bg: eframe::egui::Color32,
    pub layer: eframe::egui::Color32,
    pub field: eframe::egui::Color32,
    pub border_subtle: eframe::egui::Color32,
    pub border_strong: eframe::egui::Color32,
    pub text_primary: eframe::egui::Color32,
    pub text_secondary: eframe::egui::Color32,
    pub text_helper: eframe::egui::Color32,
    pub interactive: eframe::egui::Color32,
    pub interactive_hover: eframe::egui::Color32,
    pub interactive_active: eframe::egui::Color32,
    pub selected: eframe::egui::Color32,
    pub danger: eframe::egui::Color32,
    pub danger_bg: eframe::egui::Color32,
    pub success: eframe::egui::Color32,
    pub success_bg: eframe::egui::Color32,
    pub warning: eframe::egui::Color32,
    pub warning_bg: eframe::egui::Color32,
    pub output_bg: eframe::egui::Color32,
    pub button_bg: eframe::egui::Color32,
    pub button_text: eframe::egui::Color32,
    pub button_border: eframe::egui::Color32,
    pub button_hover_bg: eframe::egui::Color32,
    pub button_hover_border: eframe::egui::Color32,
    pub button_active_bg: eframe::egui::Color32,
}

impl CarbonPalette {
    pub fn light() -> Self {
        use carbon::*;
        Self {
            dark_mode: false,
            bg: WHITE,
            layer: GRAY_10,
            field: WHITE,
            border_subtle: GRAY_20,
            border_strong: GRAY_50,
            text_primary: GRAY_100,
            text_secondary: GRAY_70,
            text_helper: GRAY_60,
            interactive: BLUE_60,
            interactive_hover: BLUE_70,
            interactive_active: BLUE_80,
            selected: BLUE_20,
            danger: RED_60,
            danger_bg: RED_10,
            success: GREEN_60,
            success_bg: GREEN_10,
            warning: YELLOW_70,
            warning_bg: YELLOW_10,
            output_bg: GRAY_10,
            button_bg: WHITE,
            button_text: GRAY_100,
            button_border: GRAY_40,
            button_hover_bg: GRAY_10,
            button_hover_border: GRAY_60,
            button_active_bg: GRAY_20,
        }
    }

    pub fn dark() -> Self {
        use carbon::*;
        Self {
            dark_mode: true,
            bg: GRAY_100,
            layer: GRAY_90,
            field: GRAY_90,
            border_subtle: GRAY_80,
            border_strong: GRAY_60,
            text_primary: GRAY_10,
            text_secondary: GRAY_30,
            text_helper: GRAY_40,
            interactive: BLUE_50,
            interactive_hover: BLUE_40,
            interactive_active: BLUE_30,
            selected: BLUE_80,
            danger: RED_50,
            danger_bg: eframe::egui::Color32::from_rgb(0x2d, 0x07, 0x09),
            success: GREEN_40,
            success_bg: eframe::egui::Color32::from_rgb(0x0e, 0x2b, 0x1e),
            warning: YELLOW_30,
            warning_bg: eframe::egui::Color32::from_rgb(0x2e, 0x24, 0x00),
            output_bg: GRAY_90,
            button_bg: GRAY_90,
            button_text: GRAY_10,
            button_border: GRAY_60,
            button_hover_bg: GRAY_80,
            button_hover_border: GRAY_40,
            button_active_bg: GRAY_70,
        }
    }

    /// Pick the palette that matches the UI's current theme.
    pub fn of(ui: &eframe::egui::Ui) -> Self {
        if ui.visuals().dark_mode {
            Self::dark()
        } else {
            Self::light()
        }
    }
}

/// Carbon-styled [`eframe::egui::Visuals`]: square corners, hairline borders,
/// a white (or Gray 100) canvas, and Carbon focus/selection colours.
pub fn carbon_visuals(dark: bool) -> eframe::egui::Visuals {
    use eframe::egui::{
        epaint::Shadow,
        style::{Selection, WidgetVisuals},
        Color32, CornerRadius, Stroke, Visuals,
    };

    let p = if dark {
        CarbonPalette::dark()
    } else {
        CarbonPalette::light()
    };
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.dark_mode = dark;
    v.override_text_color = None;
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.window_stroke = Stroke::new(1.0_f32, p.border_subtle);
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.layer;
    v.code_bg_color = p.layer;
    v.hyperlink_color = p.interactive;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.selection = Selection {
        bg_fill: p.selected,
        stroke: Stroke::new(2.0_f32, p.interactive),
    };
    v.text_cursor.stroke = Stroke::new(2.0_f32, p.interactive);
    let alpha = if dark { 120 } else { 36 };
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(alpha),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(alpha),
    };
    v.widgets.noninteractive = WidgetVisuals {
        bg_fill: p.bg,
        weak_bg_fill: p.bg,
        bg_stroke: Stroke::NONE,
        corner_radius: CornerRadius::ZERO,
        fg_stroke: Stroke::new(1.0_f32, p.text_primary),
        expansion: 0.0,
    };
    v.widgets.inactive = WidgetVisuals {
        bg_fill: p.button_bg,
        weak_bg_fill: p.button_bg,
        bg_stroke: Stroke::new(1.0_f32, p.button_border),
        corner_radius: CornerRadius::ZERO,
        fg_stroke: Stroke::new(1.0_f32, p.button_text),
        expansion: 0.0,
    };
    v.widgets.hovered = WidgetVisuals {
        bg_fill: p.button_hover_bg,
        weak_bg_fill: p.button_hover_bg,
        bg_stroke: Stroke::new(1.0_f32, p.button_hover_border),
        corner_radius: CornerRadius::ZERO,
        fg_stroke: Stroke::new(1.0_f32, p.text_primary),
        expansion: 0.0,
    };
    v.widgets.active = WidgetVisuals {
        bg_fill: p.button_active_bg,
        weak_bg_fill: p.button_active_bg,
        bg_stroke: Stroke::new(1.0_f32, p.interactive),
        corner_radius: CornerRadius::ZERO,
        fg_stroke: Stroke::new(1.0_f32, p.text_primary),
        expansion: 0.0,
    };
    v.widgets.open = v.widgets.active;
    v
}

/// User settings.  See the module docs for the storage location and format.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    /// Proportional UI font size in points.
    pub ui_font_size: f32,
    /// Monospace (code) font size in points.
    pub code_font_size: f32,
    /// Inspector auto-refresh interval in seconds; 0 disables auto-refresh.
    pub inspector_refresh_secs: u64,
    pub keybindings: Keybindings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Light,
            // Carbon's productive scale, nudged up for 1x displays where
            // egui's grayscale, unhinted AA is softest.
            ui_font_size: 15.0,
            code_font_size: 14.0,
            inspector_refresh_secs: 2,
            keybindings: Keybindings::default(),
        }
    }
}

impl Settings {
    /// Apply theme, font sizes and spacing to an egui context.  Safe to call
    /// repeatedly (e.g. while dragging a settings slider).
    pub fn apply(&self, ctx: &egui::Context) {
        ctx.set_theme(match self.theme {
            ThemeMode::System => egui::ThemePreference::System,
            ThemeMode::Light => egui::ThemePreference::Light,
            ThemeMode::Dark => egui::ThemePreference::Dark,
        });
        let mut style = (*ctx.style()).clone();
        apply_typography(self, &mut style);
        ctx.set_style(style);
    }
}

/// Carbon's productive type scale and 8 px grid, independent of the palette.
/// Body/Small use Plex Regular, buttons Plex Medium (clearer at small sizes)
/// and headings Plex SemiBold.
fn apply_typography(settings: &Settings, style: &mut egui::Style) {
    let medium = egui::FontFamily::Name(PLEX_SANS_MEDIUM.into());
    let semibold = egui::FontFamily::Name(PLEX_SANS_SEMIBOLD.into());
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(settings.ui_font_size, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(settings.ui_font_size.max(10.0), medium),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(settings.code_font_size, egui::FontFamily::Monospace),
    );
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(settings.ui_font_size + 6.0, semibold),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(
            (settings.ui_font_size - 2.0).max(10.0),
            egui::FontFamily::Proportional,
        ),
    );
    // Carbon's 8 px grid: roomy item spacing, 32 px controls, square corners.
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.interact_size = egui::vec2(40.0, 32.0);
}

/// Like [`Settings::apply`], but also installs the Carbon palette (square
/// widgets, hairline borders, Carbon focus/selection) for **both** the light
/// and dark styles, so a system theme switch while the app is running keeps
/// the Carbon look instead of falling back to egui's defaults.
pub fn apply_with_palette(settings: &Settings, ctx: &egui::Context) {
    settings.apply(ctx);
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let mut style = (*ctx.style_of(theme)).clone();
        style.visuals = carbon_visuals(theme == egui::Theme::Dark);
        apply_typography(settings, &mut style);
        ctx.set_style_of(theme, style);
    }
}

/// `~/.config/sagenote/settings.toml` on both targets, matching
/// `config.toml` (deliberately not `dirs::config_dir()` on macOS).
pub fn settings_path() -> PathBuf {
    home_dir()
        .join(".config")
        .join("sagenote")
        .join("settings.toml")
}

/// Load settings from disk, falling back to defaults for a missing,
/// unreadable or unparseable file.
pub fn load_settings() -> Settings {
    let Ok(text) = fs::read_to_string(settings_path()) else {
        return Settings::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

/// Persist settings atomically: temp file in the destination directory,
/// fsync, rename.
pub fn save_settings(settings: &Settings) -> Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(settings).context("serialising settings.toml")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("toml.tmp-{}-{}", std::process::id(), nonce));
    let result = (|| -> Result<()> {
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_jupyter() {
        let kb = Keybindings::default();
        assert_eq!(kb.get("run_and_advance"), "shift+enter");
        assert_eq!(kb.get("run_in_place"), "ctrl+enter");
        assert_eq!(kb.get("run_and_insert_below"), "alt+enter");
        assert_eq!(kb.get("command_mode"), "escape");
        assert_eq!(kb.get("complete"), "tab");
        assert_eq!(kb.get("show_docs"), "ctrl+space");
        assert_eq!(kb.get("completion_next"), "ctrl+n");
        assert_eq!(kb.get("completion_prev"), "ctrl+p");
        assert_eq!(kb.get("edit_selected"), "enter");
        assert_eq!(kb.get("insert_below"), "b");
        assert_eq!(kb.get("insert_above"), "a");
        assert_eq!(kb.get("delete_cell"), "d d");
        assert_eq!(kb.get("undo_delete"), "z");
        assert_eq!(kb.get("cut_cell"), "x");
        assert_eq!(kb.get("copy_cell"), "c");
        assert_eq!(kb.get("paste_below"), "v");
        assert_eq!(kb.get("paste_above"), "shift+v");
        assert_eq!(kb.get("interrupt_kernel"), "i i");
        assert_eq!(kb.get("restart_kernel"), "0 0");
        assert_eq!(kb.get("select_prev"), "arrowup");
        assert_eq!(kb.get("select_next"), "arrowdown");
        assert_eq!(kb.get("restart_sage"), "");
    }

    #[test]
    fn every_meta_id_has_a_default() {
        for meta in BINDING_META {
            // A missing default would silently disable the action.
            assert!(
                !default_binding(meta.id).is_empty() || meta.id == "restart_sage",
                "no default binding for {}",
                meta.id
            );
            assert_eq!(
                Keybindings::default().get(meta.id),
                default_binding(meta.id),
                "default map disagrees with default_binding for {}",
                meta.id
            );
        }
    }

    #[test]
    fn parse_and_format_roundtrip() {
        for binding in [
            "shift+enter",
            "ctrl+enter",
            "alt+enter",
            "ctrl+shift+s",
            "cmd+period",
            "escape",
            "enter",
            "arrowup",
            "a",
            "d d",
            "i i",
            "0 0",
        ] {
            let parsed =
                parse_binding(binding).unwrap_or_else(|| panic!("could not parse {binding}"));
            assert_eq!(format_binding(&parsed), binding, "roundtrip failed");
        }
    }

    #[test]
    fn unbound_and_garbage_fall_back() {
        assert!(parse_binding("").is_none());
        assert!(parse_binding("not a key").is_none());
        let mut kb = Keybindings::default();
        kb.set("insert_below", "");
        assert_eq!(kb.get("insert_below"), "");
        kb.set("insert_below", "garbage key!");
        // Invalid override falls back to the default rather than disabling.
        assert_eq!(kb.get("insert_below"), "b");
    }

    #[test]
    fn plex_fonts_are_registered_and_render() {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label(
                    egui::RichText::new("Sage")
                        .font(egui::FontId::new(14.0, egui::FontFamily::Proportional)),
                );
                ui.label(egui::RichText::new("Run").font(egui::FontId::new(
                    14.0,
                    egui::FontFamily::Name(PLEX_SANS_MEDIUM.into()),
                )));
                ui.label(egui::RichText::new("Cell").font(egui::FontId::new(
                    20.0,
                    egui::FontFamily::Name(PLEX_SANS_SEMIBOLD.into()),
                )));
                ui.monospace("a = 6*7");
            });
        });
    }

    #[test]
    fn toml_roundtrip_preserves_overrides() {
        let mut settings = Settings {
            ui_font_size: 18.0,
            code_font_size: 15.25,
            theme: ThemeMode::Dark,
            ..Settings::default()
        };
        settings.keybindings.set("insert_below", "ctrl+b");
        settings.keybindings.set("quit", "");
        let text = toml::to_string_pretty(&settings).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.ui_font_size, 18.0);
        assert_eq!(back.code_font_size, 15.25);
        assert_eq!(back.theme, ThemeMode::Dark);
        assert_eq!(back.keybindings.get("insert_below"), "ctrl+b");
        assert_eq!(back.keybindings.get("quit"), "");
        // Unset ids keep their defaults after a roundtrip.
        assert_eq!(back.keybindings.get("run_and_advance"), "shift+enter");
    }

    #[test]
    fn partial_toml_uses_field_defaults() {
        let settings: Settings = toml::from_str("ui_font_size = 20.0").unwrap();
        assert_eq!(settings.ui_font_size, 20.0);
        assert_eq!(settings.inspector_refresh_secs, 2);
        assert_eq!(settings.keybindings.get("insert_below"), "b");
    }

    #[test]
    fn binding_display_is_pretty() {
        assert_eq!(binding_display("d d"), "D D");
        assert_eq!(binding_display("shift+enter"), "Shift+Enter");
        assert_eq!(binding_display("ctrl+shift+s"), "Ctrl+Shift+S");
        assert_eq!(binding_display("arrowup"), "↑");
        assert_eq!(binding_display(""), "—");
    }
}
