//! Cell workspace, menu/status bars, the variable inspector, the syntect-based
//! Rust-native editor, and the output panel (RaTeX/SVG/PNG + text fallbacks).
//!
//! Why not `egui_code_editor`?  The current release (0.4.2) pins egui 0.36
//! while this app uses eframe/egui 0.31.1, and it ships its own highlighter
//! rather than accepting a caller-supplied `syntect::SyntaxSet`.  The spec
//! explicitly allows the fallback: `TextEdit::multiline` plus a syntect
//! layouter.  That also makes Shift+Enter interception straightforward.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontId, Key, Modifiers, RichText, Stroke, TextStyle, Ui};
use sagenote::backend::{CellId, DisplayMode, MimeBundle, MimeType, StreamName, VarInfo};
use sagenote::cells::{Cell, CellState, Notebook};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::app::{CellRuntime, LiveOutput};
use crate::render::Renderer;
use crate::text::strip_ansi;

const ERROR_RED: Color32 = Color32::from_rgb(178, 45, 45);
const RUNNING_BLUE: Color32 = Color32::from_rgb(45, 95, 165);
const STDERR_ORANGE: Color32 = Color32::from_rgb(168, 92, 25);
const STALE_GREY: Color32 = Color32::from_gray(115);
const OUTPUT_BG: Color32 = Color32::from_gray(245);
const CELL_BORDER: Color32 = Color32::from_gray(222);

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
    RunAll,
    RunAbove(CellId),
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
    pub last_updated: Option<Instant>,
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
}

/// Syntax definitions and theme loaded once at startup.  The fallback editor
/// uses these for every cell.
pub struct SyntaxAssets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl SyntaxAssets {
    pub fn new() -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        // Prefer a light theme to match the light UI, then any theme, then
        // syntect's Default-derived empty theme.  A missing theme here must
        // degrade to unhighlighted text, never panic the app at startup.
        let theme = themes
            .themes
            .get("InspiredGitHub")
            .or_else(|| themes.themes.get("base16-ocean.light"))
            .or_else(|| themes.themes.values().next())
            .cloned()
            .unwrap_or_default();
        Self { syntaxes, theme }
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
        let fallback_format = egui::text::TextFormat {
            font_id: font.clone(),
            color: Color32::from_rgb(35, 35, 35),
            ..Default::default()
        };

        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = if wrap_width.is_finite() && wrap_width > 0.0 {
            wrap_width
        } else {
            f32::INFINITY
        };

        let mut highlighter = HighlightLines::new(syntax, &self.theme);
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
}

