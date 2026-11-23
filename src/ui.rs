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
    self, Color32, Event, FontId, Key, Modifiers, Response, RichText, Stroke, TextStyle, Ui,
};
use sagenote::backend::{
    CellId, CompletionItem, CompletionKind, DisplayMode, MimeBundle, MimeType, StreamName, VarInfo,
};
use sagenote::cells::{Cell, CellState, Notebook};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::app::{CellRuntime, LiveOutput, PendingCompletion};
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
    /// Ctrl+Space (or the configured binding): ask the driver for
    /// documentation.  `name` is the target the UI resolved around the caret
    /// (the token there, or the enclosing call's callee); `None` lets the
    /// driver resolve the offset itself.
    RequestDocs {
        cell: CellId,
        source: String,
        offset: usize,
        name: Option<String>,
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
    /// Auto-close brackets and quotes while typing (Settings).
    pub auto_close: bool,
    /// The current documentation answer, for the completion popup's side
    /// panel.  The floating window is drawn by `App::draw_docs_window`.
    pub doc: Option<crate::app::DocPopup>,
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
        auto_close,
        doc,
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
            auto_close,
            doc.as_ref(),
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
    auto_close: bool,
    doc: Option<&crate::app::DocPopup>,
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

        // Documentation: Ctrl+Space by default.  With the completion popup
        // open it toggles docs-follow mode instead: the driver's answers
        // render in a panel beside the candidate list and follow the
        // selection (`App::pump_completion_docs` issues the requests).
        // Without a popup, the UI resolves the target around the caret — the
        // token there, or the enclosing call's callee inside an argument
        // list — and falls back to the driver's own token extraction.
        if focused && rt.doc_request.is_none() && consume_binding(ui, bindings.get("show_docs")) {
            if let Some(popup) = rt.completion_popup.as_mut() {
                popup.docs = !popup.docs;
            } else {
                let cursor = egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
                    .and_then(|state| state.cursor.char_range())
                    .filter(|range| range.primary.index == range.secondary.index)
                    .map(|range| range.primary.index)
                    .unwrap_or_else(|| cell.source.chars().count());
                actions.push(UiAction::RequestDocs {
                    cell: cell.id,
                    source: cell.source.clone(),
                    offset: cursor,
                    name: docs_target(&cell.source, cursor),
                });
            }
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
        if let Some(pending) = rt.pending_completion.take() {
            // A callable candidate accepted with auto-close on leaves
            // `plot()` with the caret between the parentheses, exactly as
            // typing the opener does; otherwise the clean name is inserted.
            let closes = auto_close && pending.closes;
            let text = if closes {
                format!("{}()", pending.text)
            } else {
                pending.text
            };
            let (new_source, new_cursor) =
                apply_completion(&cell.source, pending.start, pending.end, &text);
            if new_source != cell.source {
                cell.source = new_source;
                cursor_after = Some(if closes {
                    new_cursor.saturating_sub(1)
                } else {
                    new_cursor
                });
                source_changed = true;
            }
        }

        // Auto-pair interception (Settings → "Auto-close brackets and
        // quotes").  Typed openers insert the pair, a closer typed over the
        // matching closer skips over it, Backspace between an empty pair
        // deletes both, and a selection is wrapped.  The decision runs on
        // pure functions below and is applied before the editor is laid
        // out, reusing the completion machinery for the caret move.
        if focused && auto_close && rt.completion_popup.is_none() {
            if let Some(edit) = auto_pair_edit(ui, &cell.source, editor_id) {
                match edit {
                    PairEdit::Replace {
                        start,
                        end,
                        text,
                        cursor,
                    } => {
                        let (new_source, _) = apply_completion(&cell.source, start, end, &text);
                        if new_source != cell.source {
                            cell.source = new_source;
                            cursor_after = Some(cursor);
                            source_changed = true;
                        }
                    }
                    PairEdit::SkipOver { cursor } => {
                        cursor_after = Some(cursor);
                    }
                }
            }
        }

        // VSCode-Jupyter chrome: no box.  A 2 px left accent marks the
        // selected/running cell, the gutter carries the execution count, and
        // the strip above the editor hosts the small toolbar — only while
        // the cell is selected.  The strip is always allocated so selection
        // does not shift the layout.
        let is_selected = selected == Some(cell.id);
        let accent = if is_selected || rt.state == CellState::Running {
            p.interactive
        } else {
            Color32::TRANSPARENT
        };
        let cell_top_left = ui.cursor().min;
        let strip =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
        if is_selected {
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(strip.0), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().interact_size = egui::vec2(0.0, 18.0);
                    ui.spacing_mut().button_padding = egui::vec2(6.0, 1.0);
                    ui.add_enabled_ui(backend_ready, |ui| {
                        if strip_button(ui, "Run", true)
                            .on_hover_text("Evaluate this cell (Shift+Enter)")
                            .clicked()
                        {
                            actions.push(UiAction::Run(cell.id));
                        }
                    });
                    if strip_button(ui, "+", false)
                        .on_hover_text("Insert a cell below")
                        .clicked()
                    {
                        actions.push(UiAction::AddCell(Some(cell.id)));
                    }
                    if ui
                        .add_enabled(index > 0, egui::Button::new("↑").small())
                        .on_hover_text("Move up")
                        .clicked()
                    {
                        actions.push(UiAction::MoveUp(cell.id));
                    }
                    if ui
                        .add_enabled(index + 1 < total, egui::Button::new("↓").small())
                        .on_hover_text("Move down")
                        .clicked()
                    {
                        actions.push(UiAction::MoveDown(cell.id));
                    }
                    if ui
                        .add_enabled(total > 1, egui::Button::new("×").small())
                        .on_hover_text("Delete cell")
                        .clicked()
                    {
                        actions.push(UiAction::DeleteCell(cell.id));
                    }
                });
            });
        }

        let edit_changed = ui
            .horizontal_top(|ui| {
                let (gutter, _) =
                    ui.allocate_exact_size(egui::vec2(44.0, 18.0), egui::Sense::hover());
                paint_gutter(ui, gutter, rt, &p);
                let width = ui.available_width();
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    syntax.layout_galley(ui, text, wrap_width)
                };
                let mut output = egui::TextEdit::multiline(&mut cell.source)
                    .id(editor_id)
                    .code_editor()
                    .desired_rows(1)
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
                    actions.push(UiAction::RunAndAdvance(cell.id));
                }

                if let Some(index) = cursor_after {
                    output
                        .state
                        .cursor
                        .set_char_range(Some(CCursorRange::one(CCursor::new(index))));
                    output.state.store(ui.ctx(), editor_id);
                    output.response.request_focus();
                } else if rt.completion_popup.is_some() && output.response.has_focus() {
                    draw_completion_popup(ui, cell.id, rt, &output, focus_request, doc);
                } else if !output.response.has_focus() {
                    rt.completion_popup = None;
                }
                changed
            })
            .inner;

        if rt.has_output()
            || rt.error.is_some()
            || matches!(
                rt.state,
                CellState::Running | CellState::Queued | CellState::Interrupted
            )
        {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(44.0);
                draw_outputs(ui, rt, renderer);
            });
        }

        // Left accent: a 2 px Carbon Blue 60 bar on the cell's edge.
        if accent != Color32::TRANSPARENT {
            let bottom = ui.cursor().min.y;
            let rect = egui::Rect::from_min_max(
                egui::pos2(cell_top_left.x, cell_top_left.y),
                egui::pos2(cell_top_left.x + 2.0, bottom),
            );
            ui.painter().rect_filled(rect, 0.0, accent);
        }
        edit_changed
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
        rt.pending_completion = Some(PendingCompletion {
            start: popup.start,
            end: popup.end,
            text: item.name,
            closes: item.kind.is_callable(),
        });
        rt.completion_popup = None;
        consumed = true;
    } else if cancel {
        rt.completion_popup = None;
        consumed = true;
    }
    consumed
}

