//! GUI application state: notebook, cell runtimes, background Sage startup,
//! event routing, file lifecycle, inspector state, native error dialogs and
//! the renderer cache.  The cell workspace itself lives in [`crate::ui`];
//! this file owns state and side effects.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, TryRecvError};
use eframe::egui::{self, ViewportCommand};
use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use sagenote::backend::{
    BackendEvent, CellId, DisplayMode, DriverConfig, EvalBackend, Frame, InterruptScope,
    IntrospectId, MimeBundle, RunId, StdioDriverBackend, StreamName, VarInfo, WakeCallback,
};
use sagenote::cells::{Cell, CellState, Notebook, SavedOutput};
use sagenote::platform::{self, DiscoveryError, DiscoveryFailure, DiscoverySource, SageInstall};
use tokio::runtime::{Handle, Runtime};

use crate::render::Renderer;
use crate::settings::{
    binding_display, default_binding, format_binding, parse_binding, CarbonPalette, KeyPress,
    Keybindings, Settings, ThemeMode, BINDING_META,
};
use crate::ui::{self, MenuState, NotebookUi, StatusState, SyntaxAssets, ToolbarState, UiAction};

/// One live output block.  The notebook's serialized `SavedOutput` is the
/// on-disk projection; this is the display/runtime representation.
#[derive(Clone, Debug)]
pub enum LiveOutput {
    Stream {
        name: StreamName,
        text: String,
        stale: bool,
    },
    Mime {
        bundle: MimeBundle,
        display: DisplayMode,
        stale: bool,
    },
}

impl LiveOutput {
    #[cfg(test)]
    fn stale(&self) -> bool {
        match self {
            LiveOutput::Stream { stale, .. } | LiveOutput::Mime { stale, .. } => *stale,
        }
    }

    fn set_stale(&mut self) {
        match self {
            LiveOutput::Stream { stale, .. } | LiveOutput::Mime { stale, .. } => *stale = true,
        }
    }
}

/// A completion request waiting for the driver's answer.  The source is kept
/// verbatim so a response that arrives after an edit is discarded instead of
/// replacing text the user changed in the meantime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionRequest {
    pub source: String,
    /// Character offset of the caret when Tab was pressed.
    pub offset: usize,
    /// Character offset where the completed token starts.
    pub start: usize,
}

/// A documentation request waiting for the driver's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocRequest {
    pub source: String,
    pub offset: usize,
}

/// The floating documentation window.
#[derive(Clone, Debug)]
pub struct DocPopup {
    pub cell: CellId,
    pub name: String,
    pub type_name: String,
    pub signature: String,
    pub text: String,
}

/// An open Tab-completion popup.  `start..end` is the character range that
/// accepting the selected candidate will replace.
#[derive(Clone, Debug)]
pub struct CompletionPopup {
    pub items: Vec<String>,
    pub selected: usize,
    pub start: usize,
    pub end: usize,
}

/// Runtime state for one cell.  It is deliberately separate from [`Cell`] so a
/// transient `running` state never has to be serialized mid-execution.
#[derive(Clone, Debug, Default)]
pub struct CellRuntime {
    pub state: CellState,
    pub run: Option<RunId>,
    pub started: Option<Instant>,
    pub elapsed: Option<Duration>,
    pub outputs: Vec<LiveOutput>,
    pub error: Option<String>,
    pub error_stale: bool,
    /// Jupyter-style execution count, assigned when a run starts.
    pub execution_count: Option<u64>,
    /// A Tab-completion request sent to the driver and not answered yet.
    pub completion_request: Option<CompletionRequest>,
    /// Candidates currently shown in the completion popup.
    pub completion_popup: Option<CompletionPopup>,
    /// Replacement staged for the next UI frame: (start, end, text).
    pub pending_completion: Option<(usize, usize, String)>,
    /// A documentation request sent to the driver and not answered yet.
    pub doc_request: Option<DocRequest>,
}

impl CellRuntime {
    pub fn from_saved(cell: &Cell) -> Self {
        let outputs = cell
            .outputs
            .iter()
            .map(|saved| {
                let mut bundle = MimeBundle::new();
                bundle.insert_raw(saved.mime.clone(), saved.data.clone());
                LiveOutput::Mime {
                    bundle,
                    display: saved.display,
                    stale: true,
                }
            })
            .collect();
        Self {
            state: CellState::Idle,
            run: None,
            started: None,
            elapsed: None,
            outputs,
            error: None,
            error_stale: false,
            execution_count: cell.execution_count,
            ..Default::default()
        }
    }

    fn begin(&mut self, run: RunId) {
        self.state = CellState::Queued;
        self.run = Some(run);
        self.started = None;
        self.elapsed = None;
        self.outputs.clear();
        self.error = None;
        self.error_stale = false;
        self.execution_count = Some(run.0);
        self.completion_request = None;
        self.completion_popup = None;
        self.pending_completion = None;
        self.doc_request = None;
    }

    fn start(&mut self, run: RunId) {
        if self.run != Some(run) {
            return;
        }
        self.state = CellState::Running;
        self.started = Some(Instant::now());
        self.elapsed = None;
        self.outputs.clear();
        self.error = None;
        self.error_stale = false;
    }

    fn push_stream(&mut self, name: StreamName, text: String) {
        if let Some(LiveOutput::Stream {
            name: last_name,
            text: last_text,
            stale: false,
        }) = self.outputs.last_mut()
        {
            if *last_name == name {
                last_text.push_str(&text);
                return;
            }
        }
        self.outputs.push(LiveOutput::Stream {
            name,
            text,
            stale: false,
        });
    }

    fn push_result(&mut self, bundle: MimeBundle, display: DisplayMode) {
        self.outputs.push(LiveOutput::Mime {
            bundle,
            display,
            stale: false,
        });
    }

    fn set_error(&mut self, traceback: String) {
        self.error = Some(traceback);
        self.error_stale = false;
    }

    fn finish(&mut self, state: CellState) {
        self.elapsed = self.started.take().map(|started| started.elapsed());
        self.state = state;
    }

    pub fn mark_stale(&mut self) {
        for output in &mut self.outputs {
            output.set_stale();
        }
        if self.error.is_some() {
            self.error_stale = true;
        }
    }

    fn persisted_state(&self) -> CellState {
        if self.state.is_transient() {
            CellState::Idle
        } else {
            self.state
        }
    }

    pub fn has_output(&self) -> bool {
        !self.outputs.is_empty() || self.error.is_some()
    }