/// Menus and quick actions.  All actions are returned to the app, which is the
/// only writer of notebook/backend state.
pub fn menu_bar(ctx: &egui::Context, state: &MenuState) -> Vec<UiAction> {
    let mut actions = Vec::new();
    egui::TopBottomPanel::top("sagenote-menubar")
        .frame(
            egui::Frame::default()
                .fill(Color32::from_gray(248))
                .stroke(Stroke::new(1.0_f32, Color32::from_gray(228)))
                .inner_margin(egui::Margin::symmetric(8, 4)),
        )
        .show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    menu_item(
                        ui,
                        "New",
                        Some("Ctrl/⌘ N"),
                        true,
                        UiAction::New,
                        &mut actions,
                    );
                    menu_item(
                        ui,
                        "Open…",
                        Some("Ctrl/⌘ O"),
                        true,
                        UiAction::Open,
                        &mut actions,
                    );
                    menu_item(
                        ui,
                        "Save",
                        Some("Ctrl/⌘ S"),
                        true,
                        UiAction::Save,
                        &mut actions,
                    );
                    menu_item(
                        ui,
                        "Save As…",
                        Some("Ctrl/⌘ Shift S"),
                        true,
                        UiAction::SaveAs,
                        &mut actions,
                    );
                    ui.separator();
                    menu_item(
                        ui,
                        "Quit",
                        Some("Ctrl/⌘ Q"),
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
                        Some("Ctrl/⌘ ."),
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
                        Some("Ctrl/⌘ I"),
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
                        "About sagenote",
                        None,
                        true,
                        UiAction::ShowAbout,
                        &mut actions,
                    );
                });

                ui.separator();
                if state.starting {
                    ui.spinner();
                    ui.label(RichText::new("Starting Sage…").small().weak());
                } else {
                    if ui
                        .add_enabled(state.backend_ready, egui::Button::new("▶ Run All").small())
                        .on_hover_text("Evaluate every non-empty cell")
                        .clicked()
                    {
                        actions.push(UiAction::RunAll);
                    }
                    if ui
                        .add_enabled(state.running, egui::Button::new("■ Interrupt").small())
                        .on_hover_text("SIGINT the Sage process group")
                        .clicked()
                    {
                        actions.push(UiAction::Interrupt);
                    }
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
    egui::TopBottomPanel::bottom("sagenote-status")
        .frame(
            egui::Frame::default()
                .fill(Color32::from_gray(248))
                .stroke(Stroke::new(1.0_f32, Color32::from_gray(228)))
                .inner_margin(egui::Margin::symmetric(10, 3)),
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
                            .strong(),
                    );
                    if let Some(path) = &state.sage_path {
                        let text = display_path(path, 48);
                        ui.separator();
                        ui.add(egui::Label::new(RichText::new(text).small().weak()).truncate())
                            .on_hover_text(path.display().to_string());
                    }
                } else {
                    ui.label(RichText::new("Sage unavailable").small().color(ERROR_RED));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if state.dirty {
                        ui.label(
                            RichText::new("● unsaved")
                                .small()
                                .color(egui::Color32::from_rgb(180, 110, 20)),
                        );
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
                        ui.label(RichText::new(running).small().color(RUNNING_BLUE));
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
        .fill(Color32::from_gray(249))
        .stroke(Stroke::new(1.0_f32, Color32::from_gray(226)))
        .inner_margin(egui::Margin::same(10));
    egui::SidePanel::right("sagenote-inspector")
        .resizable(true)
        .default_width(276.0)
        .width_range(220.0..=420.0)
        .frame(frame)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Variables").strong().size(15.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("✕")
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

            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                if state.starting {
                    ui.spinner();
                    ui.label(RichText::new("Starting Sage…").small().weak());
                } else if state.loading {
                    ui.spinner();
                    ui.label(RichText::new("Reading globals…").small().weak());
                } else if state.blocked_running {
                    ui.label(
                        RichText::new("Paused while a cell runs")
                            .small()
                            .color(STDERR_ORANGE),
                    );
                } else if let Some(error) = state.error {
                    ui.label(RichText::new(error).small().color(ERROR_RED));
                } else if !state.backend_ready {
                    ui.label(RichText::new("Sage is not running").small().weak());
                } else if let Some(updated) = state.last_updated {
                    let noun = if state.vars.len() == 1 {
                        "variable"
                    } else {
                        "variables"
                    };
                    ui.label(
                        RichText::new(format!(
                            "{} {noun} · updated {} · auto 2 s",
                            state.vars.len(),
                            format_age(updated.elapsed())
                        ))
                        .small()
                        .weak(),
                    );
                } else {
                    ui.label(RichText::new("Not read yet").small().weak());
                }
            });
            ui.separator();

            if state.vars.is_empty() {
                if state.backend_ready
                    && !state.blocked_running
                    && !state.loading
                    && state.error.is_none()
                    && state.last_updated.is_some()
                {
                    ui.add_space(8.0);
                    ui.label(RichText::new("No user variables yet.").strong());
                    ui.add_space(2.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "Assign a name in a cell, then refresh. The driver hides Sage's own globals.",
                            )
                            .small()
                            .weak(),
                        )
                        .wrap(),
                    );
                }
                return;
            }

            egui::ScrollArea::vertical()
                .id_salt("sagenote-inspector-vars")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for var in state.vars {
                        ui.push_id(var.name.as_str(), |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(&var.name)
                                        .monospace()
                                        .strong()
                                        .size(13.0),
                                );
                                ui.label(RichText::new(&var.type_name).small().weak());
                            });
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&var.value).monospace().small(),
                                )
                                .wrap(),
                            );
                        });
                        ui.add_space(2.0);
                        ui.separator();
                    }
                });
        });
    action
}