/// Render the completion list with its left edge at the token start, so it
/// stays put while the user types.  Each row carries a small type icon so the
/// candidate names align in a column.  In docs-follow mode a panel to the
/// right of the list shows the selected candidate's documentation and
/// follows the selection.
fn draw_completion_popup(
    ui: &mut Ui,
    cell: CellId,
    rt: &mut CellRuntime,
    output: &egui::widgets::text_edit::TextEditOutput,
    focus_request: &mut Option<CellId>,
    doc: Option<&crate::app::DocPopup>,
) {
    let Some(mut popup) = rt.completion_popup.take() else {
        return;
    };
    let p = CarbonPalette::of(ui);
    // Anchor at the completed token's start, not at the caret: the left
    // edge is stable while typing within the token.
    let token = output.galley.pos_from_ccursor(CCursor::new(popup.start));
    let anchor = output.galley_pos + token.left_bottom().to_vec2() + egui::vec2(0.0, 4.0);

    let mut chosen: Option<usize> = None;
    let list = egui::Area::new(egui::Id::new(("sagenote-completion", cell.0)))
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
                    // Compact, IDE-like rows: 22 px tall, 13 px mono, with
                    // a 12 px icon column.  The global 32 px control height
                    // is too airy for a completion list.
                    ui.spacing_mut().interact_size = egui::vec2(0.0, 22.0);
                    ui.set_min_width(200.0);
                    ui.set_max_width(480.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("sagenote-completion-scroll", cell.0))
                        .max_height(180.0)
                        .show(ui, |ui| {
                            for (index, item) in popup.items.iter().enumerate() {
                                let selected = index == popup.selected;
                                let hover = if item.signature.is_empty() {
                                    item.kind.as_str().to_owned()
                                } else {
                                    format!("{} {}", item.kind.as_str(), item.signature)
                                };
                                let response = ui
                                    .allocate_response(
                                        egui::vec2(ui.available_width(), 22.0),
                                        egui::Sense::click(),
                                    )
                                    .on_hover_text(hover);
                                paint_candidate_row(ui, response.rect, item, selected, &p);
                                if selected {
                                    response.scroll_to_me(Some(egui::Align::Center));
                                }
                                if response.clicked() {
                                    chosen = Some(index);
                                }
                            }
                        });
                })
        });

    if popup.docs {
        if let Some(doc) = doc.filter(|doc| doc.anchored && doc.cell == cell) {
            draw_docs_panel(ui.ctx(), list.response.rect, cell, doc, &p);
        }
    }

    if let Some(index) = chosen {
        let item = popup.items[index].clone();
        rt.pending_completion = Some(PendingCompletion {
            start: popup.start,
            end: popup.end,
            text: item.name,
            closes: item.kind.is_callable(),
        });
        *focus_request = Some(cell);
    } else {
        popup.selected = popup.selected.min(popup.items.len().saturating_sub(1));
        rt.completion_popup = Some(popup);
    }
}

