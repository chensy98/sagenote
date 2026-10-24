//! Backend abstraction and the stdio Sage driver.
//!
//! **This module tree must not import `eframe`, `egui` or any windowing
//! crate.** It is the part CI can run headless on both platforms, and the part
//! a future `JupyterKernelBackend` can replace without UI changes.

pub mod driver;
pub mod protocol;

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel as mpsc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use driver::{DriverConfig, DriverLaunch, StdioDriverBackend, WakeCallback};
pub use protocol::{DisplayMode, Frame, MimeBundle, StreamName, VarInfo};

/// Stable identity of a cell. UUIDs survive save/load and cell reordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CellId(pub Uuid);

impl CellId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for CellId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identity of one evaluation of a cell. Results are routed by (cell, run) and
/// discarded when the cell is gone or the run has been superseded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub u64);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntrospectId(pub u64);

impl fmt::Display for IntrospectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The mimetypes this backend and the UI currently understand.
///
/// A new output kind is added here and in the bundle, never inferred from a
/// string in `text/plain`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MimeType {
    TextLatex,
    ImageSvg,
    ImagePng,
    TextPlain,
    TextHtml,
}

impl MimeType {
    pub const ALL: [MimeType; 5] = [
        MimeType::TextLatex,
        MimeType::ImageSvg,
        MimeType::ImagePng,
        MimeType::TextPlain,
        MimeType::TextHtml,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            MimeType::TextLatex => "text/latex",
            MimeType::ImageSvg => "image/svg+xml",
            MimeType::ImagePng => "image/png",
            MimeType::TextPlain => "text/plain",
            MimeType::TextHtml => "text/html",
        }
    }

    pub fn from_mime(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.as_str() == s)
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which stream a chunk of text came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InterruptScope {
    /// Abort the currently executing cell.
    Cell,
    /// Interrupt the whole kernel (the only thing this stdio prototype can do).
    Kernel,
}

/// Typed events leaving a backend. Backend threads never touch the notebook;
/// the UI/runtime drains this channel.
#[derive(Clone, Debug)]
pub enum BackendEvent {
    /// The process handshake completed. `sage_path` is the absolute binary we
    /// actually launched, for the status bar.
    Ready {
        sage_path: PathBuf,
        sage_version: String,
    },
    /// A protocol frame, passed through typed.
    Frame(Frame),
    /// An eval was accepted but is waiting behind the active one. This backend
    /// puts at most one cell on the driver's stack.
    Queued { cell: CellId, run: RunId },
    /// The client gave up on this run. Emitted for queued work dropped on an
    /// interrupt, and for the active cell when the watchdog had to reset the
    /// process.
    Interrupted { cell: CellId, run: RunId },
    /// No `end` arrived within the interrupt grace period; the group was
    /// SIGKILLed and the process restarted.
    WatchdogFired {
        cells: Vec<(CellId, RunId)>,
        timeout: Duration,
    },
    /// A fresh process finished the ready handshake and the preamble.
    Restarted { sage_version: String },
    /// Placeholder completion channel. M1 emits an empty vector.
    Completion {
        cell: CellId,
        offset: usize,
        items: Vec<String>,
    },
    /// Text on the *process's* stderr (not a user `print(..., file=sys.stderr)`,
    /// which the driver captures into a `stream` frame).
    Stderr { text: String },
    /// A frame line that did not parse. The session is kept alive; this is a
    /// diagnostic, not a fatal error.
    ProtocolWarning { message: String },
    /// The driver process is gone unexpectedly.
    ProcessExited { code: Option<i32>, message: String },
    /// The supervisor could not start/restart the process.
    Fatal { message: String },
}

/// The one backend interface the UI is allowed to know.
///
/// `JupyterKernelBackend` can be dropped in later: the UI only sees typed
/// events and mimetype bundles, never text parsed out of `repr`.
pub trait EvalBackend {
    fn start(&mut self) -> Result<()>;
    fn restart(&mut self) -> Result<()>;
    fn eval(&mut self, cell: CellId, run: RunId, source: &str) -> Result<()>;
    fn interrupt(&mut self, scope: InterruptScope) -> Result<()>;
    fn introspect(&mut self, req: IntrospectId) -> Result<()>;
    fn completion(&mut self, cell: CellId, offset: usize) -> Result<()>;
    fn declared_mimetypes(&self) -> &[MimeType];
    fn events(&self) -> mpsc::Receiver<BackendEvent>;
}

/// Convenience for `&[MimeType]` call sites.
pub const DECLARED_MIMETYPES: &[MimeType] = &MimeType::ALL;