    fn to_saved_outputs(&self) -> Vec<SavedOutput> {
        let mut saved = Vec::new();
        for output in &self.outputs {
            match output {
                LiveOutput::Stream { text, stale, .. } => saved.push(SavedOutput {
                    mime: "text/plain".into(),
                    data: text.clone(),
                    display: DisplayMode::Block,
                    stale: *stale,
                }),
                LiveOutput::Mime {
                    bundle,
                    display,
                    stale,
                } => {
                    for (mime, data) in bundle.iter() {
                        saved.push(SavedOutput {
                            mime: mime.to_owned(),
                            data: data.to_owned(),
                            display: *display,
                            stale: *stale,
                        });
                    }
                }
            }
        }
        if let Some(error) = &self.error {
            // The .sagenote v1 outputs array has no error kind; keep the
            // traceback as a plain output so save/load never loses it.
            saved.push(SavedOutput {
                mime: "text/plain".into(),
                data: error.clone(),
                display: DisplayMode::Block,
                stale: self.error_stale,
            });
        }
        saved
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupMode {
    Discover,
    Restart,
}

enum StartupResult {
    Ready {
        backend: StdioDriverBackend,
        install: Option<SageInstall>,
    },
    StartFailed {
        backend: StdioDriverBackend,
        install: Option<SageInstall>,
        error: String,
    },
    DiscoveryFailed {
        error: DiscoveryError,
    },
    BuildFailed {
        install: SageInstall,
        error: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingAction {
    New,
    Open,
    Quit,
}

struct ErrorDialog {
    title: String,
    message: String,
    attempts: Vec<DiscoveryFailure>,
    /// Show Retry / Browse / Keep editing instead of a single OK.
    recovery: bool,
}

/// Shortcut recorder state for the settings dialog.
struct Recording {
    /// The binding id (a [`crate::settings::BINDING_META`] entry) being recorded.
    action: &'static str,
    /// Presses captured so far.
    presses: Vec<crate::settings::KeyPress>,
    /// How many presses complete the binding: 1 for a simple binding, the
    /// sequence length for a double-press binding such as `d d`.
    expected: usize,
}

pub struct App {
    notebook: Notebook,
    cells: HashMap<CellId, CellRuntime>,
    backend: Option<StdioDriverBackend>,
    events: Option<Receiver<BackendEvent>>,
    backend_alive: bool,
    starting: bool,
    startup_rx: Option<Receiver<StartupResult>>,
    startup_pgid: Arc<Mutex<Option<Arc<AtomicI32>>>>,
    startup_cancel: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    startup_abort: Arc<AtomicBool>,
    forced_sage: Option<PathBuf>,
    install: Option<SageInstall>,
    sage_path: Option<PathBuf>,
    sage_version: String,
    active: Option<(CellId, RunId)>,
    next_run: u64,
    focus_request: Option<CellId>,
    last_focused: Option<CellId>,
    pending: Option<PendingAction>,
    show_unsaved: bool,
    error_dialog: Option<ErrorDialog>,
    about_open: bool,
    allow_close: bool,
    last_title: String,
    log: String,
    syntax: SyntaxAssets,
    renderer: Renderer,
    show_inspector: bool,
    inspector_vars: Vec<VarInfo>,
    inspector_error: Option<String>,
    inspector_last_request: Option<Instant>,
    introspect_in_flight: Option<IntrospectId>,
    doc_popup: Option<DocPopup>,
    next_introspect: u64,
    tokio: Option<Runtime>,
    tokio_handle: Option<Handle>,
    settings: Settings,
    settings_open: bool,
    shortcuts_open: bool,
    recording: Option<Recording>,
    /// Jupyter command-mode selection: the cell `A`/`B`/`D D` act on.
    selected_cell: Option<CellId>,
    /// Scroll this cell into view on the next frame (set on selection moves).
    scroll_to_cell: Option<CellId>,
    /// `(index, cell)` of the last deleted cell, for command-mode `Z`.
    last_deleted: Option<(usize, Cell)>,
    /// Internal cell cut/copy buffer for command-mode `X`/`C`/`V`.
    cell_clipboard: Option<Cell>,
    /// First key of an in-progress sequence binding (`D D`, `I I`, `0 0`).
    sequence_pending: Option<(egui::Key, Instant)>,
}

/// A backend evaluation can still be active after the user edits or deletes
/// the running cell: the UI runtime no longer says Running, but the evaluation
/// is still on the backend stack.  Treat that as running so the inspector stays
/// paused, Interrupt stays enabled, and New/Open still stops it.
fn is_running(active: Option<(CellId, RunId)>, cells: &HashMap<CellId, CellRuntime>) -> bool {
    active.is_some()
        || cells
            .values()
            .any(|rt| matches!(rt.state, CellState::Running | CellState::Queued))
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let settings = crate::settings::load_settings();
        crate::settings::install_fonts(&cc.egui_ctx);
        crate::settings::apply_with_palette(&settings, &cc.egui_ctx);
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let tokio_handle = tokio.handle().clone();
        let notebook = Notebook::new("unknown");
        let mut cells = HashMap::new();
        for cell in &notebook.cells {
            cells.insert(cell.id, CellRuntime::from_saved(cell));
        }
        let focus_request = notebook.cells.first().map(|cell| cell.id);
        let selected_cell = focus_request;
        let mut app = Self {
            notebook,
            cells,
            backend: None,
            events: None,
            backend_alive: false,
            starting: false,
            startup_rx: None,
            startup_pgid: Arc::new(Mutex::new(None)),
            startup_cancel: Arc::new(Mutex::new(None)),
            startup_abort: Arc::new(AtomicBool::new(false)),
            forced_sage: None,
            install: None,
            sage_path: None,
            sage_version: "unknown".into(),
            active: None,
            next_run: 1,
            focus_request,
            last_focused: None,
            pending: None,
            show_unsaved: false,
            error_dialog: None,
            about_open: false,
            allow_close: false,
            last_title: String::new(),
            log: String::new(),
            syntax: SyntaxAssets::new(),
            renderer: Renderer::new(),
            show_inspector: true,
            inspector_vars: Vec::new(),
            inspector_error: None,
            inspector_last_request: None,
            introspect_in_flight: None,
            doc_popup: None,
            next_introspect: 1,
            tokio: Some(tokio),
            tokio_handle: Some(tokio_handle),
            settings,
            settings_open: false,
            shortcuts_open: false,
            recording: None,
            selected_cell,
            scroll_to_cell: None,
            last_deleted: None,
            cell_clipboard: None,
            sequence_pending: None,
        };
        app.push_log("starting Sage discovery");
        app.start_backend(&cc.egui_ctx, StartupMode::Discover);
        Ok(app)
    }

    fn push_log(&mut self, line: impl AsRef<str>) {
        append_log(&mut self.log, line.as_ref());
    }

    fn backend_ready(&self) -> bool {
        self.backend_alive && self.backend.is_some() && !self.starting
    }

    fn any_running(&self) -> bool {
        is_running(self.active, &self.cells)
    }

    fn running_cell_label(&self) -> Option<String> {
        if let Some((id, _)) = self.active {
            return Some(
                self.notebook
                    .cell_index(id)
                    .map(|index| format!("cell {} running", index + 1))
                    .unwrap_or_else(|| "a deleted cell is running".into()),
            );
        }
        self.cells.iter().find_map(|(id, rt)| {
            (rt.state == CellState::Running).then(|| {
                let index = self.notebook.cell_index(*id).map(|i| i + 1).unwrap_or(0);
                format!("cell {index} running")
            })
        })
    }

    fn start_backend(&mut self, ctx: &egui::Context, mode: StartupMode) {
        if self.startup_rx.is_some() || self.tokio_handle.is_none() {
            return;
        }
        self.starting = true;
        self.backend_alive = false;
        self.error_dialog = None;
        self.clear_inspector(None);
        log::info!("starting Sage ({mode:?})");

        let (tx, rx) = bounded(1);
        self.startup_rx = Some(rx);
        let slot: Arc<Mutex<Option<Arc<AtomicI32>>>> = Arc::new(Mutex::new(None));
        self.startup_pgid = Arc::clone(&slot);
        let cancel_slot: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::new(Mutex::new(None));
        self.startup_cancel = Arc::clone(&cancel_slot);
        self.startup_abort = Arc::new(AtomicBool::new(false));
        let abort = Arc::clone(&self.startup_abort);
        // A missing runtime handle means the app is being torn down; starting
        // Sage from here would leak the process group past Drop.
        let Some(handle) = self.tokio_handle.clone() else {
            self.starting = false;
            self.startup_rx = None;
            return;
        };

        match mode {
            StartupMode::Restart => {
                let Some(mut backend) = self.backend.take() else {
                    self.starting = false;
                    self.startup_rx = None;
                    self.start_backend(ctx, StartupMode::Discover);
                    return;
                };
                self.mark_session_outputs_stale();
                *slot.lock().expect("pgid mutex") = Some(backend.process_group_handle());
                *cancel_slot.lock().expect("cancel mutex") = Some(backend.cancel_handle());
                self.events = Some(backend.events());
                let install = self.install.clone();
                let wake_ctx = ctx.clone();
                handle.spawn_blocking(move || {
                    if abort.load(Ordering::SeqCst) {
                        return;
                    }
                    let result = match backend.restart() {
                        Ok(()) => StartupResult::Ready { backend, install },
                        Err(error) => StartupResult::StartFailed {
                            backend,
                            install,
                            error: format!("{error:#}"),
                        },
                    };
                    let _ = tx.send(result);
                    wake_ctx.request_repaint();
                });
                self.push_log("restarting Sage");
            }
            StartupMode::Discover => {
                // A new discovery replaces any previous backend.  Any outputs
                // and in-flight work it left behind belong to a dead session.
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
                self.backend = None;
                self.events = None;
                let forced = self.forced_sage.take();
                let wake_ctx = ctx.clone();
                let wake: WakeCallback = {
                    let ctx = ctx.clone();
                    Arc::new(move || ctx.request_repaint())
                };
                handle.spawn_blocking(move || {
                    let install = match forced {
                        Some(path) => match platform::validate_sage(&path) {
                            Ok(validated) => {
                                if let Err(error) = platform::save_config_sage(&validated.path) {
                                    log::warn!("could not save sage config: {error:#}");
                                }
                                SageInstall {
                                    path: validated.path,
                                    version: validated.version,
                                    source: DiscoverySource::Explicit,
                                    child_path: None,
                                }
                            }
                            Err(err) => {
                                let _ = tx.send(StartupResult::DiscoveryFailed {
                                    error: DiscoveryError {
                                        attempts: vec![DiscoveryFailure {
                                            stage: "browse",
                                            candidate: path.display().to_string(),
                                            reason: err.reason,
                                        }],
                                    },
                                });
                                wake_ctx.request_repaint();
                                return;
                            }
                        },
                        None => match platform::discover_sage() {
                            Ok(install) => install,
                            Err(error) => {
                                let _ = tx.send(StartupResult::DiscoveryFailed { error });
                                wake_ctx.request_repaint();
                                return;
                            }
                        },
                    };
                    if abort.load(Ordering::SeqCst) {
                        return;
                    }

                    let mut config = DriverConfig::new(install.path.clone());
                    config.sage_path = install.path.clone();
                    config.child_path = install.child_path.clone();
                    config.display = DisplayMode::Block;

                    let mut backend = match StdioDriverBackend::new_with_wake(config, wake) {
                        Ok(backend) => backend,
                        Err(err) => {
                            let _ = tx.send(StartupResult::BuildFailed {
                                install,
                                error: format!("{err:#}"),
                            });
                            wake_ctx.request_repaint();
                            return;
                        }
                    };
                    if abort.load(Ordering::SeqCst) {
                        return;
                    }
                    *slot.lock().expect("pgid mutex") = Some(backend.process_group_handle());
                    *cancel_slot.lock().expect("cancel mutex") = Some(backend.cancel_handle());
                    if abort.load(Ordering::SeqCst) {
                        return;
                    }
                    let result = match backend.start() {
                        Ok(()) => StartupResult::Ready {
                            backend,
                            install: Some(install),
                        },
                        Err(error) => StartupResult::StartFailed {
                            backend,
                            install: Some(install),
                            error: format!("{error:#}"),
                        },
                    };
                    let _ = tx.send(result);
                    wake_ctx.request_repaint();
                });
                self.push_log("discovering and starting sage");
            }
        }
    }

    fn clear_startup_slot(&mut self) {
        *self.startup_pgid.lock().expect("pgid mutex") = None;
        *self.startup_cancel.lock().expect("cancel mutex") = None;
    }

    fn drain_startup(&mut self, ctx: &egui::Context) {
        if self.startup_rx.is_none() {
            return;
        }
        let received = match self.startup_rx.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(())),
            },
            None => None,
        };
        match received {
            None => {}
            Some(Err(())) => {
                self.startup_rx = None;
                self.starting = false;
                self.backend_alive = false;
                self.clear_startup_slot();
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
                self.show_error(
                    "Sage startup failed",
                    "The startup task ended without a result.",
                    Vec::new(),
                    true,
                );
                ctx.request_repaint();
            }
            Some(Ok(result)) => {
                self.startup_rx = None;
                self.starting = false;
                self.clear_startup_slot();
                self.install_startup_result(result, ctx);
            }
        }
    }