/// One completion row: a 12 px kind badge (icon column) and the candidate
/// name at a fixed x, so the names left-align.  Selected rows get the Carbon
/// selection fill.
fn paint_candidate_row(
    ui: &Ui,
    rect: egui::Rect,
    item: &CompletionItem,
    selected: bool,
    p: &CarbonPalette,
) {
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 0.0, p.selected);
    }
    let (glyph, fill) = kind_badge(p, item.kind);
    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        egui::vec2(12.0, 12.0),
    );
    painter.rect_filled(icon, 2.0, fill);
    if !glyph.is_empty() {
        painter.text(
            icon.center(),
            egui::Align2::CENTER_CENTER,
            glyph,
            FontId::monospace(9.0),
            Color32::WHITE,
        );
    }
    let font = FontId::monospace(13.0);
    let name_rect = painter.text(
        egui::pos2(rect.left() + 24.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &item.name,
        font.clone(),
        p.text_primary,
    );
    if item.signature.is_empty() {
        return;
    }
    let start = name_rect.right() + 6.0;
    let available = (rect.right() - 6.0 - start).max(0.0);
    let signature = ellipsize(painter, &item.signature, font.clone(), available);
    if signature.is_empty() {
        return;
    }
    painter.text(
        egui::pos2(start, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &signature,
        font,
        p.text_helper,
    );
}

/// Truncate `text` with an ellipsis so it fits in `max_width` when rendered
/// in `font`.  A completion row's signature can be far wider than the popup.
fn ellipsize(painter: &egui::Painter, text: &str, font: egui::FontId, max_width: f32) -> String {
    if max_width <= 0.0 {
        return String::new();
    }
    let width = |string: &str| {
        painter
            .layout_no_wrap(string.to_owned(), font.clone(), egui::Color32::WHITE)
            .size()
            .x
    };
    if width(text) <= max_width {
        return text.to_owned();
    }
    let mut cut = text.chars().count();
    while cut > 1 {
        let with_ellipsis: String = format!("{}…", text.chars().take(cut).collect::<String>());
        if width(&with_ellipsis) <= max_width {
            return with_ellipsis;
        }
        cut -= 1;
    }
    "…".to_owned()
}

/// The `[n]` execution-count gutter — VSCode-Jupyter's id number.  `[*]`
/// while queued/running, `[–]` after an interrupt, `[n]` in the danger colour
/// for a failed run, and grey when the shown outputs are stale.
fn paint_gutter(ui: &Ui, rect: egui::Rect, rt: &CellRuntime, p: &CarbonPalette) {
    let count = match rt.execution_count {
        Some(n) => format!("[{n}]"),
        None => "[ ]".to_owned(),
    };
    let (text, color) = match rt.state {
        CellState::Queued | CellState::Running => ("[*]".to_owned(), p.interactive),
        CellState::Interrupted => ("[–]".to_owned(), p.warning),
        CellState::Error => (
            count,
            if rt.error_stale {
                p.text_helper
            } else {
                p.danger
            },
        ),
        _ => {
            let stale = rt.outputs.iter().any(|output| {
                matches!(
                    output,
                    LiveOutput::Stream { stale: true, .. } | LiveOutput::Mime { stale: true, .. }
                )
            });
            (
                count,
                if stale {
                    p.text_helper
                } else {
                    p.text_secondary
                },
            )
        }
    };
    ui.painter().text(
        egui::pos2(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        text,
        FontId::monospace(12.0),
        color,
    );
}

/// Compact 12 px-text strip button for the selected cell's floating toolbar.
/// `primary` is the Carbon Blue 60 fill used for Run.
fn strip_button(ui: &mut Ui, label: &str, primary: bool) -> Response {
    let p = CarbonPalette::of(ui);
    ui.scope(|ui| {
        if primary {
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
        }
        ui.add(egui::Button::new(RichText::new(label).size(12.0)))
    })
    .inner
}

/// The kind badge for a completion candidate: a one-letter glyph and a Carbon
/// accent colour.  Function reuses the palette's interactive Blue 60.
fn kind_badge(p: &CarbonPalette, kind: CompletionKind) -> (&'static str, Color32) {
    match kind {
        CompletionKind::Function => ("f", p.interactive),
        CompletionKind::Class => ("c", Color32::from_rgb(0x8a, 0x3f, 0xfc)),
        CompletionKind::Module => ("m", Color32::from_rgb(0x00, 0x9a, 0x9a)),
        CompletionKind::Keyword => ("k", Color32::from_rgb(0xd1, 0x27, 0x65)),
        CompletionKind::Variable => ("v", Color32::from_rgb(0x78, 0x78, 0x78)),
        CompletionKind::Unknown => ("", Color32::from_rgb(0xa8, 0xa8, 0xa8)),
    }
}

/// The docs-follow panel: the selected candidate's documentation, anchored to
/// the right of the completion list and redrawn as the selection moves.
fn draw_docs_panel(
    ctx: &egui::Context,
    list: egui::Rect,
    cell: CellId,
    doc: &crate::app::DocPopup,
    p: &CarbonPalette,
) {
    let pos = egui::pos2(list.right() + 8.0, list.top());
    egui::Area::new(egui::Id::new(("sagenote-completion-docs", cell.0)))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .constrain(true)
        .show(ctx, |ui| {
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
                .inner_margin(10.0)
                .show(ui, |ui| {
                    ui.set_min_width(320.0);
                    ui.set_max_width(440.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&doc.name)
                                .font(semibold(14.0))
                                .color(p.text_primary),
                        );
                        if !doc.type_name.is_empty() {
                            ui.label(RichText::new(&doc.type_name).small().color(p.text_helper));
                        }
                    });
                    if !doc.signature.is_empty() {
                        ui.add_space(4.0);
                        egui::Frame::default()
                            .fill(p.layer)
                            .corner_radius(0.0)
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&doc.signature)
                                            .monospace()
                                            .color(p.text_secondary),
                                    )
                                    .wrap(),
                                );
                            });
                    }
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("sagenote-completion-docs-body", cell.0))
                        .max_height(220.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            draw_doc_text(ui, &doc.text, p);
                        });
                });
        });
}

