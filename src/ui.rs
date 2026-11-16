//! Cell workspace, menu/status bars, the variable inspector, the syntect-based
//! Rust-native editor, and the output panel (RaTeX/SVG/PNG + text fallbacks).
//!
//! Why not `egui_code_editor`?  The current release (0.4.2) pins egui 0.36
//! while this app uses eframe/egui 0.31.1, and it ships its own highlighter
//! rather than accepting a caller-supplied `syntect::SyntaxSet`.  The spec
//! explicitly allows the fallback: `TextEdit::multiline` plus a syntect
//! layouter.  That also makes Shift+Enter interception straightforward.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::text::{CCursor, CCursorRange};
use eframe::egui::{
    self, Color32, FontId, Key, Modifiers, Response, RichText, Stroke, TextStyle, Ui,
};
use sagenote::backend::{CellId, DisplayMode, MimeBundle, MimeType, StreamName, VarInfo};
use sagenote::cells::{Cell, CellState, Notebook};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::app::{CellRuntime, LiveOutput};
use crate::render::Renderer;
use crate::settings::CarbonPalette;
use crate::text::strip_ansi;

/// Carbon semi-bold for headings/labels that need hierarchy.  egui's
/// `strong()` only changes colour, which is invisible in the light palette.
pub(crate) fn semibold(size: f32) -> egui::FontId {
    egui::FontId::new(
        size,
        egui::FontFamily::Name(crate::settings::PLEX_SANS_SEMIBOLD.into()),
    )
}

/// Carbon's primary action button: Blue 60 fill, white label, square.
pub(crate) fn primary_button(ui: &mut Ui, label: impl Into<egui::WidgetText>) -> Response {
    let p = CarbonPalette::of(ui);
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.weak_bg_fill = p.interactive;
        widgets.inactive.bg_fill = p.interactive;
        widgets.inactive.bg_stroke = Stroke::NONE;
        widgets.inactive.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        widgets.hovered.weak_bg_fill = p.interactive_hover;
        widgets.hovered.bg_fill = p.interactive_hover;
        widgets.hovered.bg_stroke = Stroke::NONE;
        widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        widgets.active.weak_bg_fill = p.interactive_active;
        widgets.active.bg_fill = p.interactive_active;
        widgets.active.bg_stroke = Stroke::NONE;
        widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        ui.add(egui::Button::new(label))
    })
    .inner
}

/// Carbon danger button for destructive actions (interrupt).
pub(crate) fn danger_button(ui: &mut Ui, label: impl Into<egui::WidgetText>) -> Response {
    let p = CarbonPalette::of(ui);
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.weak_bg_fill = p.danger_bg;
        widgets.inactive.bg_fill = p.danger_bg;
        widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.danger);
        widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.danger);
        widgets.hovered.weak_bg_fill = p.danger;
        widgets.hovered.bg_fill = p.danger;
        widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.danger);
        widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        widgets.active.weak_bg_fill = p.danger;
        widgets.active.bg_fill = p.danger;
        widgets.active.bg_stroke = Stroke::new(1.0_f32, p.danger);
        widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        ui.add(egui::Button::new(label))
    })
    .inner
}

/// Consume a single-press binding (e.g. "shift+enter") from this frame's
/// input.  Returns true if the binding fired.
pub(crate) fn consume_binding(ui: &mut Ui, binding: &str) -> bool {
    let Some(presses) = crate::settings::parse_binding(binding) else {
        return false;
    };
    let Some(press) = presses.first() else {
        return false;
    };
    ui.input_mut(|input| input.consume_key(press.modifiers, press.key))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    New,
    Open,
    Save,
    SaveAs,
    Quit,
    AddCell(Option<CellId>),
    DeleteCell(CellId),
    MoveUp(CellId),
    MoveDown(CellId),
    Run(CellId),
    RunAndAdvance(CellId),
    RunAndInsertBelow(CellId),
    /// Tab was pressed in a cell editor: ask the driver for completions for
    /// the token ending at `offset` (in characters) and starting at `start`.
    RequestCompletion {
        cell: CellId,
        source: String,
        offset: usize,
        start: usize,
    },
    /// Ctrl+Space (or the configured binding): ask the driver for the docstring
    /// of the name ending at `offset`.
    RequestDocs {
        cell: CellId,
        source: String,
        offset: usize,
    },
    RunAll,
    RunAbove(CellId),
    ShowSettings,
    ShowShortcuts,
    ToCommandMode(CellId),
    Interrupt,
    RestartSage,
    ToggleInspector,
    RefreshInspector,
    ShowAbout,
}

pub struct MenuState {
    pub backend_ready: bool,
    pub starting: bool,
    pub running: bool,
    pub can_run_above: bool,
    pub run_above_id: Option<CellId>,
    pub can_restart: bool,
    pub inspector_open: bool,
    pub can_refresh_inspector: bool,
}

pub struct StatusState {
    pub starting: bool,
    pub backend_alive: bool,
    pub sage_path: Option<PathBuf>,
    pub sage_version: String,
    pub notebook_path: Option<PathBuf>,
    pub notebook_title: String,
    pub dirty: bool,
    pub cells: usize,
    pub running: Option<String>,
}

pub struct InspectorState<'a> {
    pub vars: &'a [VarInfo],
    pub loading: bool,
    pub error: Option<&'a str>,
    pub backend_ready: bool,
    pub starting: bool,
    pub blocked_running: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectorAction {
    Refresh,
    Close,
}

pub struct NotebookUi<'a> {
    pub runtime: &'a mut HashMap<CellId, CellRuntime>,
    pub focus_request: &'a mut Option<CellId>,
    pub last_focused: &'a mut Option<CellId>,
    pub backend_ready: bool,
    pub syntax: &'a SyntaxAssets,
    pub renderer: &'a mut Renderer,
    pub actions: &'a mut Vec<UiAction>,
    pub bindings: &'a crate::settings::Keybindings,
    /// Jupyter-style selection highlight (command mode).
    pub selected: Option<CellId>,
    /// Cell to scroll into view this frame.
    pub scroll_to_cell: &'a mut Option<CellId>,
}

/// Syntax definitions and themes loaded once at startup.  The fallback editor
/// uses these for every cell.  Both a light (InspiredGitHub) and a dark
/// (base16-ocean.dark) theme are kept so switching the UI theme recolours the
/// code instead of leaving dark text on a dark canvas.
pub struct SyntaxAssets {
    syntaxes: SyntaxSet,
    light_theme: Theme,
    dark_theme: Theme,
    /// Memoised shaped text so resize/repaint does not re-run syntect
    /// highlighting every frame.  Keyed by source text, wrap width, font and
    /// theme.
    galley_cache: RefCell<HashMap<u64, Arc<egui::Galley>>>,
}