fn format_age(elapsed: Duration) -> String {
    if elapsed.as_secs() < 1 {
        "just now".to_owned()
    } else if elapsed.as_secs() < 60 {
        format!("{}s ago", elapsed.as_secs())
    } else {
        format!("{}m ago", elapsed.as_secs() / 60)
    }
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
        );
        if index + 1 < total {
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
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
) -> bool {
    ui.push_id(cell.id, |ui| {
        let editor_id = egui::Id::new(("sagenote-cell-editor", cell.id.0));
        // Consume Shift+Enter before TextEdit sees it.  Only when this editor is
        // focused, so Enter elsewhere keeps its normal meaning.
        let focused = ui.memory(|memory| memory.has_focus(editor_id));
        let shift_enter =
            focused && ui.input_mut(|input| input.consume_key(Modifiers::SHIFT, Key::Enter));

        let border = if rt.state == CellState::Running {
            RUNNING_BLUE.gamma_multiply(0.55)
        } else {
            CELL_BORDER
        };
        let frame = egui::Frame::default()
            .fill(Color32::WHITE)
            .stroke(Stroke::new(1.0_f32, border))
            .corner_radius(8.0)
            .inner_margin(12.0);
        frame
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("Cell {}", index + 1))
                            .strong()
                            .size(14.0),
                    );
                    ui.add_space(4.0);
                    state_badge(ui, rt);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").on_hover_text("Delete cell").clicked() {
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
                            .small_button("＋")
                            .on_hover_text("Insert a cell below")
                            .clicked()
                        {
                            actions.push(UiAction::AddCell(Some(cell.id)));
                        }
                        if ui
                            .add_enabled(backend_ready, egui::Button::new("Run").small())
                            .on_hover_text("Evaluate this cell (Shift+Enter)")
                            .clicked()
                        {
                            actions.push(UiAction::Run(cell.id));
                        }
                    });
                });

                ui.add_space(4.0);
                let width = ui.available_width();
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    let job = syntax.layout(text, ui, wrap_width);
                    ui.fonts(|fonts| fonts.layout_job(job))
                };
                let response = ui.add(
                    egui::TextEdit::multiline(&mut cell.source)
                        .id(editor_id)
                        .code_editor()
                        .desired_rows(4)
                        .desired_width(width)
                        .background_color(Color32::from_gray(252))
                        .layouter(&mut layouter),
                );
                if *focus_request == Some(cell.id) {
                    response.request_focus();
                    *focus_request = None;
                }
                if response.has_focus() {
                    *last_focused = Some(cell.id);
                }
                let changed = response.changed();
                if changed {
                    // The displayed value no longer corresponds to the source,
                    // and any in-flight run is superseded for routing purposes.
                    rt.mark_stale();
                    rt.run = None;
                    rt.state = CellState::Idle;
                    rt.started = None;
                    rt.elapsed = None;
                    cell.state = CellState::Idle;
                }
                if shift_enter {
                    actions.push(UiAction::Run(cell.id));
                }

                if rt.has_output()
                    || rt.error.is_some()
                    || matches!(
                        rt.state,
                        CellState::Running | CellState::Queued | CellState::Interrupted
                    )
                {
                    ui.add_space(6.0);
                    draw_outputs(ui, rt, renderer);
                }
                changed
            })
            .inner
    })
    .inner
}