/// Render a plain docstring with light IDE-style formatting: RST section
/// headings become semi-bold labels, indented/`>>>` blocks are monospaced on a
/// Gray 10 code surface, and prose is wrapped normally.
pub(crate) fn draw_doc_text(ui: &mut Ui, text: &str, p: &CarbonPalette) {
    let text = strip_ansi(text);
    if text.trim().is_empty() {
        ui.label(RichText::new("No documentation.").color(p.text_helper));
        return;
    }
    let lines: Vec<&str> = text.lines().collect();
    let is_code = |line: &str| {
        line.starts_with("    ")
            || line.starts_with('\t')
            || line.trim_start().starts_with(">>>")
            || line.trim_start().starts_with("...")
    };

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // RST section heading: prose followed by a line of punctuation.
        if i + 1 < lines.len() && !line.trim().is_empty() {
            let underline = lines[i + 1].trim();
            if underline.len() >= 3
                && underline
                    .chars()
                    .all(|ch| matches!(ch, '-' | '=' | '~' | '^' | '"'))
            {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(line.trim())
                        .font(semibold(14.0))
                        .color(p.text_primary),
                );
                i += 2;
                continue;
            }
        }
        if line.trim().is_empty() {
            ui.add_space(4.0);
            i += 1;
            continue;
        }
        if is_code(line) {
            let mut block = String::new();
            while i < lines.len() && (is_code(lines[i]) || lines[i].trim().is_empty()) {
                block.push_str(lines[i].trim_end());
                block.push('\n');
                i += 1;
            }
            egui::Frame::default()
                .fill(p.layer)
                .corner_radius(0.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(block.trim_end())
                                .monospace()
                                .color(p.text_secondary),
                        )
                        .wrap(),
                    );
                });
        } else {
            let mut block = String::new();
            while i < lines.len() && !lines[i].trim().is_empty() && !is_code(lines[i]) {
                block.push_str(lines[i].trim_end());
                block.push('\n');
                i += 1;
            }
            ui.add(egui::Label::new(RichText::new(block.trim_end()).color(p.text_primary)).wrap());
        }
    }
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
            // The transport mimetype is protocol plumbing, not something the
            // reader needs: rendered results carry no mime tag.  The tag row
            // survives only for the `inline` display mode and `stale`.
            let inline = *display == DisplayMode::Inline;
            if inline || *stale {
                ui.horizontal_wrapped(|ui| {
                    if inline {
                        ui.label(RichText::new("inline").small().weak());
                    }
                    if *stale {
                        stale_tag(ui);
                    }
                });
            }
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

