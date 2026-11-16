//! `StdioDriverBackend`: spawn, supervise, interrupt and restart `sage -python`.
//!
//! # Why a Python driver instead of the interactive Sage REPL
//!
//! The Sage REPL is IPython-based.  Driven over a pipe it emits a banner, an
//! `In [n]:` prompt echo, ANSI colour and `...` continuation prompts; stdout is
//! block-buffered when it is not a tty; `%display latex` is IPython-magic global
//! state the user can switch off; and pasting a multi-line block requires
//! continuation handling.  A JSON line cannot contain a raw newline, so
//! newline-delimited JSON has no delimiter collision and user `print()` output
//! can be captured into typed `stream` frames.  The driver also lets us probe
//! `latex()` and Sage's rich-output machinery directly and cap every payload
//! before it crosses the pipe.  The `===SAGE_OUTPUT_BEGIN_<nonce>===` sentinel
//! remains only as a documented debug fallback for a hypothetical raw-REPL
//! path, where payloads would be base64-encoded.  It is not used here.
//!
//! # Known limitation (prototype)
//!
//! SIGINT on this stdio prototype interrupts the entire `sage -python` process
//! group, not an individual cell.  Sage evaluation is single-threaded and the
//! driver's `run()` is the only thing on the stack, so an interrupt can only
//! abort whatever is currently executing; cells queued behind it are dropped by
//! this supervisor and marked `Interrupted`.  Per-cell cancellation arrives
//! with the Jupyter kernel backend (`interrupt_request`), which is why
//! [`crate::backend::EvalBackend::interrupt`] already takes an
//! [`InterruptScope`].  Both scopes currently mean "SIGINT the group".
//!
//! # Process control
//!
//! `sage` is a shell script wrapping Python, so the spawned pid is the group
//! leader, not necessarily the interpreter.  We spawn with
//! `Command::process_group(0)` and signal `-pgid`.  `Child::kill()` is SIGKILL
//! and would silently destroy the session's globals; it is used only in the
//! watchdog and drop fallback.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as ProcessCommand, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{after, bounded, select, unbounded, Receiver, RecvTimeoutError, Sender};

use super::protocol::{DisplayMode, DriverRequest, Frame};
use super::{
    BackendEvent, CellId, EvalBackend, InterruptScope, IntrospectId, MimeType, RunId,
    DECLARED_MIMETYPES,
};

/// A result line bigger than this is discarded before serde allocates a graph
/// for it.  The driver caps individual payloads at ~1 MB; a bundle can hold
/// several, so 16 MB is a generous local safety net.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Opaque repaint/notification hook installed by the GUI.  The supervisor
/// calls it after pushing an event; it must not block.
pub type WakeCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug)]
pub enum DriverLaunch {
    /// Release path: `sage -python -u <driver.py>`.
    Sage { sage_bin: PathBuf },
    /// Compatibility fallback for Sage CLIs that dropped `-python`:
    /// `sage -c 'os.execv(sys.executable, [...])'`.  Using Sage's own command
    /// preserves the environment the wrapper sets up (important inside the
    /// macOS .app framework) without vendoring or guessing an interpreter.
    SageExec { sage_bin: PathBuf },
    /// Test/fixture path: `python3 -u <driver.py>`.  Never selected by discovery.
    Python { python: PathBuf },
}

