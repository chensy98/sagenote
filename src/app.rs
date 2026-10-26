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
use crate::ui::{self, MenuState, NotebookUi, StatusState, SyntaxAssets, UiAction};

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
    inspector_last_update: Option<Instant>,
    introspect_in_flight: Option<IntrospectId>,
    next_introspect: u64,
    tokio: Option<Runtime>,
    tokio_handle: Option<Handle>,
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
        configure_style(&cc.egui_ctx);
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
            inspector_last_update: None,
            introspect_in_flight: None,
            next_introspect: 1,
            tokio: Some(tokio),
            tokio_handle: Some(tokio_handle),
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
        self.inspector_last_update = None;
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
        let due = self
            .inspector_last_request
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(2));
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
                last_updated: self.inspector_last_update,
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
            BackendEvent::Completion { cell, offset, .. } => {
                self.push_log(format!(
                    "completion hook: cell={cell} offset={offset} (no LSP in this prototype)"
                ));
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
                    self.inspector_last_update = Some(Instant::now());
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
            if matches!(rt.state, CellState::Ok | CellState::Error) {
                rt.state = CellState::Idle;
            }
        }
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

    fn delete_cell(&mut self, id: CellId) {
        let Some(index) = self.notebook.cell_index(id) else {
            return;
        };
        self.notebook.remove_cell(id);
        self.cells.remove(&id);
        self.renderer.clear();
        if self.active.map(|(active, _)| active) == Some(id) {
            // Keep the active pair so Stream frames are discarded (the cell is
            // gone) and End clears it; do not attach output to another cell.
        }
        if self.notebook.cells.is_empty() {
            let new_id = self.notebook.add_cell("");
            self.notebook.dirty = true;
            self.cells.insert(new_id, CellRuntime::default());
            self.focus_request = Some(new_id);
        } else {
            let next = index.min(self.notebook.cells.len() - 1);
            self.focus_request = Some(self.notebook.cells[next].id);
        }
    }

    fn move_cell(&mut self, id: CellId, up: bool) {
        let moved = if up {
            self.notebook.move_cell_up(id)
        } else {
            self.notebook.move_cell_down(id)
        };
        if moved {
            self.focus_request = Some(id);
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
        self.cells.clear();
        for cell in &self.notebook.cells {
            self.cells.insert(cell.id, CellRuntime::from_saved(cell));
        }
        self.active = None;
        self.last_focused = None;
        self.focus_request = self.notebook.cells.first().map(|cell| cell.id);
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
        if self.error_dialog.is_some() || self.show_unsaved || self.about_open {
            return;
        }
        let command = egui::Modifiers::COMMAND;
        let command_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;

        if ctx.input_mut(|i| i.consume_key(command_shift, egui::Key::S)) {
            self.save_as(ctx);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(command, egui::Key::S)) {
            self.save(ctx);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(command, egui::Key::N)) {
            self.request_pending(PendingAction::New, ctx);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(command, egui::Key::O)) {
            self.request_pending(PendingAction::Open, ctx);
            return;
        }
        if ctx.input_mut(|i| i.consume_key(command, egui::Key::I)) {
            self.toggle_inspector();
            return;
        }
        if ctx.input_mut(|i| i.consume_key(command, egui::Key::Period)) {
            self.interrupt();
            return;
        }
        if ctx.input_mut(|i| {
            i.consume_key(command, egui::Key::Q) || i.consume_key(command, egui::Key::W)
        }) {
            self.request_pending(PendingAction::Quit, ctx);
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
            UiAction::ShowAbout => self.about_open = true,
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
            egui::Frame::default()
                .fill(egui::Color32::from_rgb(255, 243, 235))
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    egui::Color32::from_rgb(240, 190, 160),
                ))
                .corner_radius(6.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new("Sage is not running.")
                                .strong()
                                .color(egui::Color32::from_rgb(160, 50, 30)),
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
                egui::RichText::new("Shift+Enter run  ·  Ctrl/⌘ I variables  ·  Ctrl/⌘ S save")
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
}

impl eframe::App for App {
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
            .frame(
                egui::Frame::default()
                    .fill(egui::Color32::from_gray(250))
                    .inner_margin(egui::Margin::same(12)),
            )
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
                            },
                        )
                    })
                    .inner;
                if changed {
                    self.notebook.dirty = true;
                }
                ui.add_space(8.0);
                if ui.button("＋ Add cell").clicked() {
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
        if self.error_dialog.is_none() {
            self.draw_unsaved_modal(ctx);
            self.draw_about_modal(ctx);
        }
        self.update_title(ctx);

        if self.starting || self.any_running() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.show_inspector && backend_ready {
            ctx.request_repaint_after(Duration::from_millis(500));
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

fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.panel_fill = egui::Color32::from_gray(250);
    style.visuals.window_fill = egui::Color32::from_gray(252);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, egui::FontId::monospace(13.5));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
    ctx.set_style(style);
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