impl SyntaxAssets {
    pub fn new() -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let light_theme = themes
            .themes
            .get("InspiredGitHub")
            .or_else(|| themes.themes.get("base16-ocean.light"))
            .or_else(|| themes.themes.values().next())
            .cloned()
            .unwrap_or_default();
        let dark_theme = themes
            .themes
            .get("base16-ocean.dark")
            .or_else(|| themes.themes.get("Solarized (dark)"))
            .or_else(|| themes.themes.get("Monokai"))
            .cloned()
            .unwrap_or_else(|| light_theme.clone());
        Self {
            syntaxes,
            light_theme,
            dark_theme,
            galley_cache: RefCell::new(HashMap::new()),
        }
    }

    fn layout(&self, text: &str, ui: &Ui, wrap_width: f32) -> egui::text::LayoutJob {
        let syntax = self
            .syntaxes
            .find_syntax_by_extension("py")
            .or_else(|| self.syntaxes.find_syntax_by_name("Sage"))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let font = ui
            .style()
            .text_styles
            .get(&TextStyle::Monospace)
            .cloned()
            .unwrap_or_else(|| FontId::monospace(13.5));
        let p = CarbonPalette::of(ui);
        let fallback_format = egui::text::TextFormat {
            font_id: font.clone(),
            color: p.text_primary,
            ..Default::default()
        };
        let theme = if p.dark_mode {
            &self.dark_theme
        } else {
            &self.light_theme
        };

        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = if wrap_width.is_finite() && wrap_width > 0.0 {
            wrap_width
        } else {
            f32::INFINITY
        };

        let mut highlighter = HighlightLines::new(syntax, theme);
        for line in LinesWithEndings::from(text) {
            match highlighter.highlight_line(line, &self.syntaxes) {
                Ok(ranges) => {
                    for (style, segment) in ranges {
                        if segment.is_empty() {
                            continue;
                        }
                        let color = Color32::from_rgba_unmultiplied(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                            style.foreground.a,
                        );
                        job.append(
                            segment,
                            0.0,
                            egui::text::TextFormat {
                                font_id: font.clone(),
                                color,
                                ..Default::default()
                            },
                        );
                    }
                }
                Err(_) => job.append(line, 0.0, fallback_format.clone()),
            }
        }
        job
    }

    /// Shape `text` with syntax highlighting, memoised on
    /// `(text, wrap_width, font)` so a repaint or resize only pays for the
    /// cells whose wrap width actually changed.
    pub fn layout_galley(&self, ui: &Ui, text: &str, wrap_width: f32) -> Arc<egui::Galley> {
        let font = ui
            .style()
            .text_styles
            .get(&TextStyle::Monospace)
            .cloned()
            .unwrap_or_else(|| FontId::monospace(13.5));
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        text.hash(&mut hasher);
        hasher.write_u32(wrap_width.to_bits());
        hasher.write_u32(font.size.to_bits());
        font.family.hash(&mut hasher);
        hasher.write_u8(u8::from(ui.visuals().dark_mode));
        let key = hasher.finish();

        if let Some(galley) = self.galley_cache.borrow().get(&key) {
            return galley.clone();
        }
        let job = self.layout(text, ui, wrap_width);
        let galley: Arc<egui::Galley> = ui.fonts(|fonts| fonts.layout_job(job));
        let mut cache = self.galley_cache.borrow_mut();
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(key, galley.clone());
        galley
    }
}

/// Menus and quick actions.  All actions are returned to the app, which is the
/// only writer of notebook/backend state.
pub fn menu_bar(ctx: &egui::Context, state: &MenuState) -> Vec<UiAction> {
    let mut actions = Vec::new();
    let p = if ctx.theme() == egui::Theme::Dark {
        CarbonPalette::dark()
    } else {
        CarbonPalette::light()
    };
    egui::TopBottomPanel::top("sagenote-menubar")
        .frame(
            egui::Frame::default()
                .fill(p.bg)
                .stroke(Stroke::new(1.0_f32, p.border_subtle))
                .inner_margin(egui::Margin::symmetric(12, 4)),
        )
        .show(ctx, |ui| {
            // Carbon navigation is text-first: no boxes around menu labels.
            // Hover still paints a Gray 10 row so the menus remain discoverable.
            {
                let widgets = &mut ui.visuals_mut().widgets;
                widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
                widgets.inactive.bg_stroke = Stroke::NONE;
                widgets.hovered.bg_stroke = Stroke::NONE;
                widgets.active.bg_stroke = Stroke::NONE;
            }
            egui::menu::bar(ui, |ui| {
                ui.label(
                    RichText::new("sagenote")
                        .font(semibold(15.0))
                        .color(p.text_primary),
                );
                ui.add_space(4.0);
                ui.menu_button("File", |ui| {
                    menu_item(
                        ui,
                        "New",
                        Some("Ctrl/Cmd N"),
                        true,
                        UiAction::New,
                        &mut actions,
                    );
                    menu_item(
                        ui,
                        "Open…",
                        Some("Ctrl/Cmd O"),
                        true,
                        UiAction::Open,
                        &mut actions,
                    );
                    menu_item(
                        ui,
                        "Save",
                        Some("Ctrl/Cmd S"),
                        true,
                        UiAction::Save,
                        &mut actions,
                    );
                    menu_item(
                        ui,
                        "Save As…",
                        Some("Ctrl/Cmd Shift S"),
                        true,
                        UiAction::SaveAs,
                        &mut actions,
                    );
                    ui.separator();
                    menu_item(
                        ui,
                        "Quit",
                        Some("Ctrl/Cmd Q"),
                        true,
                        UiAction::Quit,
                        &mut actions,
                    );
                });
                ui.menu_button("Notebook", |ui| {
                    menu_item(
                        ui,
                        "Run All",
                        None,
                        state.backend_ready,
                        UiAction::RunAll,
                        &mut actions,
                    );
                    if let Some(id) = state.run_above_id {
                        menu_item(
                            ui,
                            "Run Above",
                            None,
                            state.can_run_above && state.backend_ready,
                            UiAction::RunAbove(id),
                            &mut actions,
                        );
                    } else {
                        menu_item(ui, "Run Above", None, false, UiAction::RunAll, &mut actions);
                    }
                    menu_item(
                        ui,
                        "Interrupt",
                        Some("Ctrl/Cmd ."),
                        state.running,
                        UiAction::Interrupt,
                        &mut actions,
                    );
                    ui.separator();
                    menu_item(
                        ui,
                        "Restart Sage",
                        None,
                        state.can_restart,
                        UiAction::RestartSage,
                        &mut actions,
                    );
                });
                ui.menu_button("View", |ui| {
                    let label = if state.inspector_open {
                        "✓ Inspector"
                    } else {
                        "Inspector"
                    };
                    menu_item(
                        ui,
                        label,
                        Some("Ctrl/Cmd I"),
                        true,
                        UiAction::ToggleInspector,
                        &mut actions,
                    );
                    menu_item(
                        ui,
                        "Refresh Variables",
                        None,
                        state.inspector_open && state.can_refresh_inspector,
                        UiAction::RefreshInspector,
                        &mut actions,
                    );
                });
                ui.menu_button("Help", |ui| {
                    menu_item(
                        ui,
                        "Keyboard Shortcuts…",
                        None,
                        true,
                        UiAction::ShowShortcuts,
                        &mut actions,
                    );
                    menu_item(
                        ui,
                        "About sagenote",
                        None,
                        true,
                        UiAction::ShowAbout,
                        &mut actions,
                    );
                });
                ui.menu_button("Settings", |ui| {
                    menu_item(
                        ui,
                        "Settings…",
                        Some("Ctrl/Cmd ,"),
                        true,
                        UiAction::ShowSettings,
                        &mut actions,
                    );
                });

                ui.separator();
                if state.starting {
                    ui.spinner();
                    ui.label(RichText::new("Starting Sage…").small().weak());
                } else {
                    ui.add_enabled_ui(state.backend_ready, |ui| {
                        if primary_button(ui, "Run All")
                            .on_hover_text("Evaluate every non-empty cell")
                            .clicked()
                        {
                            actions.push(UiAction::RunAll);
                        }
                    });
                    ui.add_enabled_ui(state.running, |ui| {
                        if danger_button(ui, "Interrupt")
                            .on_hover_text("SIGINT the Sage process group")
                            .clicked()
                        {
                            actions.push(UiAction::Interrupt);
                        }
                    });
                }
            });
        });
    actions
}