/// The documentation target around the caret: the dotted identifier ending
/// there, or — when the caret sits in an argument list with no token — the
/// innermost enclosing call's callee.  `None` when nothing is identifiable;
/// the driver then resolves the token at the offset itself.
pub fn docs_target(source: &str, caret: usize) -> Option<String> {
    let chars: Vec<char> = source.chars().collect();
    let caret = caret.min(chars.len());
    let start = completion_token_start(source, caret);
    if start < caret {
        return Some(chars[start..caret].iter().collect());
    }
    // No token at the caret: scan back to the innermost unclosed bracket,
    // skipping balanced pairs, and take the dotted identifier before it.
    let mut depth = 0usize;
    let mut i = caret;
    while i > 0 {
        i -= 1;
        match chars[i] {
            ')' | ']' | '}' => depth += 1,
            '(' | '[' | '{' => {
                if depth > 0 {
                    depth -= 1;
                    continue;
                }
                let mut end = i;
                while end > 0 && chars[end - 1].is_whitespace() {
                    end -= 1;
                }
                let mut start = end;
                while start > 0
                    && (chars[start - 1].is_alphanumeric()
                        || chars[start - 1] == '_'
                        || chars[start - 1] == '.')
                {
                    start -= 1;
                }
                if start < end {
                    return Some(chars[start..end].iter().collect());
                }
                return None;
            }
            _ => {}
        }
    }
    None
}

/// Bracket and quote pairs that auto-close while typing.
const PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];

/// The outcome of one intercepted auto-pair event, expressed as a
/// `apply_completion`-style edit so the editor's staged-caret machinery is
/// reused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairEdit {
    /// Replace `start..end` (characters) with `text`; the caret ends at
    /// `cursor`.
    Replace {
        start: usize,
        end: usize,
        text: String,
        cursor: usize,
    },
    /// Only move the caret (typing a closer over the same closer).
    SkipOver { cursor: usize },
}