    fn install_startup_result(&mut self, result: StartupResult, ctx: &egui::Context) {
        match result {
            StartupResult::Ready { backend, install } => {
                if let Some(install) = install {
                    self.sage_path = Some(install.path.clone());
                    self.sage_version = install.version.clone();
                    self.install = Some(install);
                }
                self.events = Some(backend.events());
                self.backend = Some(backend);
                self.backend_alive = true;
                log::info!("Sage backend ready");
                self.push_log("Sage process ready");
            }
            StartupResult::StartFailed {
                backend,
                install,
                error,
            } => {
                if let Some(install) = install {
                    self.sage_path = Some(install.path.clone());
                    self.sage_version = install.version.clone();
                    self.install = Some(install);
                }
                self.events = Some(backend.events());
                self.backend = Some(backend);
                self.backend_alive = false;
                self.clear_inspector(Some("Sage did not start; variables are unavailable."));
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
                log::error!("Sage start failed: {error}");
                self.push_log(format!("Sage start failed: {error}"));
                self.show_error("Sage did not start", error, Vec::new(), true);
            }
            StartupResult::DiscoveryFailed { error } => {
                log::warn!("Sage discovery failed: {error}");
                self.push_log(error.to_string());
                self.install = None;
                self.backend = None;
                self.events = None;
                self.backend_alive = false;
                self.clear_inspector(Some("Sage is not available."));
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
                let chosen = error.attempts.iter().any(|attempt| {
                    attempt.stage == "browse" && attempt.candidate != "(not attempted)"
                });
                let (title, message) = if chosen {
                    (
                        "Sage executable is not usable",
                        "The sage executable you chose could not be validated.",
                    )
                } else {
                    (
                        "SageMath was not found",
                        "No usable sage executable was discovered.",
                    )
                };
                self.show_error(title, message, error.attempts, true);
            }
            StartupResult::BuildFailed { install, error } => {
                self.sage_path = Some(install.path.clone());
                self.sage_version = install.version.clone();
                self.install = Some(install);
                self.backend = None;
                self.events = None;
                self.backend_alive = false;
                self.clear_inspector(Some("Sage is not available."));
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
                log::error!("Sage backend could not start: {error}");
                self.push_log(format!("Sage backend could not start: {error}"));
                self.show_error("Sage backend error", error, Vec::new(), true);
            }
        }
        ctx.request_repaint();
    }

    fn show_error(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        attempts: Vec<DiscoveryFailure>,
        recovery: bool,
    ) {
        let title = title.into();
        let message = message.into();
        self.push_log(format!("error: {title}: {message}"));
        self.error_dialog = Some(ErrorDialog {
            title,
            message,
            attempts,
            recovery,
        });
    }

    /// Error UI: a native rfd/AppKit/zenity message box.  Recovery dialogs
    /// use Retry / Browse / Keep editing; informational errors are a single OK.
    /// The in-window banner remains the fallback when a desktop has no native
    /// message provider (rfd returns Cancel in that case).
    fn present_native_error(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.error_dialog.take() else {
            return;
        };
        let mut description = dialog.message.clone();
        if !dialog.attempts.is_empty() {
            description.push_str("\n\nDiscovery attempts:");
            for attempt in &dialog.attempts {
                description.push_str(&format!(
                    "\n[{}] {}\n    {}",
                    attempt.stage, attempt.candidate, attempt.reason
                ));
            }
        }
        let buttons = if dialog.recovery {
            MessageButtons::YesNoCancelCustom(
                "Retry".to_owned(),
                "Browse for sage…".to_owned(),
                "Keep editing".to_owned(),
            )
        } else {
            MessageButtons::Ok
        };
        let result = MessageDialog::new()
            .set_level(MessageLevel::Error)
            .set_title(dialog.title.clone())
            .set_description(description)
            .set_buttons(buttons)
            .show();
        self.push_log(format!("error dialog: {} ({result})", dialog.title));
        if !dialog.recovery {
            return;
        }
        match result {
            MessageDialogResult::Yes => self.retry_backend(ctx),
            MessageDialogResult::No => self.browse_sage(ctx),
            MessageDialogResult::Custom(label) if label.eq_ignore_ascii_case("Retry") => {
                self.retry_backend(ctx);
            }
            MessageDialogResult::Custom(label) if label.to_ascii_lowercase().contains("browse") => {
                self.browse_sage(ctx);
            }
            _ => {}
        }
    }

    fn clear_inspector(&mut self, error: Option<&str>) {
        self.introspect_in_flight = None;
        self.inspector_vars.clear();
        self.inspector_last_request = None;
        self.inspector_error = error.map(str::to_owned);
    }

    fn request_inspector(&mut self) {
        if self.introspect_in_flight.is_some() || self.starting || self.any_running() {
            return;
        }
        if !self.backend_ready() {
            self.inspector_error = Some("Sage is not running.".to_owned());
            return;
        }
        let id = IntrospectId(self.next_introspect);
        let started = Instant::now();
        let Some(backend) = self.backend.as_mut() else {
            return;
        };
        match backend.introspect(id) {
            Ok(()) => {
                self.next_introspect += 1;
                self.introspect_in_flight = Some(id);
                self.inspector_last_request = Some(started);
                self.inspector_error = None;
                log::debug!("inspector: requested introspection {id}");
            }
            Err(error) => {
                self.inspector_last_request = Some(started);
                self.inspector_error = Some(format!("Could not read variables: {error:#}"));
                log::warn!("introspection request failed: {error:#}");
                self.push_log(format!("introspection request failed: {error:#}"));
            }
        }
    }