fn menu_item(
    ui: &mut Ui,
    label: &str,
    shortcut: Option<&str>,
    enabled: bool,
    action: UiAction,
    actions: &mut Vec<UiAction>,
) {
    let mut button = egui::Button::new(label);
    if let Some(shortcut) = shortcut {
        button = button.shortcut_text(shortcut);
    }
    if ui.add_enabled(enabled, button).clicked() {
        actions.push(action);
        ui.close_menu();
    }
}

pub fn status_bar(ctx: &egui::Context, state: &StatusState) {
    let p = if ctx.theme() == egui::Theme::Dark {
        CarbonPalette::dark()
    } else {
        CarbonPalette::light()
    };
    egui::TopBottomPanel::bottom("sagenote-status")
        .frame(
            egui::Frame::default()
                .fill(p.layer)
                .stroke(Stroke::new(1.0_f32, p.border_subtle))
                .inner_margin(egui::Margin::symmetric(12, 4)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if state.starting {
                    ui.spinner();
                    ui.label(RichText::new("Starting Sage…").small());
                } else if state.backend_alive {
                    ui.label(
                        RichText::new(format!("Sage {}", state.sage_version))
                            .small()
                            .strong()
                            .color(p.text_primary),
                    );
                    if let Some(path) = &state.sage_path {
                        let text = display_path(path, 48);
                        ui.separator();
                        ui.add(egui::Label::new(RichText::new(text).small().weak()).truncate())
                            .on_hover_text(path.display().to_string());
                    }
                } else {
                    ui.label(RichText::new("Sage unavailable").small().color(p.danger));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if state.dirty {
                        ui.label(RichText::new("• unsaved").small().color(p.warning));
                    } else if state.notebook_path.is_none() {
                        ui.label(RichText::new("not saved").small().weak());
                    } else {
                        ui.label(RichText::new("saved").small().weak());
                    }
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{} cell(s)", state.cells))
                            .small()
                            .weak(),
                    );
                    ui.separator();
                    if let Some(running) = &state.running {
                        ui.label(RichText::new(running).small().color(p.interactive));
                        ui.separator();
                    }
                    let title = state
                        .notebook_path
                        .as_ref()
                        .map(|path| display_path(path, 52))
                        .unwrap_or_else(|| state.notebook_title.clone());
                    ui.add(egui::Label::new(RichText::new(title).small()).truncate())
                        .on_hover_text(
                            state
                                .notebook_path
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| "not saved yet".into()),
                        );
                });
            });
        });
}

pub fn inspector_panel(ctx: &egui::Context, state: &InspectorState<'_>) -> Option<InspectorAction> {
    let mut action = None;
    let p = if ctx.theme() == egui::Theme::Dark {
        CarbonPalette::dark()
    } else {
        CarbonPalette::light()
    };
    let can_refresh =
        state.backend_ready && !state.starting && !state.loading && !state.blocked_running;
    let disabled_reason = if state.starting {
        "Sage is starting"
    } else if !state.backend_ready {
        "Sage is not running"
    } else if state.loading {
        "Already reading variables"
    } else if state.blocked_running {
        "Paused while a cell runs"
    } else {
        "Refresh variables"
    };

    let frame = egui::Frame::default()
        .fill(p.layer)
        .stroke(Stroke::new(1.0_f32, p.border_subtle))
        .inner_margin(egui::Margin::same(12));
    egui::SidePanel::right("sagenote-inspector")
        .resizable(true)
        .default_width(276.0)
        .width_range(220.0..=420.0)
        .frame(frame)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Variables")
                        .font(semibold(15.0))
                        .color(p.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("×")
                        .on_hover_text("Close inspector")
                        .clicked()
                    {
                        action = Some(InspectorAction::Close);
                    }
                    let refresh = ui.add_enabled(can_refresh, egui::Button::new("↻").small());
                    let refresh = if can_refresh {
                        refresh.on_hover_text(disabled_reason)
                    } else {
                        refresh.on_disabled_hover_text(disabled_reason)
                    };
                    if refresh.clicked() {
                        action = Some(InspectorAction::Refresh);
                    }
                });
            });

            if state.vars.is_empty() {
                // Only states the user needs to act on are shown.  There is no
                // permanent "N variables · updated HH:MM:SS" status line: the
                // list itself is the answer, and the timestamp only made the
                // panel flicker on every auto-refresh.
                ui.add_space(8.0);
                if state.starting {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Starting Sage…").small().weak());
                    });
                } else if state.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Reading variables…").small().weak());
                    });
                } else if let Some(error) = state.error {
                    ui.add(egui::Label::new(RichText::new(error).small().color(p.danger)).wrap());
                } else if !state.backend_ready {
                    ui.label(RichText::new("Sage is not running").small().weak());
                } else if state.blocked_running {
                    ui.label(
                        RichText::new("Paused while a cell runs")
                            .small()
                            .color(p.warning),
                    );
                } else {
                    ui.label(
                        RichText::new("No user variables yet.")
                            .small()
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Assign a name in a cell; the driver hides Sage's own globals.",
                            )
                            .small()
                            .weak(),
                        )
                        .wrap(),
                    );
                }
                return;
            }

            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt("sagenote-inspector-vars")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for var in state.vars {
                        ui.push_id(var.name.as_str(), |ui| {
                            // One compact block per variable: name + type on
                            // one line, repr below.  No per-entry box and no
                            // separator; spacing alone keeps the list readable.
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(&var.name)
                                        .monospace()
                                        .strong()
                                        .color(p.text_primary),
                                );
                                ui.label(
                                    RichText::new(&var.type_name).small().color(p.text_helper),
                                );
                            });
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&var.value)
                                        .monospace()
                                        .small()
                                        .color(p.text_secondary),
                                )
                                .wrap(),
                            );
                        });
                        ui.add_space(10.0);
                    }
                });
        });
    action
}

/// State for the toolbar row.
pub struct ToolbarState {
    pub backend_ready: bool,
    pub running: bool,
}