/// Pure decision for one typed character.  `head`/`anchor` are the (possibly
/// unordered) cursor positions in characters.  An opener inserts the pair;
/// with a selection it wraps instead.  A quote only closes when it would not
/// glue onto a word character (apostrophes inside words stay untouched).
pub fn auto_pair_on_text(
    source: &str,
    head: usize,
    anchor: usize,
    typed: &str,
) -> Option<PairEdit> {
    let mut chars_of = typed.chars();
    let ch = chars_of.next()?;
    // One character per event; multi-character (IME) events pass through.
    if chars_of.next().is_some() {
        return None;
    }
    let chars: Vec<char> = source.chars().collect();
    let head = head.min(chars.len());
    let anchor = anchor.min(chars.len());
    let (sel_min, sel_max) = if head <= anchor {
        (head, anchor)
    } else {
        (anchor, head)
    };

    // Typing a closer over the same closer skips over it.
    if PAIRS
        .iter()
        .any(|(_, close)| *close == ch && chars.get(head).copied() == Some(ch))
    {
        return Some(PairEdit::SkipOver { cursor: head + 1 });
    }

    // Typing an opener inserts the pair (quotes only when the previous
    // character would not read as an apostrophe inside a word: `dont` stays
    // plain, `s = 'x` closes its string).
    let close = PAIRS
        .iter()
        .find(|(open, _)| *open == ch)
        .map(|(_, c)| *c)?;
    if (ch == '"' || ch == '\'')
        && head > 0
        && (chars[head - 1].is_alphanumeric() || chars[head - 1] == '_')
    {
        return None;
    }
    if head != anchor {
        // Wrap the selection in the pair.
        let selected: String = chars[sel_min..sel_max].iter().collect();
        Some(PairEdit::Replace {
            start: sel_min,
            end: sel_max,
            text: format!("{ch}{selected}{close}"),
            cursor: sel_max + 2,
        })
    } else {
        Some(PairEdit::Replace {
            start: head,
            end: head,
            text: format!("{ch}{close}"),
            cursor: head + 1,
        })
    }
}

/// Pure decision for Backspace: delete an empty pair around the caret.
pub fn auto_pair_on_backspace(source: &str, caret: usize) -> Option<PairEdit> {
    let chars: Vec<char> = source.chars().collect();
    let caret = caret.min(chars.len());
    if caret == 0 || caret >= chars.len() {
        return None;
    }
    let before = chars[caret - 1];
    let after = chars[caret];
    if PAIRS
        .iter()
        .any(|(open, close)| *open == before && *close == after)
    {
        return Some(PairEdit::Replace {
            start: caret - 1,
            end: caret + 1,
            text: String::new(),
            cursor: caret - 1,
        });
    }
    None
}