impl DriverLaunch {
    fn program(&self) -> &PathBuf {
        match self {
            DriverLaunch::Sage { sage_bin } | DriverLaunch::SageExec { sage_bin } => sage_bin,
            DriverLaunch::Python { python } => python,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DriverConfig {
    pub launch: DriverLaunch,
    /// Absolute path shown in the status bar / Ready event.
    pub sage_path: PathBuf,
    /// The driver file written at startup, unless overridden.
    pub driver_override: Option<PathBuf>,
    pub display: DisplayMode,
    /// Signed macOS .app cold starts take 10–20 s.
    pub ready_timeout: Duration,
    /// Time from SIGINT to watchdog SIGKILL.
    pub interrupt_timeout: Duration,
    /// Grace between SIGTERM and SIGKILL during restart/drop.
    pub shutdown_grace: Duration,
    /// A login-shell PATH to splice into the child's environment (macOS Finder).
    pub child_path: Option<OsString>,
    pub extra_env: Vec<(OsString, OsString)>,
}

impl DriverConfig {
    pub fn new(sage_bin: impl Into<PathBuf>) -> Self {
        let sage_bin = sage_bin.into();
        Self {
            launch: DriverLaunch::Sage {
                sage_bin: sage_bin.clone(),
            },
            sage_path: sage_bin,
            driver_override: None,
            display: DisplayMode::Block,
            ready_timeout: Duration::from_secs(90),
            interrupt_timeout: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(2),
            child_path: None,
            extra_env: Vec::new(),
        }
    }

    /// Test fixture: run `python -u fake_driver.py`.  Kept public so the
    /// integration tests exercise the real supervisor, not a mock of it.
    pub fn fake(python: impl Into<PathBuf>, driver: impl Into<PathBuf>) -> Self {
        let python = python.into();
        Self {
            launch: DriverLaunch::Python {
                python: python.clone(),
            },
            sage_path: python,
            driver_override: Some(driver.into()),
            display: DisplayMode::Block,
            ready_timeout: Duration::from_secs(10),
            interrupt_timeout: Duration::from_secs(2),
            shutdown_grace: Duration::from_secs(1),
            child_path: None,
            extra_env: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct ReadyInfo {
    sage_version: String,
}

enum Command {
    Start {
        reply: Sender<std::result::Result<ReadyInfo, String>>,
    },
    Restart {
        reply: Sender<std::result::Result<ReadyInfo, String>>,
    },
    Eval {
        cell: CellId,
        run: RunId,
        source: String,
    },
    Interrupt {
        scope: InterruptScope,
    },
    Introspect {
        id: IntrospectId,
    },
    Completion {
        cell: CellId,
        offset: usize,
        source: String,
    },
    Docs {
        cell: CellId,
        offset: usize,
        source: String,
    },
    Shutdown {
        done: Sender<()>,
    },
}

enum ProcessEvent {
    Frame { generation: u64, frame: Frame },
    Stderr { generation: u64, text: String },
    ProtocolWarning { generation: u64, message: String },
    Eof { generation: u64 },
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    pid: i32,
    generation: u64,
    /// Reader/stderr threads are detached: they end at EOF, and generation
    /// filtering makes their late events harmless.  We never join a pipe reader
    /// that a forked user process might keep open.
    _reader: JoinHandle<()>,
    _stderr_reader: JoinHandle<()>,
}

struct DriverFile {
    path: PathBuf,
    /// True only for the extracted copy; an override belongs to the user.
    is_temp: bool,
}

/// Multiple backends in one process (tests) must not share `driver.py`.
static DRIVER_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The public handle.  It owns a command channel to one supervisor thread, so
/// `App::update()` can drain events without locking the notebook and without a
/// Tokio runtime on the winit thread.
pub struct StdioDriverBackend {
    cmd_tx: Sender<Command>,
    events_rx: Receiver<BackendEvent>,
    worker: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    pgid: Arc<AtomicI32>,
    startup_wait: Duration,
    shutdown_grace: Duration,
    started: bool,
}

impl StdioDriverBackend {
    pub fn new(config: DriverConfig) -> Result<Self> {
        Self::new_inner(config, None)
    }

    /// Like [`Self::new`], but the supervisor calls `wake()` whenever it pushes a
    /// [`BackendEvent`].  The GUI passes `egui::Context::request_repaint` here;
    /// the backend stays windowing-agnostic because the callback is opaque.
    pub fn new_with_wake(config: DriverConfig, wake: WakeCallback) -> Result<Self> {
        Self::new_inner(config, Some(wake))
    }

    fn new_inner(config: DriverConfig, wake: Option<WakeCallback>) -> Result<Self> {
        let (cmd_tx, cmd_rx) = unbounded();
        let (events_tx, events_rx) = unbounded();
        let (proc_tx, proc_rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let pgid = Arc::new(AtomicI32::new(0));
        let startup_wait = config.ready_timeout + Duration::from_secs(10);
        let shutdown_grace = config.shutdown_grace;

        let worker = thread::Builder::new()
            .name("sagenote-supervisor".into())
            .spawn({
                let cancel = Arc::clone(&cancel);
                let pgid = Arc::clone(&pgid);
                move || {
                    let launch = config.launch.clone();
                    let mut supervisor = Supervisor {
                        config,
                        launch,
                        cmd_rx,
                        proc_tx,
                        proc_rx,
                        events_tx,
                        wake,
                        cancel,
                        pgid,
                        proc: None,
                        next_generation: 1,
                        inflight: BTreeMap::new(),
                        queue: VecDeque::new(),
                        interrupt_deadline: None,
                        driver_file: None,
                        shutting_down: false,
                        ready_version: None,
                    };
                    supervisor.run();
                    supervisor.cleanup();
                }
            })
            .context("spawning the sagenote supervisor thread")?;

        Ok(Self {
            cmd_tx,
            events_rx,
            worker: Some(worker),
            cancel,
            pgid,
            startup_wait,
            shutdown_grace,
            started: false,
        })
    }

    /// Prototype/debug hook: the process-group id, if a driver is running.
    /// Used by `sadrive` tests and emergency diagnostics; not part of the trait.
    #[doc(hidden)]
    pub fn process_group(&self) -> Option<i32> {
        let pid = self.pgid.load(Ordering::SeqCst);
        (pid > 0).then_some(pid)
    }

    /// Shared pgid slot for the GUI's close-during-startup fallback.  It is 0
    /// until the child is spawned and returns to 0 after reaping.
    #[doc(hidden)]
    pub fn process_group_handle(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.pgid)
    }

    /// Shared cancellation flag for the GUI's close-during-startup fallback.
    #[doc(hidden)]
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    fn request_start(&mut self, restart: bool) -> Result<()> {
        let (tx, rx) = bounded(1);
        let command = if restart {
            Command::Restart { reply: tx }
        } else {
            Command::Start { reply: tx }
        };
        self.cmd_tx
            .send(command)
            .map_err(|_| anyhow!("supervisor thread is not running"))?;
        match rx.recv_timeout(self.startup_wait) {
            Ok(Ok(_)) => {
                self.started = true;
                Ok(())
            }
            Ok(Err(message)) => bail!("{message}"),
            Err(_) => bail!("timed out waiting for the supervisor to start Sage"),
        }
    }
}

impl EvalBackend for StdioDriverBackend {
    fn start(&mut self) -> Result<()> {
        self.request_start(false)
    }

    fn restart(&mut self) -> Result<()> {
        self.request_start(true)
    }

    fn eval(&mut self, cell: CellId, run: RunId, source: &str) -> Result<()> {
        if !self.started {
            bail!("backend not started");
        }
        self.cmd_tx
            .send(Command::Eval {
                cell,
                run,
                source: source.to_owned(),
            })
            .map_err(|_| anyhow!("supervisor thread is not running"))
    }

    fn interrupt(&mut self, scope: InterruptScope) -> Result<()> {
        self.cmd_tx
            .send(Command::Interrupt { scope })
            .map_err(|_| anyhow!("supervisor thread is not running"))
    }

    fn introspect(&mut self, req: IntrospectId) -> Result<()> {
        self.cmd_tx
            .send(Command::Introspect { id: req })
            .map_err(|_| anyhow!("supervisor thread is not running"))
    }

    fn completion(&mut self, cell: CellId, source: &str, offset: usize) -> Result<()> {
        self.cmd_tx
            .send(Command::Completion {
                cell,
                offset,
                source: source.to_owned(),
            })
            .map_err(|_| anyhow!("supervisor thread is not running"))
    }

    fn docs(&mut self, cell: CellId, source: &str, offset: usize) -> Result<()> {
        self.cmd_tx
            .send(Command::Docs {
                cell,
                offset,
                source: source.to_owned(),
            })
            .map_err(|_| anyhow!("supervisor thread is not running"))
    }

    fn declared_mimetypes(&self) -> &[MimeType] {
        DECLARED_MIMETYPES
    }

    fn events(&self) -> Receiver<BackendEvent> {
        self.events_rx.clone()
    }
}

impl Drop for StdioDriverBackend {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        let (done_tx, done_rx) = bounded(1);
        let _ = self.cmd_tx.send(Command::Shutdown { done: done_tx });
        let wait = self.shutdown_grace + Duration::from_secs(3);
        let finished = done_rx.recv_timeout(wait).is_ok();
        if !finished {
            // The supervisor is wedged in a pipe write or a 90 s handshake.
            // Do not hang the app close path: SIGKILL the group and detach.
            emergency_kill(&self.pgid);
            self.worker.take();
        } else if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

struct Supervisor {
    config: DriverConfig,
    /// Starts as `config.launch`; updated once if a Sage CLI without
    /// `-python` forces the `sage -c` fallback.
    launch: DriverLaunch,
    wake: Option<WakeCallback>,
    cmd_rx: Receiver<Command>,
    proc_tx: Sender<ProcessEvent>,
    proc_rx: Receiver<ProcessEvent>,
    events_tx: Sender<BackendEvent>,
    cancel: Arc<AtomicBool>,
    pgid: Arc<AtomicI32>,
    proc: Option<Process>,
    next_generation: u64,
    /// At most one cell is on the driver's stack; the rest wait here.  This is
    /// what makes "drop queued cells on interrupt" possible without the driver
    /// having to understand cancellation.
    inflight: BTreeMap<(CellId, RunId), Instant>,
    queue: VecDeque<(CellId, RunId, String)>,
    interrupt_deadline: Option<Instant>,
    driver_file: Option<DriverFile>,
    shutting_down: bool,
    ready_version: Option<String>,
}

impl Supervisor {
    fn run(&mut self) {
        while !self.shutting_down {
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            let timeout = self
                .interrupt_deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(3600))
                .max(Duration::from_millis(10));

            let cmd_rx = self.cmd_rx.clone();
            let proc_rx = self.proc_rx.clone();
            select! {
                recv(cmd_rx) -> message => match message {
                    Ok(command) => self.handle_command(command),
                    Err(_) => break,
                },
                recv(proc_rx) -> message => {
                    if let Ok(event) = message {
                        self.handle_process_event(event);
                    }
                },
                recv(after(timeout)) -> _ => self.check_watchdog(),
            }
        }
    }

    fn cleanup(&mut self) {
        self.terminate_process(self.config.shutdown_grace, libc::SIGTERM);
        if let Some(driver) = self.driver_file.take() {
            if driver.is_temp {
                let _ = fs::remove_file(&driver.path);
                if let Some(dir) = driver.path.parent() {
                    // Succeeds only when the last backend in this pid is done.
                    let _ = fs::remove_dir(dir);
                }
            }
        }
    }

    fn emit(&self, event: BackendEvent) {
        let _ = self.events_tx.send(event);
        if let Some(wake) = &self.wake {
            wake();
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Start { reply } => {
                if let Some(version) = &self.ready_version {
                    if self.proc.is_some() {
                        let _ = reply.send(Ok(ReadyInfo {
                            sage_version: version.clone(),
                        }));
                        return;
                    }
                }
                let result = self.start_process();
                let _ = reply.send(result);
            }
            Command::Restart { reply } => {
                let result = self.restart_process();
                let _ = reply.send(result);
            }
            Command::Eval { cell, run, source } => self.accept_eval(cell, run, source),
            Command::Interrupt { scope } => self.interrupt(scope),
            Command::Introspect { id } => {
                if self
                    .write_request(&DriverRequest::Introspect { id })
                    .is_err()
                {
                    self.emit(BackendEvent::Fatal {
                        message: "cannot introspect: driver is not running".into(),
                    });
                }
            }
            Command::Completion {
                cell,
                offset,
                source,
            } => {
                if let Err(error) = self.write_request(&DriverRequest::Complete {
                    id: cell,
                    offset,
                    source: &source,
                }) {
                    // Completion is advisory: keep the session and answer with
                    // an empty list so the UI does not wait forever.
                    self.emit(BackendEvent::ProtocolWarning {
                        message: format!("completion request failed: {error:#}"),
                    });
                    self.emit(BackendEvent::Completion {
                        cell,
                        offset,
                        items: Vec::new(),
                    });
                }
            }
            Command::Docs {
                cell,
                offset,
                source,
            } => {
                if let Err(error) = self.write_request(&DriverRequest::Doc {
                    id: cell,
                    offset,
                    source: &source,
                }) {
                    self.emit(BackendEvent::ProtocolWarning {
                        message: format!("documentation request failed: {error:#}"),
                    });
                    self.emit(BackendEvent::Frame(Frame::Doc {
                        id: cell,
                        offset,
                        name: String::new(),
                        type_name: String::new(),
                        signature: String::new(),
                        text: format!("Could not request documentation: {error:#}"),
                    }));
                }
            }
            Command::Shutdown { done } => {
                self.shutting_down = true;
                self.terminate_process(self.config.shutdown_grace, libc::SIGTERM);
                let _ = done.send(());
            }
        }
    }

    fn accept_eval(&mut self, cell: CellId, run: RunId, source: String) {
        if self.proc.is_none() {
            self.emit(BackendEvent::Fatal {
                message: "cannot evaluate: driver is not running".into(),
            });
            return;
        }
        if self.inflight.is_empty() {
            self.inflight.insert((cell, run), Instant::now());
            if let Err(err) = self.write_request(&DriverRequest::Eval {
                id: cell,
                run,
                source: &source,
            }) {
                self.inflight.remove(&(cell, run));
                self.emit(BackendEvent::Fatal {
                    message: format!("cannot write to driver: {err:#}"),
                });
            }
        } else {
            self.queue.push_back((cell, run, source));
            self.emit(BackendEvent::Queued { cell, run });
        }
    }

    fn interrupt(&mut self, _scope: InterruptScope) {
        // Both Cell and Kernel mean SIGINT the process group in this backend.
        // See the module-level known-limitation note.
        if let Some(process) = &self.proc {
            kill_group(process.pid, libc::SIGINT);
        }

        // Cells queued behind the active one are never put on the driver's
        // stack (we serialize evals), so they can be dropped here cleanly.
        let dropped: Vec<_> = self.queue.drain(..).collect();
        for (cell, run, _) in dropped {
            self.emit(BackendEvent::Interrupted { cell, run });
        }

        if !self.inflight.is_empty() {
            self.interrupt_deadline = Some(Instant::now() + self.config.interrupt_timeout);
        }
    }

    fn check_watchdog(&mut self) {
        let Some(deadline) = self.interrupt_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.interrupt_deadline = None;

        let mut cells: Vec<(CellId, RunId)> = self.inflight.keys().copied().collect();
        cells.extend(self.queue.iter().map(|(cell, run, _)| (*cell, *run)));

        self.emit(BackendEvent::WatchdogFired {
            cells: cells.clone(),
            timeout: self.config.interrupt_timeout,
        });

        // Required by the spec: no `end` in time => SIGKILL the group, restart,
        // and mark everything in flight Interrupted.
        self.terminate_process(self.config.shutdown_grace, libc::SIGKILL);
        self.mark_all_interrupted();
        match self.start_process() {
            Ok(info) => {
                self.emit(BackendEvent::Restarted {
                    sage_version: info.sage_version,
                });
            }
            Err(message) => self.emit(BackendEvent::Fatal { message }),
        }
    }

    fn mark_all_interrupted(&mut self) {
        let inflight: Vec<_> = self.inflight.keys().copied().collect();
        self.inflight.clear();
        for (cell, run) in inflight {
            self.emit(BackendEvent::Interrupted { cell, run });
        }
        let queued: Vec<_> = self.queue.drain(..).collect();
        for (cell, run, _) in queued {
            self.emit(BackendEvent::Interrupted { cell, run });
        }
        self.interrupt_deadline = None;
    }

    fn restart_process(&mut self) -> std::result::Result<ReadyInfo, String> {
        self.terminate_process(self.config.shutdown_grace, libc::SIGTERM);
        self.mark_all_interrupted();
        let info = self.start_process()?;
        self.emit(BackendEvent::Restarted {
            sage_version: info.sage_version.clone(),
        });
        Ok(info)
    }

    fn handle_process_event(&mut self, event: ProcessEvent) {
        match event {
            ProcessEvent::Frame { generation, frame } => {
                if !self.is_current(generation) {
                    return;
                }
                match &frame {
                    Frame::Start { id, run } => {
                        self.inflight
                            .entry((*id, *run))
                            .or_insert_with(Instant::now);
                        self.emit(BackendEvent::Frame(frame));
                    }
                    Frame::End { id, run } => {
                        self.inflight.remove(&(*id, *run));
                        self.interrupt_deadline = None;
                        self.emit(BackendEvent::Frame(frame));
                        self.dispatch_next();
                    }
                    Frame::Error { .. } => {
                        // Keep the in-flight entry until `end`; the driver
                        // guarantees an end even for KeyboardInterrupt.
                        self.emit(BackendEvent::Frame(frame));
                    }
                    Frame::Complete { id, offset, items } => {
                        // Completion is not cell output: expose it as the
                        // dedicated event so the UI can route it by cell id.
                        self.emit(BackendEvent::Completion {
                            cell: *id,
                            offset: *offset,
                            items: items.clone(),
                        });
                    }
                    _ => self.emit(BackendEvent::Frame(frame)),
                }
            }
            ProcessEvent::Stderr { generation, text } => {
                if self.is_current(generation) {
                    self.emit(BackendEvent::Stderr { text });
                }
            }
            ProcessEvent::ProtocolWarning {
                generation,
                message,
            } => {
                if self.is_current(generation) {
                    self.emit(BackendEvent::ProtocolWarning { message });
                }
            }
            ProcessEvent::Eof { generation } => {
                if self.is_current(generation) {
                    self.handle_unexpected_eof();
                }
            }
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.proc
            .as_ref()
            .map(|process| process.generation == generation)
            .unwrap_or(false)
    }

    fn dispatch_next(&mut self) {
        if self.proc.is_none() || !self.inflight.is_empty() {
            return;
        }
        let Some((cell, run, source)) = self.queue.pop_front() else {
            return;
        };
        self.inflight.insert((cell, run), Instant::now());
        if let Err(err) = self.write_request(&DriverRequest::Eval {
            id: cell,
            run,
            source: &source,
        }) {
            self.inflight.clear();
            self.emit(BackendEvent::Fatal {
                message: format!("cannot write to driver: {err:#}"),
            });
        }
    }

    fn write_request(&mut self, request: &DriverRequest<'_>) -> Result<()> {
        let process = self
            .proc
            .as_mut()
            .context("driver process is not running")?;
        let line = request.to_json_line().context("encoding driver request")?;
        process
            .stdin
            .write_all(line.as_bytes())
            .context("writing driver request")?;
        process.stdin.write_all(b"\n").context("writing newline")?;
        process.stdin.flush().context("flushing driver stdin")?;
        Ok(())
    }

    fn start_process(&mut self) -> std::result::Result<ReadyInfo, String> {
        if self.proc.is_some() {
            if let Some(version) = &self.ready_version {
                return Ok(ReadyInfo {
                    sage_version: version.clone(),
                });
            }
            return Err("driver is already starting".into());
        }

        let driver_path = self
            .ensure_driver_file()
            .map_err(|err| format!("{err:#}"))?;
        let primary = self.launch.clone();
        // One ready deadline for the whole start/restart, including the
        // compatibility attempt.  A hung primary must not get 90 s *and* a
        // fallback 90 s while the UI promises a 90 s handshake.
        let deadline = Instant::now() + self.config.ready_timeout;

        // Documented compatibility fallback: some Sage builds (e.g. the new
        // `sage.cli` in 10.10) no longer accept `sage -python`.  The primary
        // path is still exactly the spec's `sage -python -u driver.py`; this
        // only runs if that launch exits before `ready`.
        match self.spawn_and_handshake(&driver_path, primary.clone(), deadline) {
            Ok(info) => Ok(info),
            Err(primary_error) => {
                let DriverLaunch::Sage { sage_bin } = primary else {
                    return Err(primary_error);
                };
                if Instant::now() >= deadline {
                    return Err(primary_error);
                }
                // Use Sage's own Python through its -c command and exec the
                // driver.  That preserves the environment the Sage wrapper set
                // up, which is what makes this work inside a macOS .app.
                let fallback = DriverLaunch::SageExec { sage_bin };
                match self.spawn_and_handshake(&driver_path, fallback.clone(), deadline) {
                    Ok(info) => {
                        self.launch = fallback;
                        Ok(info)
                    }
                    Err(fallback_error) => Err(format!(
                        "{primary_error}\n(this Sage CLI rejected -python; the sage -c fallback also failed: {fallback_error})"
                    )),
                }
            }
        }
    }

    fn spawn_and_handshake(
        &mut self,
        driver_path: &Path,
        launch: DriverLaunch,
        deadline: Instant,
    ) -> std::result::Result<ReadyInfo, String> {
        let generation = self.next_generation;
        self.next_generation += 1;
        let process = spawn_process(
            &self.config,
            &launch,
            driver_path,
            generation,
            self.proc_tx.clone(),
        )
        .map_err(|err| format!("cannot spawn Sage: {err:#}"))?;

        self.pgid.store(process.pid, Ordering::SeqCst);
        self.proc = Some(process);

        let mut startup_stderr = String::new();
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                self.terminate_process(Duration::from_millis(200), libc::SIGTERM);
                return Err(with_stderr("startup cancelled", &startup_stderr));
            }
            let now = Instant::now();
            if now >= deadline {
                self.terminate_process(Duration::from_millis(200), libc::SIGKILL);
                return Err(with_stderr(
                    format!(
                        "timed out after {} s waiting for the driver's ready frame",
                        self.config.ready_timeout.as_secs()
                    ),
                    &startup_stderr,
                ));
            }
            let wait = (deadline - now).min(Duration::from_millis(100));
            match self.proc_rx.recv_timeout(wait) {
                Ok(ProcessEvent::Frame {
                    generation: g,
                    frame,
                }) if g == generation => {
                    match frame {
                        Frame::Ready { sage_version } => {
                            if let Err(err) = self.write_request(&DriverRequest::SetDisplay {
                                mode: self.config.display,
                            }) {
                                self.terminate_process(Duration::from_millis(200), libc::SIGKILL);
                                return Err(with_stderr(
                                    format!("driver became ready but the preamble failed: {err:#}"),
                                    &startup_stderr,
                                ));
                            }
                            self.ready_version = Some(sage_version.clone());
                            self.emit(BackendEvent::Ready {
                                sage_path: self.config.sage_path.clone(),
                                sage_version: sage_version.clone(),
                            });
                            return Ok(ReadyInfo { sage_version });
                        }
                        other => {
                            // init.sage output or an auxiliary frame before ready.
                            self.emit(BackendEvent::Frame(other));
                        }
                    }
                }
                Ok(ProcessEvent::Stderr {
                    generation: g,
                    text,
                }) if g == generation => {
                    // Do not surface startup stderr immediately: the primary
                    // `sage -python` attempt may be expected to fail before the
                    // direct-interpreter fallback.  It is included in the error
                    // if no launch succeeds.
                    append_capped(&mut startup_stderr, &text, 4 * 1024);
                }
                Ok(ProcessEvent::ProtocolWarning { generation: g, .. }) if g == generation => {
                    // Pre-ready non-JSON output is usually a wrapper banner
                    // (e.g. from `sage -c`).  Do not surface it before ready;
                    // startup stderr is still included if startup fails.
                }
                Ok(ProcessEvent::Eof { generation: g }) if g == generation => {
                    self.handle_startup_eof();
                    return Err(with_stderr(
                        "driver exited before its ready frame",
                        &startup_stderr,
                    ));
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.terminate_process(Duration::from_millis(200), libc::SIGKILL);
                    return Err(with_stderr(
                        "supervisor lost the driver pipe",
                        &startup_stderr,
                    ));
                }
            }
        }
    }

    fn handle_startup_eof(&mut self) {
        let Some(mut process) = self.proc.take() else {
            return;
        };
        let status = wait_with_timeout(&mut process.child, Duration::from_secs(1)).or_else(|| {
            kill_group(process.pid, libc::SIGKILL);
            wait_with_timeout(&mut process.child, Duration::from_secs(1))
        });
        self.pgid.store(0, Ordering::SeqCst);
        drop(process.stdin);
        self.ready_version = None;
        let _ = status;
    }

    fn handle_unexpected_eof(&mut self) {
        let Some(mut process) = self.proc.take() else {
            return;
        };
        let status = wait_with_timeout(&mut process.child, Duration::from_secs(2)).or_else(|| {
            kill_group(process.pid, libc::SIGKILL);
            wait_with_timeout(&mut process.child, Duration::from_secs(2))
        });
        self.pgid.store(0, Ordering::SeqCst);
        drop(process.stdin);
        self.ready_version = None;
        self.interrupt_deadline = None;

        let message = describe_exit(status);
        self.mark_all_interrupted();
        self.emit(BackendEvent::ProcessExited {
            code: status.and_then(|s| s.code()),
            message,
        });
    }

    fn ensure_driver_file(&mut self) -> Result<PathBuf> {
        if let Some(driver) = &self.driver_file {
            return Ok(driver.path.clone());
        }
        let driver = prepare_driver_file(&self.config)?;
        let path = driver.path.clone();
        self.driver_file = Some(driver);
        Ok(path)
    }

    /// Kill the whole group, wait with a timeout, then SIGKILL.  Used for
    /// restart, watchdog and Drop; never for a normal interrupt.
    fn terminate_process(&mut self, grace: Duration, initial_signal: i32) {
        let Some(mut process) = self.proc.take() else {
            self.pgid.store(0, Ordering::SeqCst);
            return;
        };
        self.ready_version = None;
        kill_group(process.pid, initial_signal);
        let mut status = wait_with_timeout(&mut process.child, grace);
        if status.is_none() {
            kill_group(process.pid, libc::SIGKILL);
            status = wait_with_timeout(&mut process.child, Duration::from_secs(2));
        }
        if status.is_none() {
            // Last resort; `wait` after SIGKILL is expected to return promptly.
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
        self.pgid.store(0, Ordering::SeqCst);
        // Drain this generation to EOF before a new process can write.  The
        // reader threads send Eof after the pipe closes; waiting here also
        // prevents a late frame from the old child being mistaken for the new
        // session's output.  We do not join the threads themselves: a forked
        // user process could hold the pipe open, and generation filtering
        // already makes late events harmless.
        let generation = process.generation;
        let eof_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < eof_deadline {
            match self.proc_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(ProcessEvent::Eof { generation: g }) if g == generation => break,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // Close stdin only after the process is gone.
        drop(process.stdin);
    }
}

/// Python source for the SageExec fallback.  It is passed as one `-c` argument;
/// `os.execv` keeps Sage's environment and replaces the wrapper Python with the
/// driver, so stdin/stdout behave exactly as in the primary path.
fn sage_exec_code(driver_path: &Path) -> String {
    let path = driver_path.to_string_lossy();
    let escaped = path
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("import os,sys;os.execv(sys.executable,[sys.executable,'-u','{escaped}'])")
}

fn spawn_process(
    config: &DriverConfig,
    launch: &DriverLaunch,
    driver_path: &Path,
    generation: u64,
    proc_tx: Sender<ProcessEvent>,
) -> Result<Process> {
    let mut command = match launch {
        DriverLaunch::Sage { sage_bin } => {
            let mut command = ProcessCommand::new(sage_bin);
            command.arg("-python").arg("-u").arg(driver_path);
            command
        }
        DriverLaunch::SageExec { sage_bin } => {
            let mut command = ProcessCommand::new(sage_bin);
            command.arg("-c").arg(sage_exec_code(driver_path));
            command
        }
        DriverLaunch::Python { python } => {
            let mut command = ProcessCommand::new(python);
            command.arg("-u").arg(driver_path);
            command
        }
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONUNBUFFERED", "1");
    // sage is a shell script wrapping Python: make the spawned pid the group
    // leader, not merely the interpreter's parent.
    command.process_group(0);
    if let Some(path) = &config.child_path {
        command.env("PATH", path);
    }
    for (key, value) in &config.extra_env {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", launch.program().display()))?;
    let pid = child.id() as i32;
    let Some(stdin) = child.stdin.take() else {
        abort_spawned_child(&mut child, pid);
        bail!("child stdin was not piped");
    };
    let Some(stdout) = child.stdout.take() else {
        drop(stdin);
        abort_spawned_child(&mut child, pid);
        bail!("child stdout was not piped");
    };
    let Some(stderr) = child.stderr.take() else {
        drop(stdin);
        drop(stdout);
        abort_spawned_child(&mut child, pid);
        bail!("child stderr was not piped");
    };

    let reader_tx = proc_tx.clone();
    let reader = match thread::Builder::new()
        .name(format!("sagenote-stdout-{generation}"))
        .spawn(move || read_frames(stdout, generation, reader_tx))
    {
        Ok(handle) => handle,
        Err(error) => {
            drop(stdin);
            abort_spawned_child(&mut child, pid);
            bail!("spawning stdout reader: {error}");
        }
    };

    let stderr_tx = proc_tx;
    let stderr_reader = match thread::Builder::new()
        .name(format!("sagenote-stderr-{generation}"))
        .spawn(move || read_stderr(stderr, generation, stderr_tx))
    {
        Ok(handle) => handle,
        Err(error) => {
            drop(stdin);
            abort_spawned_child(&mut child, pid);
            drop(reader);
            bail!("spawning stderr reader: {error}");
        }
    };

    Ok(Process {
        child,
        stdin,
        pid,
        generation,
        _reader: reader,
        _stderr_reader: stderr_reader,
    })
}

fn read_frames(stdout: std::process::ChildStdout, generation: u64, tx: Sender<ProcessEvent>) {
    let mut reader = BufReader::new(stdout);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) => break,
            Ok(_) => {
                if bytes.len() > MAX_FRAME_BYTES {
                    let _ = tx.send(ProcessEvent::ProtocolWarning {
                        generation,
                        message: format!(
                            "discarded an over-long frame ({} bytes, cap {})",
                            bytes.len(),
                            MAX_FRAME_BYTES
                        ),
                    });
                    continue;
                }
                let line = String::from_utf8_lossy(&bytes);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.trim().is_empty() {
                    continue;
                }
                match Frame::from_json_line(line) {
                    Ok(frame) => {
                        if tx.send(ProcessEvent::Frame { generation, frame }).is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        let preview: String = line.chars().take(200).collect();
                        let _ = tx.send(ProcessEvent::ProtocolWarning {
                            generation,
                            message: format!("non-JSON driver line ({err}): {preview}"),
                        });
                    }
                }
            }
            Err(err) => {
                let _ = tx.send(ProcessEvent::ProtocolWarning {
                    generation,
                    message: format!("stdout read error: {err}"),
                });
                break;
            }
        }
    }
    let _ = tx.send(ProcessEvent::Eof { generation });
}

fn read_stderr(stderr: std::process::ChildStderr, generation: u64, tx: Sender<ProcessEvent>) {
    let mut reader = BufReader::new(stderr);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let text = truncate_chars(&text, 16 * 1024);
                let _ = tx.send(ProcessEvent::Stderr { generation, text });
            }
            Err(_) => break,
        }
    }
}

fn append_capped(buffer: &mut String, text: &str, max: usize) {
    if buffer.len() >= max {
        return;
    }
    let mut end = text.len().min(max - buffer.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    buffer.push_str(&text[..end]);
    if end < text.len() {
        buffer.push_str("\n[... startup stderr truncated ...]");
    }
}

fn with_stderr(message: impl Into<String>, stderr: &str) -> String {
    let message = message.into();
    if stderr.trim().is_empty() {
        message
    } else {
        format!("{message}\n--- driver stderr ---\n{}", stderr.trim_end())
    }
}

fn prepare_driver_file(config: &DriverConfig) -> Result<DriverFile> {
    // An explicit path (CLI --driver, test fixture) wins; SAGE_DRIVER_OVERRIDE
    // is the release/iteration hook when none was supplied.
    let override_path = config
        .driver_override
        .clone()
        .or_else(|| std::env::var_os("SAGE_DRIVER_OVERRIDE").map(PathBuf::from));
    if let Some(path) = override_path {
        return Ok(DriverFile {
            path,
            is_temp: false,
        });
    }

    // Deliberately std::env::temp_dir(), not $XDG_RUNTIME_DIR: the latter does
    // not exist on macOS.
    let dir = crate::platform::temp_driver_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating driver temp dir {}", dir.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = DRIVER_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("driver-{nonce}-{counter}.py"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("writing driver to {}", path.display()))?;
    file.write_all(include_str!("../../sage/driver.py").as_bytes())
        .with_context(|| format!("writing driver to {}", path.display()))?;
    file.sync_all().ok();
    drop(file);

    Ok(DriverFile {
        path,
        is_temp: true,
    })
}

fn abort_spawned_child(child: &mut Child, pid: i32) {
    kill_group(pid, libc::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_group(pid: i32, signal: i32) {
    if pid <= 0 {
        return;
    }
    // Negative pid = process group.  child.kill() would be SIGKILL and would
    // destroy the session; this is the only interrupt mechanism.
    // SAFETY: libc::kill only reads its arguments, which are valid: the signal
    // is a libc constant supplied by the caller, and the pid was stored by the
    // supervisor after a successful spawn, so the negative id names a group we
    // created.  Nothing is borrowed across the call, so no aliasing is
    // possible.
    unsafe {
        libc::kill(-pid, signal);
    }
}

fn emergency_kill(pgid: &AtomicI32) {
    let pid = pgid.load(Ordering::SeqCst);
    if pid > 0 {
        kill_group(pid, libc::SIGKILL);
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn describe_exit(status: Option<ExitStatus>) -> String {
    match status {
        Some(status) => {
            if let Some(code) = status.code() {
                format!("driver exited unexpectedly with status {code}")
            } else if let Some(signal) = status.signal() {
                format!("driver was terminated by signal {signal}")
            } else {
                "driver exited unexpectedly".into()
            }
        }
        None => "driver process ended and could not be reaped in time".into(),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n[... sagenote: stderr line truncated ...]");
    out
}