/// A JupyterLab-style toolbar row: quick actions with tooltips.
/// Returns actions for the app to dispatch.
pub fn toolbar(ctx: &egui::Context, state: &ToolbarState) -> Vec<UiAction> {
    let mut actions = Vec::new();
    let p = if ctx.theme() == egui::Theme::Dark {
        CarbonPalette::dark()
    } else {
        CarbonPalette::light()
    };
    egui::TopBottomPanel::top("sagenote-toolbar")
        .frame(
            egui::Frame::default()
                .fill(p.layer)
                .stroke(Stroke::new(1.0_f32, p.border_subtle))
                .inner_margin(egui::Margin::symmetric(12, 6)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button("+ Cell")
                    .on_hover_text("Insert a cell below (B)")
                    .clicked()
                {
                    actions.push(UiAction::AddCell(None));
                }
                ui.add_enabled_ui(state.backend_ready, |ui| {
                    if primary_button(ui, "Run All")
                        .on_hover_text("Evaluate every non-empty cell")
                        .clicked()
                    {
                        actions.push(UiAction::RunAll);
                    }
                });
                ui.add_enabled_ui(state.running, |ui| {
                    if danger_button(ui, "Interrupt")
                        .on_hover_text("SIGINT the Sage process group")
                        .clicked()
                    {
                        actions.push(UiAction::Interrupt);
                    }
                });
                if ui
                    .button("↻ Restart Sage")
                    .on_hover_text("Restart the Sage process")
                    .clicked()
                {
                    actions.push(UiAction::RestartSage);
                }
            });
        });
    actions
}

/// Draw the cells.  Returns true if any source text changed.
pub fn notebook_ui(ui: &mut Ui, notebook: &mut Notebook, state: NotebookUi<'_>) -> bool {
    let NotebookUi {
        runtime,
        focus_request,
        last_focused,
        backend_ready,
        syntax,
        renderer,
        actions,
        bindings,
        selected,
        scroll_to_cell,
    } = state;

    if notebook.cells.is_empty() {
        ui.label(RichText::new("No cells yet. Add one to start.").weak());
        return false;
    }

    let total = notebook.cells.len();
    let mut changed = false;
    for (index, cell) in notebook.cells.iter_mut().enumerate() {
        let rt = runtime.entry(cell.id).or_default();
        changed |= draw_cell(
            ui,
            cell,
            rt,
            index,
            total,
            backend_ready,
            syntax,
            renderer,
            focus_request,
            last_focused,
            actions,
            bindings,
            selected,
            scroll_to_cell,
        );
        if index + 1 < total {
            ui.add_space(10.0);
        }
    }
    changed
}

#[allow(clippy::too_many_arguments)]
fn draw_cell(
    ui: &mut Ui,
    cell: &mut Cell,
    rt: &mut CellRuntime,
    index: usize,
    total: usize,
    backend_ready: bool,
    syntax: &SyntaxAssets,
    renderer: &mut Renderer,
    focus_request: &mut Option<CellId>,
    last_focused: &mut Option<CellId>,
    actions: &mut Vec<UiAction>,
    bindings: &crate::settings::Keybindings,
    selected: Option<CellId>,
    scroll_to_cell: &mut Option<CellId>,
) -> bool {
    ui.push_id(cell.id, |ui| {
        let editor_id = egui::Id::new(("sagenote-cell-editor", cell.id.0));
        let p = CarbonPalette::of(ui);
        // Consume Shift+Enter before TextEdit sees it.  Only when this editor is
        // focused, so Enter elsewhere keeps its normal meaning.
        let focused = ui.memory(|memory| memory.has_focus(editor_id));
        let shift_enter =
            focused && ui.input_mut(|input| input.consume_key(Modifiers::SHIFT, Key::Enter));

        // Completion popup keys are consumed before the editor or the run
        // bindings, so arrows/Enter/Tab/Esc navigate the popup instead of
        // moving the caret or running the cell.
        if focused && rt.completion_popup.is_some() {
            handle_completion_keys(ui, rt, bindings);
        }

        // Jupyter binding consumption: only when this editor is focused so
        // Enter elsewhere keeps its normal meaning.
        if focused {
            if consume_binding(ui, bindings.get("run_and_advance")) {
                rt.completion_popup = None;
                actions.push(UiAction::RunAndAdvance(cell.id));
            } else if consume_binding(ui, bindings.get("run")) {
                rt.completion_popup = None;
                actions.push(UiAction::Run(cell.id));
            } else if consume_binding(ui, bindings.get("run_and_insert_below")) {
                rt.completion_popup = None;
                actions.push(UiAction::RunAndInsertBelow(cell.id));
            } else if ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape)) {
                // Escape leaves edit mode and enters command mode (Jupyter).
                actions.push(UiAction::ToCommandMode(cell.id));
            }
        }

        // Tab completion: only in edit mode, only when no request is already
        // in flight, and only when the popup is not open (the popup consumed
        // Tab as "accept" above).
        if focused
            && rt.completion_popup.is_none()
            && rt.completion_request.is_none()
            && consume_binding(ui, bindings.get("complete"))
        {
            let cursor = egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
                .and_then(|state| state.cursor.char_range())
                .filter(|range| range.primary.index == range.secondary.index)
                .map(|range| range.primary.index)
                .unwrap_or_else(|| cell.source.chars().count());
            let start = completion_token_start(&cell.source, cursor);
            if start < cursor {
                actions.push(UiAction::RequestCompletion {
                    cell: cell.id,
                    source: cell.source.clone(),
                    offset: cursor,
                    start,
                });
            }
        }

        // Documentation: Ctrl+Space by default.  It shares the editor-focus
        // gate with completion and closes an open candidate list first.
        if focused && rt.doc_request.is_none() && consume_binding(ui, bindings.get("show_docs")) {
            let cursor = egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
                .and_then(|state| state.cursor.char_range())
                .filter(|range| range.primary.index == range.secondary.index)
                .map(|range| range.primary.index)
                .unwrap_or_else(|| cell.source.chars().count());
            rt.completion_popup = None;
            actions.push(UiAction::RequestDocs {
                cell: cell.id,
                source: cell.source.clone(),
                offset: cursor,
            });
        }

        // Scroll this cell into view when requested (Jupyter `G`/`gg`).
        if scroll_to_cell.is_some_and(|target| target == cell.id) {
            ui.scroll_to_rect(ui.available_rect_before_wrap(), None);
            *scroll_to_cell = None;
        }

        // A completion chosen on a previous frame (single candidate, or a
        // click/Enter in the popup) is applied before the editor is laid out.
        let mut cursor_after: Option<usize> = None;
        let mut source_changed = false;
        if let Some((start, end, insert)) = rt.pending_completion.take() {
            let (new_source, new_cursor) = apply_completion(&cell.source, start, end, &insert);
            if new_source != cell.source {
                cell.source = new_source;
                cursor_after = Some(new_cursor);
                source_changed = true;
            }
        }

        let is_selected = selected == Some(cell.id);
        let border = if is_selected || rt.state == CellState::Running {
            p.interactive
        } else {
            p.border_subtle
        };
        let frame = egui::Frame::default()
            .fill(p.bg)
            .stroke(Stroke::new(1.0_f32, border))
            .corner_radius(0.0)
            .inner_margin(16.0);
        frame
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("Cell {}", index + 1))
                            .font(semibold(14.0))
                            .color(p.text_primary),
                    );
                    ui.add_space(4.0);
                    state_badge(ui, rt);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Delete cell").clicked() {
                            actions.push(UiAction::DeleteCell(cell.id));
                        }
                        if ui
                            .add_enabled(index + 1 < total, egui::Button::new("↓").small())
                            .on_hover_text("Move down")
                            .clicked()
                        {
                            actions.push(UiAction::MoveDown(cell.id));
                        }
                        if ui
                            .add_enabled(index > 0, egui::Button::new("↑").small())
                            .on_hover_text("Move up")
                            .clicked()
                        {
                            actions.push(UiAction::MoveUp(cell.id));
                        }
                        if ui
                            .small_button("+")
                            .on_hover_text("Insert a cell below")
                            .clicked()
                        {
                            actions.push(UiAction::AddCell(Some(cell.id)));
                        }
                        ui.add_enabled_ui(backend_ready, |ui| {
                            if primary_button(ui, "Run")
                                .on_hover_text("Evaluate this cell (Shift+Enter)")
                                .clicked()
                            {
                                actions.push(UiAction::Run(cell.id));
                            }
                        });
                    });
                });

                ui.add_space(6.0);
                let width = ui.available_width();
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    syntax.layout_galley(ui, text, wrap_width)
                };
                let mut output = egui::TextEdit::multiline(&mut cell.source)
                    .id(editor_id)
                    .code_editor()
                    .desired_rows(4)
                    .desired_width(width)
                    .background_color(p.field)
                    .layouter(&mut layouter)
                    .show(ui);
                if *focus_request == Some(cell.id) {
                    output.response.request_focus();
                    *focus_request = None;
                }
                if output.response.has_focus() {
                    *last_focused = Some(cell.id);
                }
                let changed = output.response.changed() || source_changed;
                if changed {
                    // The displayed value no longer corresponds to the source,
                    // and any in-flight run is superseded for routing purposes.
                    rt.mark_stale();
                    rt.run = None;
                    rt.state = CellState::Idle;
                    rt.started = None;
                    rt.elapsed = None;
                    cell.state = CellState::Idle;
                    rt.completion_popup = None;
                    rt.completion_request = None;
                    rt.doc_request = None;
                }
                if shift_enter {
                    rt.completion_popup = None;
                    actions.push(UiAction::Run(cell.id));
                }

                if let Some(index) = cursor_after {
                    output
                        .state
                        .cursor
                        .set_char_range(Some(CCursorRange::one(CCursor::new(index))));
                    output.state.store(ui.ctx(), editor_id);
                    output.response.request_focus();
                } else if rt.completion_popup.is_some() && output.response.has_focus() {
                    draw_completion_popup(ui, cell.id, rt, &output, focus_request);
                } else if !output.response.has_focus() {
                    rt.completion_popup = None;
                }

                if rt.has_output()
                    || rt.error.is_some()
                    || matches!(
                        rt.state,
                        CellState::Running | CellState::Queued | CellState::Interrupted
                    )
                {
                    ui.add_space(8.0);
                    draw_outputs(ui, rt, renderer);
                }
                changed
            })
            .inner
    })
    .inner
}