/// Consume one focused-editor input event and decide the pair edit for it.
/// Backspace is peeked first and consumed only when a pair deletes; text
/// events are removed only when a rule fires, so ordinary typing reaches the
/// editor untouched.
fn auto_pair_edit(ui: &mut Ui, source: &str, editor_id: egui::Id) -> Option<PairEdit> {
    let (head, anchor) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
        .and_then(|state| state.cursor.char_range())
        .map(|range| (range.primary.index, range.secondary.index))?;
    let mut edit: Option<PairEdit> = None;
    ui.input_mut(|input| {
        input.events.retain(|event| {
            if edit.is_some() {
                return true;
            }
            let Event::Text(text) = event else {
                return true;
            };
            match auto_pair_on_text(source, head, anchor, text) {
                Some(pair_edit) => {
                    edit = Some(pair_edit);
                    false
                }
                None => true,
            }
        });
    });
    if edit.is_none() && head == anchor {
        let backspace = ui.input(|input| {
            input.modifiers.is_none()
                && input.events.iter().any(|event| {
                    matches!(
                        event,
                        Event::Key {
                            key: Key::Backspace,
                            pressed: true,
                            ..
                        }
                    )
                })
        });
        if backspace {
            if let Some(pair_edit) = auto_pair_on_backspace(source, head) {
                ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Backspace));
                edit = Some(pair_edit);
            }
        }
    }
    edit
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
    use crate::app::CompletionPopup;

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
    fn docs_target_finds_the_token_and_the_enclosing_callee() {
        // Token ending at the caret wins.
        assert_eq!(docs_target("pri", 3), Some("pri".into()));
        assert_eq!(docs_target("a.bi", 4), Some("a.bi".into()));
        assert_eq!(docs_target("", 0), None);
        // Caret inside an argument list: resolve the innermost call's callee.
        assert_eq!(docs_target("plot(x, ", 8), Some("plot".into()));
        assert_eq!(docs_target("matrix(R, )", 10), Some("matrix".into()));
        // Nested calls: the innermost open bracket wins.
        assert_eq!(docs_target("f(g(h(a,", 8), Some("h".into()));
    }

    #[test]
    fn auto_pairs_close_wrap_and_skip() {
        // Opener inserts the pair, caret between.
        assert_eq!(
            auto_pair_on_text("x = 1", 4, 4, "("),
            Some(PairEdit::Replace {
                start: 4,
                end: 4,
                text: "()".into(),
                cursor: 5,
            })
        );
        // Selection is wrapped.
        assert_eq!(
            auto_pair_on_text("x = abc", 4, 7, "("),
            Some(PairEdit::Replace {
                start: 4,
                end: 7,
                text: "(abc)".into(),
                cursor: 9,
            })
        );
        // A closer typed over the same closer skips over it.
        assert_eq!(
            auto_pair_on_text("f(x)", 3, 3, ")"),
            Some(PairEdit::SkipOver { cursor: 4 })
        );
        // Quotes do not glue onto a word character (apostrophes).
        assert_eq!(auto_pair_on_text("dont", 4, 4, "'"), None);
        // Ordinary identifiers are untouched.
        assert_eq!(auto_pair_on_text("x = a", 5, 5, "a"), None);
    }

    #[test]
    fn auto_pair_backspace_deletes_the_empty_pair() {
        assert_eq!(
            auto_pair_on_backspace("f()", 2),
            Some(PairEdit::Replace {
                start: 1,
                end: 3,
                text: String::new(),
                cursor: 1,
            })
        );
        // Not an empty pair: no edit.
        assert_eq!(auto_pair_on_backspace("f(a)", 3), None);
        assert_eq!(auto_pair_on_backspace("f()", 0), None);
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
                        auto_close: true,
                        doc: None,
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
    fn shift_enter_runs_the_focused_editor_and_advances() {
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
            actions.contains(&UiAction::RunAndAdvance(id)),
            "Shift+Enter should queue a RunAndAdvance action, got {actions:?}"
        );
    }

    #[test]
    fn ctrl_space_inside_a_call_requests_docs_for_the_callee() {
        let mut notebook = Notebook::new("test");
        let id = notebook.cells[0].id;
        notebook.cells[0].source = "integral(sin(x), x)".into();
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
        // Place the caret just after `integral(`, inside the argument list.
        let editor_id = egui::Id::new(("sagenote-cell-editor", id.0));
        if let Some(mut state) = egui::widgets::text_edit::TextEditState::load(&ctx, editor_id) {
            state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(9))));
            state.store(&ctx, editor_id);
        }
        actions.clear();
        // Frame two sends Ctrl+Space.
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            }],
            modifiers: Modifiers::CTRL,
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
        // The UI resolved the enclosing call's callee as the explicit target.
        assert!(
            actions.iter().any(|action| matches!(
                action,
                UiAction::RequestDocs {
                    cell: c,
                    name: Some(name),
                    ..
                } if *c == id && name == "integral"
            )),
            "Ctrl+Space should request docs for the callee, got {actions:?}"
        );
    }

    #[test]
    fn accepting_a_callable_completion_parks_the_caret_inside_the_pair() {
        let mut notebook = Notebook::new("test");
        let id = notebook.cells[0].id;
        notebook.cells[0].source = "a = pri".into();
        let mut runtime = HashMap::new();
        let rt = CellRuntime {
            completion_popup: Some(CompletionPopup {
                items: vec![CompletionItem {
                    name: "print".into(),
                    kind: CompletionKind::Function,
                    signature: "(value, ..., sep=' ', end='\\n')".into(),
                }],
                selected: 0,
                start: 4,
                end: 7,
                docs: false,
            }),
            ..Default::default()
        };
        runtime.insert(id, rt);
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
        // Frame two sends Enter to accept the candidate.
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            modifiers: Modifiers::NONE,
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
        // The accepted callable leaves `print()` with the caret between the parens.
        assert_eq!(notebook.cells[0].source, "a = print()");
        let editor_id = egui::Id::new(("sagenote-cell-editor", id.0));
        let cursor = egui::widgets::text_edit::TextEditState::load(&ctx, editor_id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| range.primary.index);
        assert_eq!(cursor, Some(10));
    }

    #[test]
    fn menu_and_status_render_headless() {
        let ctx = test_context();
        let menu = MenuState {
            backend_ready: true,
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