fn state_badge(ui: &mut Ui, rt: &CellRuntime) {
    let (label, color) = match rt.state {
        CellState::Idle => ("idle", Color32::from_gray(120)),
        CellState::Queued => ("queued", Color32::from_rgb(60, 105, 160)),
        CellState::Running => ("running", RUNNING_BLUE),
        CellState::Ok => ("ok", Color32::from_rgb(45, 125, 65)),
        CellState::Error => ("error", ERROR_RED),
        CellState::Interrupted => ("interrupted", STDERR_ORANGE),
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
        .fill(color.gamma_multiply(0.12))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(color));
        });
}

fn draw_outputs(ui: &mut Ui, rt: &CellRuntime, renderer: &mut Renderer) {
    egui::Frame::default()
        .fill(OUTPUT_BG)
        .stroke(Stroke::new(1.0_f32, Color32::from_gray(228)))
        .corner_radius(6.0)
        .inner_margin(9.0)
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
                    STALE_GREY
                } else {
                    ERROR_RED
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
                        .color(STDERR_ORANGE),
                );
            }
        });
}

fn draw_live_output(ui: &mut Ui, output: &LiveOutput, renderer: &mut Renderer) {
    match output {
        LiveOutput::Stream { name, text, stale } => {
            let color = if *stale {
                STALE_GREY
            } else if *name == StreamName::Stderr {
                STDERR_ORANGE
            } else {
                Color32::from_rgb(40, 40, 40)
            };
            ui.horizontal_wrapped(|ui| {
                if *name == StreamName::Stderr {
                    ui.label(RichText::new("stderr").small().color(STDERR_ORANGE));
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
                STALE_GREY
            } else {
                Color32::from_rgb(40, 40, 40)
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
                            .color(color),
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
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("unrendered").small().color(ERROR_RED))
                    .on_hover_text(error.to_string());
                ui.label(RichText::new("RaTeX fallback").small().weak());
            });
            let color = if stale { STALE_GREY } else { color };
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
    let ppp = ui.ctx().pixels_per_point();
    match renderer.svg(ui.ctx(), data, ppp) {
        Ok(rendered) => {
            let size = fit_image_size(ui, rendered.size);
            let mut image = egui::Image::new(&rendered.texture).fit_to_exact_size(size);
            if stale {
                image = image.tint(STALE_GREY);
            }
            add_visual(ui, display, image);
        }
        Err(error) => {
            ui.label(
                RichText::new("SVG could not be rendered")
                    .small()
                    .color(ERROR_RED),
            )
            .on_hover_text(error.to_string());
            let preview: String = data.chars().take(240).collect();
            let color = if stale { STALE_GREY } else { color };
            ui.add(
                egui::Label::new(RichText::new(preview).monospace().small().color(color)).wrap(),
            );
        }
    }
}

fn draw_png(ui: &mut Ui, renderer: &mut Renderer, data: &str, display: DisplayMode, stale: bool) {
    let ppp = ui.ctx().pixels_per_point();
    match renderer.png(ui.ctx(), data, ppp) {
        Ok(rendered) => {
            let size = fit_image_size(ui, rendered.size);
            let mut image = egui::Image::new(&rendered.texture).fit_to_exact_size(size);
            if stale {
                image = image.tint(STALE_GREY);
            }
            add_visual(ui, display, image);
        }
        Err(error) => {
            ui.label(
                RichText::new("PNG could not be decoded")
                    .small()
                    .color(ERROR_RED),
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
    ui.label(RichText::new("stale").small().color(STALE_GREY).italics());
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
        let ctx = egui::Context::default();
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
        let ctx = egui::Context::default();
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
        let ctx = egui::Context::default();
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
        let ctx = egui::Context::default();
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
    fn ages_are_compact() {
        assert_eq!(format_age(Duration::from_millis(500)), "just now");
        assert_eq!(format_age(Duration::from_secs(2)), "2s ago");
        assert_eq!(format_age(Duration::from_secs(125)), "2m ago");
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
            last_updated: Some(Instant::now()),
            backend_ready: true,
            starting: false,
            blocked_running: false,
        };
        let ctx = egui::Context::default();
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
            last_updated: None,
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