/// Consume keys the completion popup owns.  Returns true when a key was
/// consumed.  `Up`/`Down` move the selection, `Enter`/`Tab` accept, `Esc`
/// closes.  Accepting stages the replacement for the next frame, because the
/// caret can only be moved after the `TextEdit` has run.
fn handle_completion_keys(
    ui: &mut Ui,
    rt: &mut CellRuntime,
    bindings: &crate::settings::Keybindings,
) -> bool {
    let Some(popup) = rt.completion_popup.as_mut() else {
        return false;
    };
    let mut consumed = false;
    if consume_binding(ui, bindings.get("completion_next"))
        || ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowDown))
    {
        popup.selected = (popup.selected + 1) % popup.items.len();
        consumed = true;
    }
    if consume_binding(ui, bindings.get("completion_prev"))
        || ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowUp))
    {
        popup.selected = (popup.selected + popup.items.len() - 1) % popup.items.len();
        consumed = true;
    }
    let accept = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter))
        || consume_binding(ui, bindings.get("complete"));
    let cancel = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
    if accept {
        let item = popup.items[popup.selected].clone();
        rt.pending_completion = Some((popup.start, popup.end, item));
        rt.completion_popup = None;
        consumed = true;
    } else if cancel {
        rt.completion_popup = None;
        consumed = true;
    }
    consumed
}

/// Render the completion list just below the caret.  The list is a Carbon
/// popup: white (or Gray 90) surface, hairline border, square corners.
fn draw_completion_popup(
    ui: &mut Ui,
    cell: CellId,
    rt: &mut CellRuntime,
    output: &egui::widgets::text_edit::TextEditOutput,
    focus_request: &mut Option<CellId>,
) {
    let Some(mut popup) = rt.completion_popup.take() else {
        return;
    };
    let p = CarbonPalette::of(ui);
    let cursor = output
        .state
        .cursor
        .char_range()
        .map(|range| range.sorted()[1].index)
        .unwrap_or(0);
    let caret = output.galley.pos_from_ccursor(CCursor::new(cursor));
    let anchor = output.galley_pos + caret.left_bottom().to_vec2() + egui::vec2(0.0, 4.0);

    let mut chosen: Option<String> = None;
    egui::Area::new(egui::Id::new(("sagenote-completion", cell.0)))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .constrain(true)
        .show(ui.ctx(), |ui| {
            egui::Frame::default()
                .fill(p.bg)
                .stroke(Stroke::new(1.0_f32, p.border_strong))
                .corner_radius(0.0)
                .shadow(egui::epaint::Shadow {
                    offset: [0, 4],
                    blur: 12,
                    spread: 0,
                    color: Color32::from_black_alpha(if p.dark_mode { 120 } else { 36 }),
                })
                .inner_margin(2.0)
                .show(ui, |ui| {
                    // Compact, IDE-like rows: 22 px tall, tight padding,
                    // 13 px mono.  The global 32 px control height is too
                    // airy for a completion list.
                    ui.spacing_mut().button_padding = egui::vec2(6.0, 2.0);
                    ui.spacing_mut().interact_size = egui::vec2(0.0, 22.0);
                    ui.set_min_width(180.0);
                    ui.set_max_width(320.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("sagenote-completion-scroll", cell.0))
                        .max_height(180.0)
                        .show(ui, |ui| {
                            for (index, item) in popup.items.iter().enumerate() {
                                let selected = index == popup.selected;
                                let text = RichText::new(item)
                                    .monospace()
                                    .size(13.0)
                                    .color(p.text_primary);
                                let response = ui.add_sized(
                                    [ui.available_width(), 22.0],
                                    egui::SelectableLabel::new(selected, text),
                                );
                                if selected {
                                    response.scroll_to_me(Some(egui::Align::Center));
                                }
                                if response.clicked() {
                                    chosen = Some(item.clone());
                                }
                            }
                        });
                });
        });

    if let Some(item) = chosen {
        rt.pending_completion = Some((popup.start, popup.end, item));
        *focus_request = Some(cell);
    } else {
        popup.selected = popup.selected.min(popup.items.len().saturating_sub(1));
        rt.completion_popup = Some(popup);
    }
}