    fn maybe_auto_refresh_inspector(&mut self) {
        if !self.show_inspector
            || self.starting
            || !self.backend_ready()
            || self.any_running()
            || self.introspect_in_flight.is_some()
        {
            return;
        }
        let interval = self.settings.inspector_refresh_secs;
        if interval == 0 {
            return;
        }
        let due = self
            .inspector_last_request
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(interval));
        if due {
            self.request_inspector();
        }
    }

    fn toggle_inspector(&mut self) {
        self.show_inspector = !self.show_inspector;
        if self.show_inspector {
            self.inspector_last_request = None;
            self.request_inspector();
        }
    }

    fn draw_inspector(&mut self, ctx: &egui::Context, backend_ready: bool, running: bool) {
        if !self.show_inspector {
            return;
        }
        let action = {
            let state = ui::InspectorState {
                vars: &self.inspector_vars,
                loading: self.introspect_in_flight.is_some(),
                error: self.inspector_error.as_deref(),
                backend_ready,
                starting: self.starting,
                blocked_running: running,
            };
            ui::inspector_panel(ctx, &state)
        };
        match action {
            Some(ui::InspectorAction::Refresh) => self.request_inspector(),
            Some(ui::InspectorAction::Close) => self.show_inspector = false,
            None => {}
        }
    }

    fn drain_events(&mut self) {
        let Some(rx) = self.events.clone() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(event) => self.handle_backend_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Safety net: the supervisor should have sent ProcessExited
                    // or Fatal first.  If it panicked, do not leave the UI
                    // believing Sage is alive.
                    self.events = None;
                    if !self.starting && self.backend_alive {
                        self.backend_alive = false;
                        self.mark_running_interrupted();
                        self.mark_session_outputs_stale();
                        self.clear_inspector(Some("Sage stopped; variables are unavailable."));
                        self.push_log("backend event channel closed unexpectedly");
                        self.show_error(
                            "Sage stopped",
                            "The Sage backend event channel closed unexpectedly. You can restart Sage, keep editing, or quit.",
                            Vec::new(),
                            true,
                        );
                    }
                    break;
                }
            }
        }
    }

    fn handle_backend_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::Ready {
                sage_path,
                sage_version,
            } => {
                self.backend_alive = true;
                self.sage_path = Some(sage_path.clone());
                self.sage_version = sage_version.clone();
                if self.notebook.meta.sage_version.is_empty()
                    || self.notebook.meta.sage_version == "unknown"
                {
                    self.notebook.meta.sage_version = sage_version.clone();
                }
                log::info!("Sage {sage_version} ready at {}", sage_path.display());
                self.push_log(format!(
                    "ready: Sage {sage_version} ({})",
                    sage_path.display()
                ));
                if self.error_dialog.as_ref().is_some_and(|d| d.recovery) {
                    self.error_dialog = None;
                }
                self.clear_inspector(None);
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
            }
            BackendEvent::Frame(frame) => self.handle_frame(frame),
            BackendEvent::Queued { cell, run } => {
                if let Some(rt) = self.cells.get_mut(&cell) {
                    if rt.run == Some(run) {
                        rt.state = CellState::Queued;
                        self.set_cell_state(cell, CellState::Queued);
                    }
                }
            }
            BackendEvent::Interrupted { cell, run } => {
                if let Some(rt) = self.cells.get_mut(&cell) {
                    if rt.run == Some(run) {
                        rt.finish(CellState::Interrupted);
                        let state = rt.state;
                        self.set_cell_state(cell, state);
                        self.notebook.dirty = true;
                    }
                }
                if self.active == Some((cell, run)) {
                    self.active = None;
                }
            }
            BackendEvent::WatchdogFired { cells, timeout } => {
                self.clear_inspector(Some("Sage is restarting after a watchdog timeout."));
                self.push_log(format!(
                    "watchdog: no end after {timeout:?}; killed and restarting ({} cell(s))",
                    cells.len()
                ));
            }
            BackendEvent::Restarted { sage_version } => {
                self.backend_alive = true;
                self.sage_version = sage_version.clone();
                log::info!("Sage restarted: {sage_version}");
                self.push_log(format!("restarted: Sage {sage_version}"));
                if self.error_dialog.as_ref().is_some_and(|d| d.recovery) {
                    self.error_dialog = None;
                }
                self.clear_inspector(None);
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
            }
            BackendEvent::Completion {
                cell,
                offset,
                items,
            } => {
                self.apply_completion(cell, offset, items);
            }
            BackendEvent::Stderr { text } => {
                self.push_log(format!("[driver stderr] {}", text.trim_end()));
            }
            BackendEvent::ProtocolWarning { message } => {
                self.push_log(format!("[protocol warning] {message}"));
            }
            BackendEvent::ProcessExited { code, message } => {
                self.backend_alive = false;
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
                self.clear_inspector(Some("Sage stopped; variables are unavailable."));
                log::warn!("Sage process exited: {message}");
                self.push_log(format!(
                    "{message}{}",
                    code.map(|c| format!(" (code {c})")).unwrap_or_default()
                ));
                self.show_error(
                    "Sage stopped",
                    format!("{message}. You can restart Sage, keep editing, or quit.",),
                    Vec::new(),
                    true,
                );
            }
            BackendEvent::Fatal { message } => {
                self.backend_alive = false;
                self.mark_running_interrupted();
                self.mark_session_outputs_stale();
                self.clear_inspector(Some("Sage stopped; variables are unavailable."));
                log::error!("Sage fatal: {message}");
                self.push_log(format!("fatal: {message}"));
                self.show_error(
                    "Sage error",
                    format!("{message}. You can restart Sage, keep editing, or quit."),
                    Vec::new(),
                    true,
                );
            }
        }
    }

    fn handle_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Start { id, run } => {
                self.active = Some((id, run));
                if let Some(rt) = self.cells.get_mut(&id) {
                    if rt.run == Some(run) {
                        rt.start(run);
                        self.set_cell_state(id, CellState::Running);
                    }
                }
            }
            Frame::Stream { name, text } => {
                let Some((cell, run)) = self.active else {
                    self.push_log(format!("[{}] {}", name.as_str(), text.trim_end()));
                    return;
                };
                let Some(rt) = self.cells.get_mut(&cell) else {
                    // The cell was deleted mid-run; discard its output rather
                    // than attaching it to a surviving cell.
                    return;
                };
                if rt.run == Some(run) {
                    rt.push_stream(name, text);
                    self.notebook.dirty = true;
                }
            }
            Frame::Result {
                id,
                run,
                bundle,
                display,
            } => {
                if let Some(rt) = self.cells.get_mut(&id) {
                    if rt.run == Some(run) {
                        rt.push_result(bundle, display);
                        self.notebook.dirty = true;
                    }
                }
            }
            Frame::Error { id, run, traceback } => {
                if let Some(rt) = self.cells.get_mut(&id) {
                    if rt.run == Some(run) {
                        rt.set_error(traceback);
                        rt.state = CellState::Error;
                        self.set_cell_state(id, CellState::Error);
                        self.notebook.dirty = true;
                    }
                }
            }
            Frame::End { id, run } => {
                if self.active == Some((id, run)) {
                    self.active = None;
                }
                if let Some(rt) = self.cells.get_mut(&id) {
                    if rt.run == Some(run) {
                        let state = if rt.error.is_some() {
                            CellState::Error
                        } else {
                            CellState::Ok
                        };
                        rt.finish(state);
                        self.set_cell_state(id, state);
                        self.notebook.dirty = true;
                    }
                }
            }
            Frame::Introspect { id, vars } => {
                if self.introspect_in_flight == Some(id) {
                    self.introspect_in_flight = None;
                    let changed = self.inspector_vars != vars;
                    self.inspector_vars = vars;
                    self.inspector_error = None;
                    log::debug!(
                        "inspector: {} variable(s) from introspection {id}",
                        self.inspector_vars.len()
                    );
                    if changed {
                        self.push_log(format!(
                            "inspector: {} variable(s)",
                            self.inspector_vars.len()
                        ));
                    }
                } else {
                    self.push_log(format!("ignored stale introspection {id}"));
                }
            }
            Frame::Complete { .. } => {
                // The supervisor converts `complete` frames into
                // `BackendEvent::Completion`; a raw frame here is unexpected.
                self.push_log("ignored a raw complete frame");
            }
            Frame::Doc {
                id,
                offset,
                name,
                type_name,
                signature,
                text,
            } => {
                self.apply_doc(id, offset, name, type_name, signature, text);
            }
            Frame::Ready { sage_version } => {
                self.push_log(format!("driver ready frame: {sage_version}"));
            }
            Frame::Pong => {}
            Frame::Unknown => self.push_log("ignored an unknown frame type"),
        }
    }

    fn set_cell_state(&mut self, id: CellId, state: CellState) {
        if let Some(cell) = self.notebook.cell_mut(id) {
            cell.state = state;
        }
    }

    /// A restarted/fresh Sage process has none of the old session's globals.
    /// Every previous output is stale, and settled Ok/Error cells are back to
    /// idle; interrupted cells keep their interrupted badge.
    fn mark_session_outputs_stale(&mut self) {
        for rt in self.cells.values_mut() {
            rt.mark_stale();
            // A dead/restarted driver will never answer an outstanding Tab
            // request; clear it so the next Tab is accepted.
            rt.completion_request = None;
            rt.completion_popup = None;
            rt.pending_completion = None;
            rt.doc_request = None;
            if matches!(rt.state, CellState::Ok | CellState::Error) {
                rt.state = CellState::Idle;
            }
        }
        self.doc_popup = None;
    }

    fn mark_running_interrupted(&mut self) {
        let ids: Vec<CellId> = self
            .cells
            .iter()
            .filter_map(|(id, rt)| {
                matches!(rt.state, CellState::Running | CellState::Queued).then_some(*id)
            })
            .collect();
        let changed = !ids.is_empty();
        for id in ids {
            if let Some(rt) = self.cells.get_mut(&id) {
                rt.finish(CellState::Interrupted);
                let state = rt.state;
                self.set_cell_state(id, state);
            }
        }
        self.active = None;
        if changed {
            self.notebook.dirty = true;
        }
    }

    fn run_cell(&mut self, id: CellId) {
        if !self.backend_ready() {
            self.push_log("run ignored: Sage is not running");
            return;
        }
        let Some(source) = self.notebook.cell(id).map(|cell| cell.source.clone()) else {
            return;
        };
        let run = RunId(self.next_run);
        self.next_run += 1;
        if let Some(rt) = self.cells.get_mut(&id) {
            rt.begin(run);
        }
        self.set_cell_state(id, CellState::Queued);
        self.last_focused = Some(id);
        self.selected_cell = Some(id);
        if let Some(backend) = &mut self.backend {
            if let Err(error) = backend.eval(id, run, &source) {
                if let Some(rt) = self.cells.get_mut(&id) {
                    rt.set_error(format!("{error:#}"));
                    rt.finish(CellState::Error);
                }
                self.set_cell_state(id, CellState::Error);
                self.notebook.dirty = true;
                self.push_log(format!("eval request failed: {error:#}"));
            }
        }
    }

    fn run_all(&mut self) {
        let ids: Vec<CellId> = self
            .notebook
            .cells
            .iter()
            .filter(|cell| !cell.source.trim().is_empty())
            .map(|cell| cell.id)
            .collect();
        for id in ids {
            self.run_cell(id);
        }
    }

    fn run_above(&mut self, id: CellId) {
        let Some(index) = self.notebook.cell_index(id) else {
            return;
        };
        let ids: Vec<CellId> = self.notebook.cells[..index]
            .iter()
            .filter(|cell| !cell.source.trim().is_empty())
            .map(|cell| cell.id)
            .collect();
        for id in ids {
            self.run_cell(id);
        }
    }

    fn add_cell(&mut self, after: Option<CellId>) {
        let id = self.notebook.insert_cell_after(after, "");
        self.cells.insert(id, CellRuntime::default());
        self.focus_request = Some(id);
    }

    /// Shift+Enter: run the cell, then select the next cell (Jupyter).  When
    /// the run cell is the last one, a new cell is inserted below it.
    fn run_and_advance(&mut self, id: CellId) {
        self.run_cell(id);
        let Some(index) = self.notebook.cell_index(id) else {
            return;
        };
        match self.notebook.cells.get(index + 1) {
            Some(next) => self.select_cell(next.id),
            None => {
                let new_id = self.notebook.insert_cell_after(Some(id), "");
                self.cells.insert(new_id, CellRuntime::default());
                self.select_cell(new_id);
            }
        }
    }

    /// Alt+Enter: run the cell and insert (and select) a new cell below it.
    fn run_and_insert_below(&mut self, id: CellId) {
        self.run_cell(id);
        let new_id = self.notebook.insert_cell_after(Some(id), "");
        self.cells.insert(new_id, CellRuntime::default());
        self.select_cell(new_id);
    }

    /// Select `id` and scroll it into view on the next frame.
    fn select_cell(&mut self, id: CellId) {
        self.selected_cell = Some(id);
        self.scroll_to_cell = Some(id);
    }

    /// Command-mode `Enter`: enter the editor of the selected cell.
    fn edit_selected(&mut self) {
        if let Some(id) = self.selected_cell {
            self.focus_request = Some(id);
        }
    }

    fn select_prev(&mut self) {
        let Some(id) = self.selected_cell else { return };
        let Some(index) = self.notebook.cell_index(id) else {
            return;
        };
        if index > 0 {
            let prev = self.notebook.cells[index - 1].id;
            self.select_cell(prev);
        }
    }

    fn select_next(&mut self) {
        let Some(id) = self.selected_cell else { return };
        let Some(index) = self.notebook.cell_index(id) else {
            return;
        };
        if let Some(next) = self.notebook.cells.get(index + 1) {
            self.select_cell(next.id);
        }
    }

    /// Command-mode `A`/`B`: insert a cell relative to the selection and
    /// select the new cell.
    fn insert_relative(&mut self, above: bool) {
        let anchor = self
            .selected_cell
            .or_else(|| self.notebook.cells.last().map(|cell| cell.id));
        let Some(anchor) = anchor else {
            return;
        };
        let after = if above {
            let index = self.notebook.cell_index(anchor).unwrap_or(0);
            if index == 0 {
                None
            } else {
                Some(self.notebook.cells[index - 1].id)
            }
        } else {
            Some(anchor)
        };
        let id = self.notebook.insert_cell_after(after, "");
        self.cells.insert(id, CellRuntime::default());
        self.select_cell(id);
    }

    /// Command-mode `Z`: undo the last cell deletion.
    fn undo_delete(&mut self) {
        let Some((index, cell)) = self.last_deleted.take() else {
            return;
        };
        let id = cell.id;
        let index = index.min(self.notebook.cells.len());
        self.notebook.cells.insert(index, cell);
        self.cells.insert(id, CellRuntime::default());
        self.notebook.dirty = true;
        self.select_cell(id);
    }

    /// Command-mode `X` (cut) / `C` (copy) the selected cell.
    fn cut_or_copy_selected(&mut self, cut: bool) {
        let Some(id) = self.selected_cell else { return };
        if cut {
            let Some(index) = self.notebook.cell_index(id) else {
                return;
            };
            if let Some(cell) = self.notebook.remove_cell(id) {
                self.cells.remove(&id);
                self.cell_clipboard = Some(cell.clone());
                self.last_deleted = Some((index, cell));
                self.notebook.dirty = true;
                let next = self
                    .notebook
                    .cells
                    .get(index)
                    .or_else(|| self.notebook.cells.last());
                if let Some(next) = next {
                    self.select_cell(next.id);
                } else {
                    self.selected_cell = None;
                }
            }
        } else if let Some(cell) = self.notebook.cell(id) {
            self.cell_clipboard = Some(cell.clone());
        }
    }

    /// Command-mode `V`/`Shift+V`: paste the clipboard cell below/above the
    /// selection.  The pasted cell gets a fresh id and is selected.
    fn paste_clipboard(&mut self, above: bool) {
        let Some(clipboard_cell) = self.cell_clipboard.clone() else {
            return;
        };
        let anchor = self
            .selected_cell
            .or_else(|| self.notebook.cells.last().map(|cell| cell.id));
        let Some(anchor) = anchor else { return };
        let after = if above {
            let index = self.notebook.cell_index(anchor).unwrap_or(0);
            if index == 0 {
                None
            } else {
                Some(self.notebook.cells[index - 1].id)
            }
        } else {
            Some(anchor)
        };
        let new_id = self.notebook.insert_cell_after(after, "");
        let pasted = Cell {
            id: new_id,
            source: clipboard_cell.source.clone(),
            outputs: clipboard_cell.outputs.clone(),
            state: clipboard_cell.state,
            execution_count: clipboard_cell.execution_count,
        };
        if let Some(cell) = self.notebook.cell_mut(new_id) {
            *cell = pasted;
        }
        self.cells.insert(new_id, CellRuntime::default());
        self.select_cell(new_id);
    }

    fn delete_cell(&mut self, id: CellId) {
        let Some(index) = self.notebook.cell_index(id) else {
            return;
        };
        if let Some(cell) = self.notebook.remove_cell(id) {
            self.last_deleted = Some((index, cell));
        }
        self.cells.remove(&id);
        if self.doc_popup.as_ref().is_some_and(|doc| doc.cell == id) {
            self.doc_popup = None;
        }
        if self.active.map(|(active, _)| active) == Some(id) {
            // Keep the active pair so Stream frames are discarded (the cell is
            // gone) and End clears it; do not attach output to another cell.
        }
        if self.notebook.cells.is_empty() {
            let new_id = self.notebook.add_cell("");
            self.notebook.dirty = true;
            self.cells.insert(new_id, CellRuntime::default());
            self.focus_request = Some(new_id);
            self.selected_cell = Some(new_id);
        } else {
            let next_index = index.min(self.notebook.cells.len() - 1);
            let next_id = self.notebook.cells[next_index].id;
            self.focus_request = Some(next_id);
            self.select_cell(next_id);
        }
    }

    fn move_cell(&mut self, id: CellId, up: bool) {
        let moved = if up {
            self.notebook.move_cell_up(id)
        } else {
            self.notebook.move_cell_down(id)
        };
        if moved {
            self.select_cell(id);
        }
    }

    fn interrupt(&mut self) {
        if let Some(backend) = &mut self.backend {
            if let Err(error) = backend.interrupt(InterruptScope::Cell) {
                self.push_log(format!("interrupt failed: {error:#}"));
            } else {
                self.push_log("SIGINT sent to the Sage process group");
            }
        }
    }

    /// Ask the driver for prefix completions of the token at `offset`.
    ///
    /// The driver is single-threaded, so completion is suppressed while a cell
    /// is queued/running: otherwise the request would sit behind the eval and
    /// the popup could replace text the user has already moved on from.
    fn request_completion(&mut self, cell: CellId, source: String, offset: usize, start: usize) {
        if !self.backend_ready() || self.any_running() {
            return;
        }
        {
            let Some(rt) = self.cells.get_mut(&cell) else {
                return;
            };
            if rt.completion_request.is_some() {
                return;
            }
            rt.completion_popup = None;
            rt.pending_completion = None;
            rt.completion_request = Some(CompletionRequest {
                source: source.clone(),
                offset,
                start,
            });
        }
        let result = match self.backend.as_mut() {
            Some(backend) => backend.completion(cell, &source, offset),
            None => {
                if let Some(rt) = self.cells.get_mut(&cell) {
                    rt.completion_request = None;
                }
                return;
            }
        };
        if let Err(error) = result {
            if let Some(rt) = self.cells.get_mut(&cell) {
                rt.completion_request = None;
            }
            self.push_log(format!("completion request failed: {error:#}"));
        }
    }

    /// Turn a driver answer into either a direct insertion (one candidate) or
    /// the popup.  Stale answers are dropped: the source must still be exactly
    /// what was sent, and the offset must match.
    fn apply_completion(&mut self, cell: CellId, offset: usize, items: Vec<String>) {
        let Some(rt) = self.cells.get_mut(&cell) else {
            return;
        };
        let Some(request) = rt.completion_request.take() else {
            return;
        };
        if request.offset != offset {
            return;
        }
        if self.notebook.cell(cell).map(|c| c.source.as_str()) != Some(request.source.as_str()) {
            return;
        }
        let current: String = request
            .source
            .chars()
            .skip(request.start)
            .take(request.offset - request.start)
            .collect();
        let mut items: Vec<String> = items
            .into_iter()
            .filter(|item| !item.is_empty() && *item != current)
            .collect();
        items.sort();
        items.dedup();
        items.truncate(100);
        match items.len() {
            0 => {}
            1 => rt.pending_completion = Some((request.start, request.offset, items.remove(0))),
            _ => {
                rt.completion_popup = Some(CompletionPopup {
                    items,
                    selected: 0,
                    start: request.start,
                    end: request.offset,
                });
            }
        }
    }

    /// Ask the driver for the docstring of the name at `offset`.  Like
    /// completion, docs share the single driver thread and are suppressed
    /// while a cell is running.
    fn request_docs(&mut self, cell: CellId, source: String, offset: usize) {
        if !self.backend_ready() || self.any_running() {
            return;
        }
        {
            let Some(rt) = self.cells.get_mut(&cell) else {
                return;
            };
            if rt.doc_request.is_some() {
                return;
            }
            rt.doc_request = Some(DocRequest {
                source: source.clone(),
                offset,
            });
        }
        let result = match self.backend.as_mut() {
            Some(backend) => backend.docs(cell, &source, offset),
            None => {
                if let Some(rt) = self.cells.get_mut(&cell) {
                    rt.doc_request = None;
                }
                return;
            }
        };
        if let Err(error) = result {
            if let Some(rt) = self.cells.get_mut(&cell) {
                rt.doc_request = None;
            }
            self.push_log(format!("documentation request failed: {error:#}"));
        }
    }

    /// Open the docs window for a driver answer, unless the cell changed since
    /// the request was made.
    fn apply_doc(
        &mut self,
        cell: CellId,
        offset: usize,
        name: String,
        type_name: String,
        signature: String,
        text: String,
    ) {
        let Some(rt) = self.cells.get_mut(&cell) else {
            return;
        };
        let Some(request) = rt.doc_request.take() else {
            return;
        };
        if request.offset != offset {
            return;
        }
        if self.notebook.cell(cell).map(|c| c.source.as_str()) != Some(request.source.as_str()) {
            return;
        }
        self.doc_popup = Some(DocPopup {
            cell,
            name,
            type_name,
            signature,
            text,
        });
    }

    fn retry_backend(&mut self, ctx: &egui::Context) {
        self.error_dialog = None;
        if self.backend.is_some() {
            self.start_backend(ctx, StartupMode::Restart);
        } else {
            self.start_backend(ctx, StartupMode::Discover);
        }
    }

    fn browse_sage(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Choose the sage executable")
            .pick_file()
        else {
            return;
        };
        if self.startup_rx.is_some() {
            self.push_log("Browse ignored: a Sage startup is already in progress");
            return;
        }
        // Validate and save the config in the background discovery task.  A
        // broken executable can take a long time to answer --version; doing
        // that here would freeze the UI thread for up to VERSION_TIMEOUT.
        self.forced_sage = Some(path);
        self.error_dialog = None;
        self.start_backend(ctx, StartupMode::Discover);
    }

    fn reset_from_notebook(&mut self) {
        self.renderer.clear();
        self.doc_popup = None;
        self.cells.clear();
        for cell in &self.notebook.cells {
            self.cells.insert(cell.id, CellRuntime::from_saved(cell));
        }
        self.active = None;
        self.last_focused = None;
        self.focus_request = self.notebook.cells.first().map(|cell| cell.id);
        self.selected_cell = self.focus_request;
    }

    fn new_notebook(&mut self) {
        if self.any_running() {
            self.interrupt();
        }
        self.notebook = Notebook::new(self.sage_version.clone());
        self.notebook.dirty = false;
        self.reset_from_notebook();
        self.push_log("new notebook");
    }

    fn open_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("sagenote notebook", &["sagenote"])
            .set_title("Open notebook")
            .pick_file()
        else {
            return;
        };
        match Notebook::open(&path) {
            Ok(notebook) => {
                if self.any_running() {
                    self.interrupt();
                }
                self.notebook = notebook;
                self.notebook.dirty = false;
                self.reset_from_notebook();
                self.push_log(format!("opened {}", path.display()));
            }
            Err(error) => self.show_error(
                "Could not open notebook",
                format!("{error:#}"),
                Vec::new(),
                false,
            ),
        }
    }

    fn sync_outputs(&mut self) {
        for cell in &mut self.notebook.cells {
            if let Some(rt) = self.cells.get(&cell.id) {
                cell.outputs = rt.to_saved_outputs();
                cell.state = rt.persisted_state();
                cell.execution_count = rt.execution_count;
            }
        }
    }

    fn save(&mut self, ctx: &egui::Context) -> bool {
        self.sync_outputs();
        let path = match self.notebook.path.clone() {
            Some(path) => path,
            None => match self.save_path_dialog() {
                Some(path) => path,
                None => return false,
            },
        };
        match self.notebook.save_as(&path) {
            Ok(()) => {
                self.notebook.dirty = false;
                self.push_log(format!("saved {}", path.display()));
                ctx.request_repaint();
                true
            }
            Err(error) => {
                self.show_error(
                    "Could not save notebook",
                    format!("{error:#}"),
                    Vec::new(),
                    false,
                );
                false
            }
        }
    }

    fn save_as(&mut self, ctx: &egui::Context) -> bool {
        self.sync_outputs();
        let Some(path) = self.save_path_dialog() else {
            return false;
        };
        match self.notebook.save_as(&path) {
            Ok(()) => {
                self.notebook.dirty = false;
                self.push_log(format!("saved {}", path.display()));
                ctx.request_repaint();
                true
            }
            Err(error) => {
                self.show_error(
                    "Could not save notebook",
                    format!("{error:#}"),
                    Vec::new(),
                    false,
                );
                false
            }
        }
    }

    fn save_path_dialog(&self) -> Option<PathBuf> {
        let suggested = self
            .notebook
            .path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled.sagenote".into());
        let picked = rfd::FileDialog::new()
            .add_filter("sagenote notebook", &["sagenote"])
            .set_file_name(suggested)
            .set_title("Save notebook")
            .save_file()?;
        Some(with_sagenote_extension(picked))
    }

    fn request_pending(&mut self, action: PendingAction, ctx: &egui::Context) {
        if self.notebook.dirty {
            self.pending = Some(action);
            self.show_unsaved = true;
        } else {
            self.perform_pending(action, ctx);
        }
    }

    fn perform_pending(&mut self, action: PendingAction, ctx: &egui::Context) {
        self.pending = None;
        self.show_unsaved = false;
        match action {
            PendingAction::New => self.new_notebook(),
            PendingAction::Open => self.open_dialog(),
            PendingAction::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if self.allow_close {
            return;
        }
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if self.error_dialog.is_some() {
            // Resolve the error first; otherwise the close-confirm would stack
            // another modal on top of it.
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            return;
        }
        if self.notebook.dirty {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.pending = Some(PendingAction::Quit);
            self.show_unsaved = true;
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.any_modal_open() {
            return;
        }
        // Escape closes the documentation window before anything else.
        if self.doc_popup.is_some()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.doc_popup = None;
            return;
        }
        // An open completion popup owns the keyboard (Ctrl+N/Ctrl+P and the
        // configured next/prev bindings must not trigger New/Open first).
        if self.cells.values().any(|rt| rt.completion_popup.is_some()) {
            return;
        }
        let command = egui::Modifiers::COMMAND;

        // Ctrl+= / Ctrl+- / Ctrl+0: UI zoom.  Not user-remappable.
        if ctx.input_mut(|i| i.consume_key(command, egui::Key::Equals)) {
            let factor = (ctx.zoom_factor() * 1.1).min(2.5);
            ctx.set_zoom_factor(factor);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(command, egui::Key::Minus)) {
            let factor = (ctx.zoom_factor() / 1.1).max(0.5);
            ctx.set_zoom_factor(factor);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(command, egui::Key::Num0)) {
            ctx.set_zoom_factor(1.0);
            return;
        }

        // Global bindings, most specific first.
        if ctx.input_mut(|i| i.consume_key(command | egui::Modifiers::SHIFT, egui::Key::S)) {
            self.save_as(ctx);
        } else if ctx.input_mut(|i| i.consume_key(command, egui::Key::S)) {
            self.save(ctx);
        } else if ctx.input_mut(|i| i.consume_key(command, egui::Key::N)) {
            self.request_pending(PendingAction::New, ctx);
        } else if ctx.input_mut(|i| i.consume_key(command, egui::Key::O)) {
            self.request_pending(PendingAction::Open, ctx);
        } else if ctx.input_mut(|i| i.consume_key(command, egui::Key::I)) {
            self.toggle_inspector();
        } else if ctx.input_mut(|i| i.consume_key(command, egui::Key::Period)) {
            self.interrupt();
        } else if ctx.input_mut(|i| {
            i.consume_key(command, egui::Key::Q) || i.consume_key(command, egui::Key::W)
        }) {
            self.request_pending(PendingAction::Quit, ctx);
        } else if ctx.input_mut(|i| i.consume_key(command, egui::Key::Comma)) {
            self.settings_open = true;
        } else if ctx.input_mut(|i| i.consume_key(command, egui::Key::H)) {
            self.shortcuts_open = true;
        } else if self.any_editor_focused(ctx) {
            // Edit mode: `Esc` and the run bindings are consumed in
            // `ui::draw_cell`, which owns the focused editor's response.
        } else {
            self.handle_command_mode_keys(ctx);
        }
    }

    /// Jupyter command mode: no editor focused, one cell selected.
    fn handle_command_mode_keys(&mut self, ctx: &egui::Context) {
        let kb = self.settings.keybindings.clone();
        // Sequences first: D D (delete), I I (interrupt), 0 0 (restart).
        if self.consume_sequence(ctx, kb.get("delete_cell")) {
            if let Some(id) = self.selected_cell {
                self.delete_cell(id);
            }
            return;
        }
        if self.consume_sequence(ctx, kb.get("interrupt_kernel")) {
            self.interrupt();
            return;
        }
        if self.consume_sequence(ctx, kb.get("restart_kernel")) {
            self.handle_action(UiAction::RestartSage, ctx);
            return;
        }

        if self.consume_binding(ctx, kb.get("edit_selected")) {
            self.edit_selected();
            return;
        }
        if self.consume_binding(ctx, kb.get("select_prev")) {
            self.select_prev();
            return;
        }
        if self.consume_binding(ctx, kb.get("select_next")) {
            self.select_next();
            return;
        }
        if self.consume_binding(ctx, kb.get("insert_above")) {
            self.insert_relative(true);
            return;
        }
        if self.consume_binding(ctx, kb.get("insert_below")) {
            self.insert_relative(false);
            return;
        }
        if self.consume_binding(ctx, kb.get("undo_delete")) {
            self.undo_delete();
            return;
        }
        if self.consume_binding(ctx, kb.get("cut_cell")) {
            self.cut_or_copy_selected(true);
            return;
        }
        if self.consume_binding(ctx, kb.get("copy_cell")) {
            self.cut_or_copy_selected(false);
            return;
        }
        if self.consume_binding(ctx, kb.get("paste_below")) {
            self.paste_clipboard(false);
            return;
        }
        if self.consume_binding(ctx, kb.get("paste_above")) {
            self.paste_clipboard(true);
            return;
        }
        if self.consume_binding(ctx, kb.get("show_shortcuts_command")) {
            self.shortcuts_open = true;
            return;
        }

        // Run bindings also work in command mode.
        if self.consume_binding(ctx, kb.get("run_and_advance")) {
            if let Some(id) = self.selected_cell {
                self.run_and_advance(id);
            }
            return;
        }
        if self.consume_binding(ctx, kb.get("run_in_place")) {
            if let Some(id) = self.selected_cell {
                self.run_cell(id);
            }
            return;
        }
        if self.consume_binding(ctx, kb.get("run_and_insert_below")) {
            if let Some(id) = self.selected_cell {
                self.run_and_insert_below(id);
            }
        }
    }

    /// True when any cell editor currently has keyboard focus.
    fn any_editor_focused(&self, ctx: &egui::Context) -> bool {
        self.notebook.cells.iter().any(|cell| {
            ctx.memory(|memory| {
                memory.has_focus(egui::Id::new(("sagenote-cell-editor", cell.id.0)))
            })
        })
    }

    /// True when a modal dialog is open (shortcuts are suppressed).
    fn any_modal_open(&self) -> bool {
        self.error_dialog.is_some()
            || self.show_unsaved
            || self.about_open
            || self.settings_open
            || self.shortcuts_open
            || self.recording.is_some()
    }

    /// Consume a single-press binding from this frame's input.
    fn consume_binding(&self, ctx: &egui::Context, binding: &str) -> bool {
        let Some(presses) = crate::settings::parse_binding(binding) else {
            return false;
        };
        let Some(press) = presses.first() else {
            return false;
        };
        ctx.input_mut(|i| i.consume_key(press.modifiers, press.key))
    }

    /// Feed a sequence binding (e.g. `"d d"`): the first press arms the
    /// sequence, the second press within [`SEQUENCE_TIMEOUT`] fires it.
    fn consume_sequence(&mut self, ctx: &egui::Context, binding: &str) -> bool {
        let Some(presses) = crate::settings::parse_binding(binding) else {
            return false;
        };
        // A double-press binding (`"d d"`) fires on the second press;
        // anything else behaves as a single-press binding.
        let double = presses.len() == 2 && presses[0] == presses[1];
        if !double {
            return self.consume_binding(ctx, binding);
        }
        let first = presses[0];
        if !ctx.input_mut(|i| i.consume_key(first.modifiers, first.key)) {
            return false;
        }
        match self.sequence_pending.take() {
            // Second press of the same key within the timeout: the binding fires.
            Some((key, at)) if key == first.key && at.elapsed() < SEQUENCE_TIMEOUT => true,
            // First press (or a stale/different pending): arm the sequence.
            _ => {
                self.sequence_pending = Some((first.key, Instant::now()));
                false
            }
        }
    }

    fn handle_action(&mut self, action: UiAction, ctx: &egui::Context) {
        match action {
            UiAction::New => self.request_pending(PendingAction::New, ctx),
            UiAction::Open => self.request_pending(PendingAction::Open, ctx),
            UiAction::Save => {
                self.save(ctx);
            }
            UiAction::SaveAs => {
                self.save_as(ctx);
            }
            UiAction::Quit => self.request_pending(PendingAction::Quit, ctx),
            UiAction::AddCell(after) => self.add_cell(after),
            UiAction::DeleteCell(id) => self.delete_cell(id),
            UiAction::MoveUp(id) => self.move_cell(id, true),
            UiAction::MoveDown(id) => self.move_cell(id, false),
            UiAction::Run(id) => self.run_cell(id),
            UiAction::RunAndAdvance(id) => self.run_and_advance(id),
            UiAction::RunAndInsertBelow(id) => self.run_and_insert_below(id),
            UiAction::ToCommandMode(id) => {
                // The editor surrendered focus (ui::draw_cell); select it.
                self.selected_cell = Some(id);
            }
            UiAction::RunAll => self.run_all(),
            UiAction::RunAbove(id) => self.run_above(id),
            UiAction::Interrupt => self.interrupt(),
            UiAction::RestartSage => {
                self.error_dialog = None;
                if self.backend.is_some() {
                    self.start_backend(ctx, StartupMode::Restart);
                } else {
                    self.start_backend(ctx, StartupMode::Discover);
                }
            }
            UiAction::ToggleInspector => self.toggle_inspector(),
            UiAction::RefreshInspector => self.request_inspector(),
            UiAction::RequestCompletion {
                cell,
                source,
                offset,
                start,
            } => self.request_completion(cell, source, offset, start),
            UiAction::RequestDocs {
                cell,
                source,
                offset,
            } => self.request_docs(cell, source, offset),
            UiAction::ShowAbout => self.about_open = true,
            UiAction::ShowSettings => self.settings_open = true,
            UiAction::ShowShortcuts => self.shortcuts_open = true,
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let marker = if self.notebook.dirty { " *" } else { "" };
        let title = format!("sagenote — {}{marker}", self.notebook.title());
        if title != self.last_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }

    fn draw_startup_banner(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.starting {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("Starting Sage…").strong());
                ui.label(
                    egui::RichText::new("the first launch of a signed macOS .app can take 10–20 s")
                        .weak(),
                );
            });
            ui.add_space(4.0);
        } else if !self.backend_alive {
            let p = CarbonPalette::of(ui);
            egui::Frame::default()
                .fill(p.danger_bg)
                .stroke(egui::Stroke::new(1.0_f32, p.danger))
                .corner_radius(0.0)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new("Sage is not running.")
                                .strong()
                                .color(p.danger),
                        );
                        if ui.button("Retry").clicked() {
                            self.retry_backend(ctx);
                        }
                        if ui.button("Browse for sage…").clicked() {
                            self.browse_sage(ctx);
                        }
                        if ui
                            .add_enabled(self.backend.is_some(), egui::Button::new("Restart"))
                            .clicked()
                        {
                            self.handle_action(UiAction::RestartSage, ctx);
                        }
                    });
                });
            ui.add_space(4.0);
        }
    }

    fn draw_unsaved_modal(&mut self, ctx: &egui::Context) {
        if !self.show_unsaved {
            return;
        }
        let mut action: Option<&'static str> = None;
        egui::Modal::new(egui::Id::new("sagenote-unsaved-modal")).show(ctx, |ui| {
            ui.set_min_width(420.0);
            ui.heading("Unsaved changes");
            ui.add_space(4.0);
            ui.label(format!(
                "\"{}\" has unsaved changes.",
                self.notebook.title()
            ));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    action = Some("save");
                }
                if ui.button("Discard").clicked() {
                    action = Some("discard");
                }
                if ui.button("Cancel").clicked() {
                    action = Some("cancel");
                }
            });
        });
        match action {
            Some("save") => {
                if self.save(ctx) {
                    if let Some(pending) = self.pending {
                        self.perform_pending(pending, ctx);
                    }
                }
            }
            Some("discard") => {
                if let Some(pending) = self.pending {
                    self.perform_pending(pending, ctx);
                } else {
                    self.show_unsaved = false;
                }
            }
            Some("cancel") => {
                self.pending = None;
                self.show_unsaved = false;
            }
            _ => {}
        }
    }

    fn draw_about_modal(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let mut close = false;
        egui::Modal::new(egui::Id::new("sagenote-about-modal")).show(ctx, |ui| {
            ui.set_min_width(460.0);
            ui.heading("sagenote");
            ui.label("A Rust-native SageMath notebook, wxMaxima-flavoured.");
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Every pixel is egui; every calculation is an external Sage. \
                     No WebView, no JavaScript, no bundled SageMath.",
                )
                .weak(),
            );
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(format!("sagenote — version {}", env!("CARGO_PKG_VERSION")));
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Shift+Enter run  ·  Ctrl/Cmd I variables  ·  Ctrl/Cmd S save")
                    .small()
                    .weak(),
            );
            ui.add_space(10.0);
            if ui.button("Close").clicked() {
                close = true;
            }
        });
        if close {
            self.about_open = false;
        }
    }

    fn draw_settings_modal(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut close = false;
        let mut changed = false;

        // Shortcut recorder: capture key presses before the dialog's widgets
        // can see them.  Escape cancels; enough presses complete the binding.
        let mut completed: Option<(&'static str, Vec<KeyPress>)> = None;
        let mut cancelled = false;
        if let Some(recording) = &mut self.recording {
            let mut fresh: Vec<KeyPress> = Vec::new();
            ctx.input(|i| {
                for event in &i.events {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } = event
                    {
                        fresh.push(KeyPress {
                            key: *key,
                            modifiers: *modifiers,
                        });
                    }
                }
            });
            for press in fresh {
                // Consume the captured key so sliders and buttons in this
                // dialog do not also react to it.
                ctx.input_mut(|i| i.consume_key(press.modifiers, press.key));
                if press.key == egui::Key::Escape {
                    cancelled = true;
                    break;
                }
                recording.presses.push(press);
                if recording.presses.len() >= recording.expected {
                    completed = Some((recording.action, recording.presses.clone()));
                    break;
                }
            }
        }
        if cancelled || completed.is_some() {
            self.recording = None;
        }
        if let Some((action, presses)) = completed {
            let value = format_binding(&presses);
            self.settings.keybindings.set(action, value);
            changed = true;
        }

        egui::Modal::new(egui::Id::new("sagenote-settings-modal")).show(ctx, |ui| {
            ui.set_min_width(540.0);
            ui.heading("Settings");
            ui.add_space(6.0);

            ui.strong("Appearance");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Theme:");
                let r = ui.radio_value(&mut self.settings.theme, ThemeMode::System, "System");
                changed |= r.changed();
                let r = ui.radio_value(&mut self.settings.theme, ThemeMode::Light, "Light");
                changed |= r.changed();
                let r = ui.radio_value(&mut self.settings.theme, ThemeMode::Dark, "Dark");
                changed |= r.changed();
            });
            let r = ui.add(
                egui::Slider::new(&mut self.settings.ui_font_size, 12.0..=24.0)
                    .text("UI font size (pt)"),
            );
            changed |= r.changed();
            let r = ui.add(
                egui::Slider::new(&mut self.settings.code_font_size, 10.0..=22.0)
                    .text("Code font size (pt)"),
            );
            changed |= r.changed();

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            ui.strong("Keyboard shortcuts");
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt("sagenote-settings-bindings")
                .max_height(320.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new("sagenote-settings-bindings-grid")
                        .num_columns(4)
                        .striped(true)
                        .min_row_height(24.0)
                        .show(ui, |ui| {
                            for meta in BINDING_META {
                                let binding = self.settings.keybindings.get(meta.id).to_owned();
                                ui.label(meta.label);
                                ui.monospace(binding_display(&binding));
                                let is_recording = self
                                    .recording
                                    .as_ref()
                                    .is_some_and(|rec| rec.action == meta.id);
                                let label = if is_recording {
                                    "Press keys\u{2026}"
                                } else {
                                    "Change"
                                };
                                let button = egui::Button::new(label).small();
                                let response = if is_recording {
                                    ui.add_enabled(false, button)
                                } else {
                                    ui.add(button)
                                };
                                if response.clicked() {
                                    let expected = parse_binding(default_binding(meta.id))
                                        .map(|presses| presses.len())
                                        .unwrap_or(1);
                                    self.recording = Some(Recording {
                                        action: meta.id,
                                        presses: Vec::new(),
                                        expected,
                                    });
                                }
                                let clear = ui.add_enabled(
                                    !is_recording && !binding.is_empty(),
                                    egui::Button::new("Clear").small(),
                                );
                                if clear.clicked() {
                                    self.settings.keybindings.set(meta.id, "");
                                    changed = true;
                                }
                                ui.end_row();
                            }
                        });
                });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Reset shortcuts to Jupyter defaults").clicked() {
                    self.settings.keybindings = Keybindings::jupyter();
                    changed = true;
                }
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Shortcuts follow Jupyter: Esc enters command mode, then \
                         B/A insert cells, D D deletes, Z undoes, X/C/V \
                         cut/copy/paste. Enter returns to the editor.",
                )
                .small()
                .weak(),
            );

            ui.add_space(10.0);
            if ui.button("Close").clicked() {
                close = true;
            }
            // Escape closes the dialog when not recording.
            if self.recording.is_none()
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                close = true;
            }
        });
        if close {
            self.settings_open = false;
            self.recording = None;
        }
        if changed {
            crate::settings::apply_with_palette(&self.settings, ctx);
            if let Err(error) = crate::settings::save_settings(&self.settings) {
                self.push_log(format!("could not save settings: {error:#}"));
            }
        }
    }

    fn draw_docs_window(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.doc_popup.clone() else {
            return;
        };
        let p = if ctx.theme() == egui::Theme::Dark {
            CarbonPalette::dark()
        } else {
            CarbonPalette::light()
        };
        let mut close = false;
        let title = if doc.name.is_empty() {
            "Documentation".to_owned()
        } else {
            doc.name.clone()
        };
        egui::Window::new("Documentation")
            .id(egui::Id::new("sagenote-docs"))
            .default_width(560.0)
            .default_height(380.0)
            .resizable(true)
            .collapsible(false)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(p.bg)
                    .stroke(egui::Stroke::new(1.0_f32, p.border_subtle))
                    .corner_radius(0.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&title)
                            .font(ui::semibold(16.0))
                            .color(p.text_primary),
                    );
                    if !doc.type_name.is_empty() {
                        ui.label(
                            egui::RichText::new(&doc.type_name)
                                .small()
                                .color(p.text_helper),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("×")
                            .on_hover_text("Close documentation")
                            .clicked()
                        {
                            close = true;
                        }
                    });
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
                                    egui::RichText::new(&doc.signature)
                                        .monospace()
                                        .color(p.text_secondary),
                                )
                                .wrap(),
                            );
                        });
                }
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .id_salt("sagenote-docs-body")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        draw_doc_text(ui, &doc.text, &p);
                    });
            });
        if close {
            self.doc_popup = None;
        }
    }

    fn draw_shortcuts_modal(&mut self, ctx: &egui::Context) {
        if !self.shortcuts_open {
            return;
        }
        let mut close = false;
        egui::Modal::new(egui::Id::new("sagenote-shortcuts-modal")).show(ctx, |ui| {
            ui.set_min_width(560.0);
            ui.heading("Keyboard shortcuts");
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Two modes, as in Jupyter: command mode (no cell focused) and \
                         edit mode (typing in a cell). `Esc` switches to command mode, \
                         `Enter` enters the editor.",
                )
                .small()
                .weak(),
            );
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .id_salt("sagenote-shortcuts-list")
                .max_height(420.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let mut last_group = "";
                    for meta in BINDING_META {
                        if meta.group != last_group {
                            ui.add_space(6.0);
                            ui.strong(meta.group);
                            last_group = meta.group;
                        }
                        let binding = self.settings.keybindings.get(meta.id);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(binding_display(binding))
                                    .monospace()
                                    .strong(),
                            );
                            ui.label(meta.label);
                        });
                    }
                });
            ui.add_space(10.0);
            if ui.button("Close").clicked() {
                close = true;
            }
        });
        if close {
            self.shortcuts_open = false;
        }
    }
}