fn state_badge(ui: &mut Ui, rt: &CellRuntime) {
    let p = CarbonPalette::of(ui);
    let (label, bg, fg) = match rt.state {
        CellState::Idle => ("idle", p.layer, p.text_helper),
        CellState::Queued => ("queued", p.layer, p.interactive),
        CellState::Running => ("running", p.selected, p.interactive),
        CellState::Ok => ("ok", p.success_bg, p.success),
        CellState::Error => ("error", p.danger_bg, p.danger),
        CellState::Interrupted => ("interrupted", p.warning_bg, p.warning),
    };
    let elapsed = if rt.state == CellState::Running {
        rt.started.map(|started| started.elapsed())
    } else {
        rt.elapsed
    };
    let text = match elapsed {
        Some(elapsed) => format!("{label} · {}", format_duration(elapsed)),
        None => label.to_owned(),
    };
    egui::Frame::default()
        .fill(bg)
        .corner_radius(2.0)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(fg));
        });
}

fn draw_outputs(ui: &mut Ui, rt: &CellRuntime, renderer: &mut Renderer) {
    let p = CarbonPalette::of(ui);
    egui::Frame::default()
        .fill(p.output_bg)
        .stroke(Stroke::new(1.0_f32, p.border_subtle))
        .corner_radius(0.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if rt.state == CellState::Queued {
                ui.label(RichText::new("queued…").small().weak());
            }
            if rt.state == CellState::Running {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("running…").small().weak());
                    if let Some(elapsed) = rt.started.map(|started| started.elapsed()) {
                        ui.label(RichText::new(format_duration(elapsed)).small().weak());
                    }
                });
            }

            for (index, output) in rt.outputs.iter().enumerate() {
                if index > 0 {
                    ui.add_space(4.0);
                }
                draw_live_output(ui, output, renderer);
            }

            if let Some(error) = &rt.error {
                if !rt.outputs.is_empty() {
                    ui.add_space(4.0);
                }
                if rt.error_stale {
                    ui.horizontal_wrapped(|ui| {
                        stale_tag(ui);
                        ui.label(RichText::new("error").small().weak());
                    });
                }
                let color = if rt.error_stale {
                    p.text_helper
                } else {
                    p.danger
                };
                let error = strip_ansi(error);
                ui.add(
                    egui::Label::new(RichText::new(error.as_ref()).monospace().color(color)).wrap(),
                );
            }

            if rt.state == CellState::Interrupted && rt.error.is_none() && rt.outputs.is_empty() {
                ui.label(
                    RichText::new("interrupted (Sage process group reset)")
                        .small()
                        .color(p.warning),
                );
            }
        });
}

fn draw_live_output(ui: &mut Ui, output: &LiveOutput, renderer: &mut Renderer) {
    let p = CarbonPalette::of(ui);
    match output {
        LiveOutput::Stream { name, text, stale } => {
            let color = if *stale {
                p.text_helper
            } else if *name == StreamName::Stderr {
                p.warning
            } else {
                p.text_primary
            };
            ui.horizontal_wrapped(|ui| {
                if *name == StreamName::Stderr {
                    ui.label(RichText::new("stderr").small().color(p.warning));
                }
                if *stale {
                    stale_tag(ui);
                }
            });
            let text = strip_ansi(text);
            ui.add(egui::Label::new(RichText::new(text.as_ref()).monospace().color(color)).wrap());
        }
        LiveOutput::Mime {
            bundle,
            display,
            stale,
        } => {
            let color = if *stale {
                p.text_helper
            } else {
                p.text_primary
            };
            let Some((mime, data)) = preferred_mime(bundle) else {
                // Forward compatibility: a future backend can add a mimetype
                // we do not know yet.  Show it rather than silently dropping it.
                if let Some((mime, data)) = bundle.iter().next() {
                    ui.label(RichText::new(mime).small().weak());
                    let data = strip_ansi(data);
                    ui.add(
                        egui::Label::new(RichText::new(data.as_ref()).monospace().color(color))
                            .wrap(),
                    );
                }
                return;
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(mime.as_str()).small().weak());
                if *display == DisplayMode::Inline {
                    ui.label(RichText::new("inline").small().weak());
                }
                if *stale {
                    stale_tag(ui);
                }
            });
            match mime {
                MimeType::TextPlain => {
                    let data = strip_ansi(data);
                    ui.add(
                        egui::Label::new(RichText::new(data.as_ref()).monospace().color(color))
                            .wrap(),
                    );
                }
                MimeType::TextLatex => {
                    draw_latex(ui, renderer, data, *display, *stale, color);
                }
                MimeType::ImageSvg => {
                    draw_svg(ui, renderer, data, *display, *stale, color);
                }
                MimeType::ImagePng => {
                    draw_png(ui, renderer, data, *display, *stale);
                }
                MimeType::TextHtml => {
                    ui.label(
                        RichText::new("HTML output — not supported in this prototype")
                            .small()
                            .color(p.text_secondary),
                    );
                }
            }
        }
    }
}

fn add_visual(ui: &mut Ui, display: DisplayMode, image: egui::Image<'_>) {
    match display {
        DisplayMode::Inline => {
            ui.horizontal_wrapped(|ui| {
                ui.add(image);
            });
        }
        DisplayMode::Block => {
            ui.add(image);
        }
    }
}

fn fit_image_size(ui: &Ui, size: egui::Vec2) -> egui::Vec2 {
    let available = (ui.available_width() - 2.0).max(24.0);
    if size.x > available && size.x > 0.0 {
        let scale = available / size.x;
        egui::vec2(available, size.y * scale)
    } else {
        size
    }
}

fn draw_latex(
    ui: &mut Ui,
    renderer: &mut Renderer,
    latex: &str,
    display: DisplayMode,
    stale: bool,
    color: Color32,
) {
    let font_size = match display {
        DisplayMode::Block => 19.0,
        DisplayMode::Inline => 15.0,
    };
    let ppp = ui.ctx().pixels_per_point();
    match renderer.math(ui.ctx(), latex, display, color, font_size, ppp) {
        Ok(rendered) => {
            let size = fit_image_size(ui, rendered.size);
            let image = egui::Image::new(&rendered.texture).fit_to_exact_size(size);
            add_visual(ui, display, image);
        }
        Err(error) => {
            let p = CarbonPalette::of(ui);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("unrendered").small().color(p.danger))
                    .on_hover_text(error.to_string());
                ui.label(RichText::new("RaTeX fallback").small().weak());
            });
            let color = if stale { p.text_helper } else { color };
            ui.add(egui::Label::new(RichText::new(latex).monospace().color(color)).wrap());
        }
    }
}

fn draw_svg(
    ui: &mut Ui,
    renderer: &mut Renderer,
    data: &str,
    display: DisplayMode,
    stale: bool,
    color: Color32,
) {
    let p = CarbonPalette::of(ui);
    let ppp = ui.ctx().pixels_per_point();
    match renderer.svg(ui.ctx(), data, ppp) {
        Ok(rendered) => {
            let size = fit_image_size(ui, rendered.size);
            let mut image = egui::Image::new(&rendered.texture).fit_to_exact_size(size);
            if stale {
                image = image.tint(p.text_helper);
            }
            add_visual(ui, display, image);
        }
        Err(error) => {
            ui.label(
                RichText::new("SVG could not be rendered")
                    .small()
                    .color(p.danger),
            )
            .on_hover_text(error.to_string());
            let preview: String = data.chars().take(240).collect();
            let color = if stale { p.text_helper } else { color };
            ui.add(
                egui::Label::new(RichText::new(preview).monospace().small().color(color)).wrap(),
            );
        }
    }
}

fn draw_png(ui: &mut Ui, renderer: &mut Renderer, data: &str, display: DisplayMode, stale: bool) {
    let p = CarbonPalette::of(ui);
    let ppp = ui.ctx().pixels_per_point();
    match renderer.png(ui.ctx(), data, ppp) {
        Ok(rendered) => {
            let size = fit_image_size(ui, rendered.size);
            let mut image = egui::Image::new(&rendered.texture).fit_to_exact_size(size);
            if stale {
                image = image.tint(p.text_helper);
            }
            add_visual(ui, display, image);
        }
        Err(error) => {
            ui.label(
                RichText::new("PNG could not be decoded")
                    .small()
                    .color(p.danger),
            )
            .on_hover_text(error.to_string());
        }
    }
}

fn preferred_mime(bundle: &MimeBundle) -> Option<(MimeType, &str)> {
    // Prefer the mathematical/rich representation, then plain text.  The
    // renderer keeps the exact source as a fallback if RaTeX/resvg rejects it.
    const ORDER: [MimeType; 5] = [
        MimeType::TextLatex,
        MimeType::ImageSvg,
        MimeType::ImagePng,
        MimeType::TextPlain,
        MimeType::TextHtml,
    ];
    for mime in ORDER {
        if let Some(data) = bundle.get(mime) {
            return Some((mime, data));
        }
    }
    bundle
        .iter()
        .next()
        .and_then(|(mime, data)| MimeType::from_mime(mime).map(|mime| (mime, data)))
}

fn stale_tag(ui: &mut Ui) {
    let p = CarbonPalette::of(ui);
    ui.label(
        RichText::new("stale")
            .small()
            .color(p.text_helper)
            .italics(),
    );
}