impl eframe::App for App {
    /// Match the window clear colour to the Carbon panel fill.  eframe's
    /// default is near-black, which showed through anywhere a panel did not
    /// cover (and behind the transparent central panel) even in light mode.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_startup(ctx);
        self.drain_events();
        self.present_native_error(ctx);
        self.handle_close_request(ctx);
        self.handle_shortcuts(ctx);
        self.maybe_auto_refresh_inspector();

        let backend_ready = self.backend_ready();
        let running = self.any_running();
        let menu_state = MenuState {
            backend_ready,
            starting: self.starting,
            running,
            can_run_above: self
                .last_focused
                .and_then(|id| self.notebook.cell_index(id))
                .is_some_and(|index| index > 0),
            run_above_id: self.last_focused,
            can_restart: !self.starting,
            inspector_open: self.show_inspector,
            can_refresh_inspector: backend_ready && !running && self.introspect_in_flight.is_none(),
        };
        let mut actions = ui::menu_bar(ctx, &menu_state);
        actions.extend(ui::toolbar(
            ctx,
            &ToolbarState {
                backend_ready: menu_state.backend_ready,
                running: menu_state.running,
            },
        ));

        let status_state = StatusState {
            starting: self.starting,
            backend_alive: self.backend_alive,
            sage_path: self.sage_path.clone(),
            sage_version: self.sage_version.clone(),
            notebook_path: self.notebook.path.clone(),
            notebook_title: self.notebook.title(),
            dirty: self.notebook.dirty,
            cells: self.notebook.cells.len(),
            running: self.running_cell_label(),
        };
        ui::status_bar(ctx, &status_state);
        self.draw_inspector(ctx, backend_ready, running);

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(12)))
            .show(ctx, |ui| {
                self.draw_startup_banner(ui, ctx);
                let changed = egui::ScrollArea::vertical()
                    .id_salt("sagenote-cells")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui::notebook_ui(
                            ui,
                            &mut self.notebook,
                            NotebookUi {
                                runtime: &mut self.cells,
                                focus_request: &mut self.focus_request,
                                last_focused: &mut self.last_focused,
                                backend_ready,
                                syntax: &self.syntax,
                                renderer: &mut self.renderer,
                                actions: &mut actions,
                                selected: self.selected_cell,
                                bindings: &self.settings.keybindings,
                                scroll_to_cell: &mut self.scroll_to_cell,
                            },
                        )
                    })
                    .inner;
                if changed {
                    self.notebook.dirty = true;
                }
                ui.add_space(8.0);
                if ui.button("+ Add cell").clicked() {
                    actions.push(UiAction::AddCell(None));
                }
                if !self.log.is_empty() {
                    ui.add_space(8.0);
                    egui::CollapsingHeader::new("Driver log")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(self.log.trim_end()).monospace().small(),
                                )
                                .wrap(),
                            );
                        });
                }
            });

        for action in actions {
            self.handle_action(action, ctx);
        }
        self.draw_docs_window(ctx);
        if self.error_dialog.is_none() {
            self.draw_unsaved_modal(ctx);
            self.draw_about_modal(ctx);
            self.draw_settings_modal(ctx);
            self.draw_shortcuts_modal(ctx);
        }
        self.update_title(ctx);

        if self.starting || self.any_running() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.show_inspector && backend_ready {
            let interval = Duration::from_secs(self.settings.inspector_refresh_secs.max(1));
            ctx.request_repaint_after(interval);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // If a start/restart task owns the backend, its pgid/cancel flag are
        // only reachable through these shared slots.  Cancel first so the
        // supervisor does not spawn the sage -c fallback after we kill the
        // current primary, then SIGKILL whatever process is running.
        self.startup_abort.store(true, Ordering::SeqCst);
        if let Some(cancel) = self.startup_cancel.lock().expect("cancel mutex").take() {
            cancel.store(true, Ordering::SeqCst);
        }
        if let Some(handle) = self.startup_pgid.lock().expect("pgid mutex").take() {
            let pid = handle.load(Ordering::SeqCst);
            if pid > 0 {
                // SAFETY: libc::kill only reads its two arguments, and both
                // are valid: SIGKILL is a libc constant, and the pgid was
                // stored by the supervisor after a successful spawn, so the
                // negative id names a process group we created.  Nothing is
                // borrowed across the call, so no aliasing is possible.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
        self.backend.take();
        if let Some(runtime) = self.tokio.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

const LOG_LIMIT: usize = 32 * 1024;

/// Render a plain docstring with light IDE-style formatting: RST section
/// headings become semi-bold labels, indented/`>>>` blocks are monospaced on a
/// Gray 10 code surface, and prose is wrapped normally.
fn draw_doc_text(ui: &mut egui::Ui, text: &str, p: &CarbonPalette) {
    let text = crate::text::strip_ansi(text);
    if text.trim().is_empty() {
        ui.label(egui::RichText::new("No documentation.").color(p.text_helper));
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
                    egui::RichText::new(line.trim())
                        .font(ui::semibold(14.0))
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
                            egui::RichText::new(block.trim_end())
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
            ui.add(
                egui::Label::new(egui::RichText::new(block.trim_end()).color(p.text_primary))
                    .wrap(),
            );
        }
    }
}

/// How long the first key of a sequence binding (`D D`) stays armed.
const SEQUENCE_TIMEOUT: Duration = Duration::from_secs(1);

fn append_log(log: &mut String, line: &str) {
    if !log.is_empty() && !log.ends_with('\n') {
        log.push('\n');
    }
    log.push_str(line);
    if !line.ends_with('\n') {
        log.push('\n');
    }
    if log.len() > LOG_LIMIT {
        let mut cut = log.len() - LOG_LIMIT;
        while cut < log.len() && !log.is_char_boundary(cut) {
            cut += 1;
        }
        log.drain(..cut);
    }
}

fn with_sagenote_extension(path: PathBuf) -> PathBuf {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("sagenote") | Some("ipynb") => path,
        _ => {
            let mut name = path
                .file_name()
                .map(|name| name.to_os_string())
                .unwrap_or_else(|| "notebook".into());
            name.push(".sagenote");
            path.with_file_name(name)
        }
    }
}

/// Unit-testable helpers live in the binary crate; they need no GUI context.
#[cfg(test)]
mod tests {
    use super::*;

    fn saved(mime: &str, data: &str) -> SavedOutput {
        SavedOutput {
            mime: mime.into(),
            data: data.into(),
            display: DisplayMode::Block,
            stale: false,
        }
    }

    #[test]
    fn loaded_outputs_are_stale_and_idle() {
        let cell = Cell::with_id(CellId::new(), "1+1");
        let mut cell = cell;
        cell.outputs = vec![saved("text/latex", "2")];
        let rt = CellRuntime::from_saved(&cell);
        assert_eq!(rt.state, CellState::Idle);
        assert!(rt.outputs[0].stale());
        assert_eq!(rt.to_saved_outputs()[0].data, "2");
    }

    #[test]
    fn stream_and_error_are_persisted_as_text() {
        let mut rt = CellRuntime::default();
        rt.push_stream(StreamName::Stdout, "hello\n".into());
        rt.set_error("Traceback: boom".into());
        let saved = rt.to_saved_outputs();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].mime, "text/plain");
        assert_eq!(saved[0].data, "hello\n");
        assert!(saved[1].data.contains("boom"));

        rt.mark_stale();
        assert!(rt.error_stale);
        let stale = rt.to_saved_outputs();
        assert!(stale.iter().all(|output| output.stale));
    }

    #[test]
    fn transient_states_become_idle_when_saved() {
        let mut rt = CellRuntime {
            state: CellState::Running,
            ..Default::default()
        };
        assert_eq!(rt.persisted_state(), CellState::Idle);
        rt.state = CellState::Ok;
        assert_eq!(rt.persisted_state(), CellState::Ok);
    }

    #[test]
    fn active_backend_run_counts_as_running() {
        let id = CellId::new();
        assert!(is_running(Some((id, RunId(7))), &HashMap::new()));

        let mut cells = HashMap::new();
        cells.insert(
            id,
            CellRuntime {
                state: CellState::Running,
                ..Default::default()
            },
        );
        assert!(is_running(None, &cells));

        if let Some(rt) = cells.get_mut(&id) {
            rt.state = CellState::Idle;
        }
        assert!(!is_running(None, &cells));
    }

    #[test]
    fn extension_is_added_only_when_missing() {
        assert_eq!(
            with_sagenote_extension(PathBuf::from("/tmp/a")),
            PathBuf::from("/tmp/a.sagenote")
        );
        assert_eq!(
            with_sagenote_extension(PathBuf::from("/tmp/a.sagenote")),
            PathBuf::from("/tmp/a.sagenote")
        );
        assert_eq!(
            with_sagenote_extension(PathBuf::from("/tmp/a.txt")),
            PathBuf::from("/tmp/a.txt.sagenote")
        );
    }

    #[test]
    fn doc_text_renders_headless() {
        let ctx = egui::Context::default();
        crate::settings::install_fonts(&ctx);
        let p = crate::settings::CarbonPalette::light();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                draw_doc_text(
                    ui,
                    "plot(x)\n=======\n\nPlot a function.\n\n    plot(sin(x))\n    plot(cos(x))\n",
                    &p,
                );
            });
        });
    }

    #[test]
    fn log_is_bounded_and_keeps_the_newest_lines() {
        let mut log = String::new();
        for i in 0..20_000 {
            append_log(&mut log, &format!("line {i}"));
        }
        assert!(log.len() <= LOG_LIMIT);
        assert!(log.contains("line 19999"));
        assert!(!log.contains("line 0\n"));
    }
}