/// Start (in characters) of the dotted identifier that ends at `cursor`.
/// Used for Tab completion; a token may contain `.` so `a.bi` is replaced as
/// a whole by the driver's `a.bit_length` candidate.
pub fn completion_token_start(source: &str, cursor: usize) -> usize {
    let chars: Vec<char> = source.chars().collect();
    let mut start = cursor.min(chars.len());
    while start > 0 {
        let ch = chars[start - 1];
        if ch.is_alphanumeric() || ch == '_' || ch == '.' {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// Replace the character range `[start, end)` with `insert`, returning the new
/// source and the caret position (in characters) after the insertion.
pub fn apply_completion(source: &str, start: usize, end: usize, insert: &str) -> (String, usize) {
    let chars: Vec<char> = source.chars().collect();
    let len = chars.len();
    let start = start.min(len);
    let end = end.clamp(start, len);
    let mut out: String = chars[..start].iter().collect();
    out.push_str(insert);
    out.extend(chars[end..].iter());
    (out, start + insert.chars().count())
}

pub fn format_duration(duration: Duration) -> String {
    if duration.as_secs_f64() < 1.0 {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

fn display_path(path: &Path, max: usize) -> String {
    let text = path.display().to_string();
    if text.chars().count() <= max {
        return text;
    }
    let keep = max.saturating_sub(3);
    let tail: String = text
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_human_readable() {
        assert_eq!(format_duration(Duration::from_millis(12)), "12 ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.50 s");
    }

    #[test]
    fn long_paths_keep_the_tail() {
        let short = display_path(Path::new("/usr/bin/sage"), 40);
        assert_eq!(short, "/usr/bin/sage");
        let long = display_path(Path::new("/a/very/long/path/that/keeps/going/to/sage"), 24);
        assert!(long.starts_with('…'));
        assert!(long.ends_with("sage"));
        assert!(long.chars().count() <= 24);
    }

    #[test]
    fn preferred_mime_prefers_latex() {
        let mut bundle = MimeBundle::new();
        bundle.insert(MimeType::TextPlain, "2");
        bundle.insert(MimeType::TextLatex, "2");
        let (mime, data) = preferred_mime(&bundle).unwrap();
        assert_eq!(mime, MimeType::TextLatex);
        assert_eq!(data, "2");
    }

    #[test]
    fn completion_token_covers_dotted_identifiers() {
        assert_eq!(completion_token_start("pri", 3), 0);
        assert_eq!(completion_token_start("a.bi", 4), 0);
        assert_eq!(completion_token_start("foo(a.bi", 8), 4);
        assert_eq!(completion_token_start("x = ", 4), 4);
        // Characters, not bytes: the token starts after the non-ASCII prefix.
        assert_eq!(completion_token_start("π + si", 6), 4);
    }

    #[test]
    fn completion_replaces_only_the_token_and_moves_the_caret() {
        let (source, cursor) = apply_completion("a = pri", 4, 7, "print(");
        assert_eq!(source, "a = print(");
        assert_eq!(cursor, 10);

        let (source, cursor) = apply_completion("obj.bi", 0, 6, "obj.bit_length");
        assert_eq!(source, "obj.bit_length");
        assert_eq!(cursor, 14);

        // Out-of-range ranges are clamped instead of panicking.
        let (source, _) = apply_completion("x", 9, 12, "tail");
        assert_eq!(source, "xtail");
    }

    #[test]
    fn carbon_palettes_have_contrasting_text_and_canvas() {
        let light = crate::settings::CarbonPalette::light();
        let dark = crate::settings::CarbonPalette::dark();
        assert!(!light.dark_mode);
        assert!(dark.dark_mode);
        assert_ne!(light.bg, light.text_primary);
        assert_ne!(dark.bg, dark.text_primary);
        // Carbon focus is Blue 60 in the light theme.
        assert_eq!(light.interactive, crate::settings::carbon::BLUE_60);
    }

    #[test]
    fn central_panel_uses_the_carbon_canvas_not_the_clear_color() {
        let ctx = test_context();
        crate::settings::apply_with_palette(&crate::settings::Settings::default(), &ctx);
        let frame = egui::Frame::central_panel(&ctx.style());
        assert_eq!(frame.fill, crate::settings::carbon::WHITE);
        assert_ne!(frame.fill, egui::Color32::TRANSPARENT);
    }

    /// A context with the bundled IBM Plex faces installed.  The real app
    /// installs them in `App::new`; headless UI tests must do the same or the
    /// named Medium/SemiBold families panic inside epaint.
    fn test_context() -> egui::Context {
        let ctx = egui::Context::default();
        crate::settings::install_fonts(&ctx);
        ctx
    }

    fn test_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            ..Default::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_notebook_frame(
        ctx: &egui::Context,
        notebook: &mut Notebook,
        runtime: &mut HashMap<CellId, CellRuntime>,
        focus: &mut Option<CellId>,
        last: &mut Option<CellId>,
        syntax: &SyntaxAssets,
        renderer: &mut Renderer,
        actions: &mut Vec<UiAction>,
        input: egui::RawInput,
    ) {
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let _ = notebook_ui(
                    ui,
                    notebook,
                    NotebookUi {
                        runtime,
                        focus_request: focus,
                        last_focused: last,
                        backend_ready: true,
                        syntax,
                        renderer,
                        actions,
                        bindings: &crate::settings::Keybindings::jupyter(),
                        selected: None,
                        scroll_to_cell: &mut None,
                    },
                );
            });
        });
    }

    #[test]
    fn cell_workspace_renders_headless() {
        let mut notebook = Notebook::new("test");
        let id = notebook.cells[0].id;
        notebook.cells[0].source = "a = 6*7\na".into();
        notebook.cells[0]
            .outputs
            .push(sagenote::cells::SavedOutput {
                mime: "text/latex".into(),
                data: "42".into(),
                display: DisplayMode::Block,
                stale: true,
            });
        let mut runtime = HashMap::new();
        runtime.insert(id, CellRuntime::from_saved(&notebook.cells[0]));
        let mut focus = Some(id);
        let mut last = None;
        let mut actions = Vec::new();
        let syntax = SyntaxAssets::new();
        let mut renderer = Renderer::new();
        let ctx = test_context();
        draw_notebook_frame(
            &ctx,
            &mut notebook,
            &mut runtime,
            &mut focus,
            &mut last,
            &syntax,
            &mut renderer,
            &mut actions,
            test_input(),
        );
        assert_eq!(focus, None, "focus request should be consumed");
    }

    #[test]
    fn rich_outputs_render_headless() {
        let mut notebook = Notebook::new("rich");
        let id = notebook.cells[0].id;
        notebook.cells[0].source = "rich outputs".into();
        let mut rt = CellRuntime::default();
        rt.outputs.push(LiveOutput::Stream {
            name: StreamName::Stdout,
            text: "\x1b[31mred\x1b[0m plain".into(),
            stale: false,
        });

        let mut latex = MimeBundle::new();
        latex.insert(MimeType::TextLatex, r"\frac{1}{2}".to_owned());
        rt.outputs.push(LiveOutput::Mime {
            bundle: latex,
            display: DisplayMode::Block,
            stale: false,
        });

        let mut svg = MimeBundle::new();
        svg.insert(
            MimeType::ImageSvg,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="12"><rect width="20" height="12" fill="#2a7"/></svg>"##
                .to_owned(),
        );
        rt.outputs.push(LiveOutput::Mime {
            bundle: svg,
            display: DisplayMode::Block,
            stale: false,
        });

        let mut png = MimeBundle::new();
        png.insert(
            MimeType::ImagePng,
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="
                .to_owned(),
        );
        rt.outputs.push(LiveOutput::Mime {
            bundle: png,
            display: DisplayMode::Inline,
            stale: true,
        });

        let mut runtime = HashMap::new();
        runtime.insert(id, rt);
        let mut focus = None;
        let mut last = None;
        let mut actions = Vec::new();
        let syntax = SyntaxAssets::new();
        let mut renderer = Renderer::new();
        let ctx = test_context();
        draw_notebook_frame(
            &ctx,
            &mut notebook,
            &mut runtime,
            &mut focus,
            &mut last,
            &syntax,
            &mut renderer,
            &mut actions,
            test_input(),
        );
    }

    #[test]
    fn shift_enter_runs_the_focused_editor() {
        let mut notebook = Notebook::new("test");
        let id = notebook.cells[0].id;
        notebook.cells[0].source = "1+1".into();
        let mut runtime = HashMap::new();
        runtime.insert(id, CellRuntime::default());
        let mut focus = Some(id);
        let mut last = None;
        let mut actions = Vec::new();
        let syntax = SyntaxAssets::new();
        let mut renderer = Renderer::new();
        let ctx = test_context();
        // Frame one grants focus to the editor.
        draw_notebook_frame(
            &ctx,
            &mut notebook,
            &mut runtime,
            &mut focus,
            &mut last,
            &syntax,
            &mut renderer,
            &mut actions,
            test_input(),
        );
        actions.clear();
        // Frame two sends Shift+Enter.  The editor's layouter is never reached
        // with the key because notebook_ui consumes it first.
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::SHIFT,
            }],
            modifiers: Modifiers::SHIFT,
            ..test_input()
        };
        draw_notebook_frame(
            &ctx,
            &mut notebook,
            &mut runtime,
            &mut focus,
            &mut last,
            &syntax,
            &mut renderer,
            &mut actions,
            input,
        );
        assert!(
            actions.contains(&UiAction::Run(id)),
            "Shift+Enter should queue a Run action, got {actions:?}"
        );
    }

    #[test]
    fn menu_and_status_render_headless() {
        let ctx = test_context();
        let menu = MenuState {
            backend_ready: true,
            starting: false,
            running: false,
            can_run_above: true,
            run_above_id: Some(CellId::new()),
            can_restart: true,
            inspector_open: true,
            can_refresh_inspector: true,
        };
        let status = StatusState {
            starting: false,
            backend_alive: true,
            sage_path: Some(PathBuf::from("/usr/bin/sage")),
            sage_version: "10.10".into(),
            notebook_path: Some(PathBuf::from("/tmp/notebook.sagenote")),
            notebook_title: "notebook.sagenote".into(),
            dirty: true,
            cells: 3,
            running: None,
        };
        let _ = ctx.run(test_input(), |ctx| {
            let _ = menu_bar(ctx, &menu);
            status_bar(ctx, &status);
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("body");
            });
        });
    }

    #[test]
    fn inspector_panel_renders_headless() {
        let vars = vec![
            VarInfo {
                name: "a".into(),
                type_name: "Integer".into(),
                value: "42".into(),
            },
            VarInfo {
                name: "matrix".into(),
                type_name: "Matrix".into(),
                value: "[1 2]\n[3 4]".into(),
            },
        ];
        let state = InspectorState {
            vars: &vars,
            loading: false,
            error: None,
            backend_ready: true,
            starting: false,
            blocked_running: false,
        };
        let ctx = test_context();
        let _ = ctx.run(test_input(), |ctx| {
            let action = inspector_panel(ctx, &state);
            assert!(action.is_none());
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("body");
            });
        });

        let empty = InspectorState {
            vars: &[],
            loading: true,
            error: None,
            backend_ready: true,
            starting: false,
            blocked_running: false,
        };
        let _ = ctx.run(test_input(), |ctx| {
            let action = inspector_panel(ctx, &empty);
            assert!(action.is_none());
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("body");
            });
        });
    }
}
