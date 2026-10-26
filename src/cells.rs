//! Notebook and cell model, `.sagenote` codec, and the ipynb hook.
//!
//! No `egui`/window types here: the notebook must be loadable and savable in a
//! headless test.  UI state (focus, scroll, textures) lives in `app`/`ui`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::backend::{CellId, DisplayMode, MimeBundle};

pub const FORMAT_VERSION: u32 = 1;
pub const EXTENSION: &str = "sagenote";

/// Per-cell evaluation state shown inline.  Persisted as a plain string, as in
/// the format example (`"state": "ok"`).  Transient states become `Idle` on load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CellState {
    #[default]
    Idle,
    Queued,
    Running,
    Ok,
    Error,
    Interrupted,
}

impl CellState {
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            CellState::Queued | CellState::Running | CellState::Interrupted
        )
    }
}

/// A serialized output.  `stale` is part of the format so the "do not pretend
/// saved globals still exist" decision can be reversed later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedOutput {
    pub mime: String,
    /// UTF-8 text for text/latex/svg/plain, base64 for image/png.
    pub data: String,
    #[serde(default)]
    pub display: DisplayMode,
    #[serde(default)]
    pub stale: bool,
}

impl SavedOutput {
    pub fn from_bundle(bundle: &MimeBundle, display: DisplayMode) -> Vec<Self> {
        bundle
            .iter()
            .map(|(mime, data)| Self {
                mime: mime.to_owned(),
                data: data.to_owned(),
                display,
                stale: false,
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub id: CellId,
    pub source: String,
    #[serde(default)]
    pub outputs: Vec<SavedOutput>,
    #[serde(default)]
    pub state: CellState,
}

impl Cell {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            id: CellId::new(),
            source: source.into(),
            outputs: Vec::new(),
            state: CellState::Idle,
        }
    }

    pub fn with_id(id: CellId, source: impl Into<String>) -> Self {
        Self {
            id,
            source: source.into(),
            outputs: Vec::new(),
            state: CellState::Idle,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub sage_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notebook {
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    #[serde(default)]
    pub meta: Meta,
    #[serde(default)]
    pub cells: Vec<Cell>,

    /// Not serialized; the open file, if any.
    #[serde(skip)]
    pub path: Option<PathBuf>,
    /// Not serialized; the UI's close-confirm bit.
    #[serde(skip)]
    pub dirty: bool,
}

fn default_format_version() -> u32 {
    FORMAT_VERSION
}

impl Notebook {
    /// A fresh notebook with one empty cell, the way a wxMaxima sheet opens.
    pub fn new(sage_version: impl Into<String>) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            meta: Meta {
                created: utc_now_rfc3339(),
                sage_version: sage_version.into(),
            },
            cells: vec![Cell::new("")],
            path: None,
            dirty: false,
        }
    }

    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }

    pub fn add_cell(&mut self, source: impl Into<String>) -> CellId {
        let cell = Cell::new(source);
        let id = cell.id;
        self.cells.push(cell);
        self.dirty = true;
        id
    }

    /// Insert a new cell immediately after `after`; `None` appends.
    pub fn insert_cell_after(
        &mut self,
        after: Option<CellId>,
        source: impl Into<String>,
    ) -> CellId {
        let cell = Cell::new(source);
        let id = cell.id;
        let index = after
            .and_then(|after| self.cells.iter().position(|c| c.id == after))
            .map(|index| index + 1)
            .unwrap_or(self.cells.len());
        self.cells.insert(index, cell);
        self.dirty = true;
        id
    }

    pub fn cell_index(&self, id: CellId) -> Option<usize> {
        self.cells.iter().position(|c| c.id == id)
    }

    pub fn remove_cell(&mut self, id: CellId) -> Option<Cell> {
        let index = self.cells.iter().position(|c| c.id == id)?;
        self.dirty = true;
        Some(self.cells.remove(index))
    }

    pub fn move_cell_up(&mut self, id: CellId) -> bool {
        let Some(index) = self.cells.iter().position(|c| c.id == id) else {
            return false;
        };
        if index == 0 {
            return false;
        }
        self.cells.swap(index, index - 1);
        self.dirty = true;
        true
    }

    pub fn move_cell_down(&mut self, id: CellId) -> bool {
        let Some(index) = self.cells.iter().position(|c| c.id == id) else {
            return false;
        };
        if index + 1 >= self.cells.len() {
            return false;
        }
        self.cells.swap(index, index + 1);
        self.dirty = true;
        true
    }

    pub fn cell(&self, id: CellId) -> Option<&Cell> {
        self.cells.iter().find(|c| c.id == id)
    }

    pub fn cell_mut(&mut self, id: CellId) -> Option<&mut Cell> {
        self.cells.iter_mut().find(|c| c.id == id)
    }

    /// Saved outputs were produced by globals that do not exist in the new
    /// session.  Render them greyed with a "stale" tag; never silently re-run.
    pub fn mark_outputs_stale(&mut self) {
        for cell in &mut self.cells {
            for output in &mut cell.outputs {
                output.stale = true;
            }
            if cell.state.is_transient() {
                cell.state = CellState::Idle;
            }
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let codec = codec_for_path(path);
        let mut notebook = codec
            .decode(&bytes)
            .with_context(|| format!("decoding {}", path.display()))?;
        notebook.path = Some(path.to_path_buf());
        notebook.dirty = false;
        Ok(notebook)
    }

    pub fn save(&mut self) -> Result<()> {
        let path = self
            .path
            .clone()
            .context("notebook has no path; use save_as")?;
        self.save_as(path)
    }

    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let codec = codec_for_path(path);
        let bytes = codec.encode(self)?;
        save_atomic(path, &bytes)?;
        self.path = Some(path.to_path_buf());
        self.dirty = false;
        Ok(())
    }
}

/// One codec per container format.  `IpynbCodec` is the hook for later; the
/// trait exists now so the UI never grows format-specific branches.
pub trait NotebookCodec {
    fn name(&self) -> &'static str;
    fn decode(&self, bytes: &[u8]) -> Result<Notebook>;
    fn encode(&self, notebook: &Notebook) -> Result<Vec<u8>>;
}

pub fn codec_for_path(path: &Path) -> Box<dyn NotebookCodec> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("ipynb") => Box::new(IpynbCodec),
        _ => Box::new(SagenoteCodec),
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SagenoteCodec;

impl NotebookCodec for SagenoteCodec {
    fn name(&self) -> &'static str {
        "sagenote"
    }

    fn decode(&self, bytes: &[u8]) -> Result<Notebook> {
        let mut notebook: Notebook =
            serde_json::from_slice(bytes).context("not a .sagenote JSON document")?;
        if notebook.format_version != FORMAT_VERSION {
            bail!(
                "unsupported .sagenote format_version {} (this build reads {})",
                notebook.format_version,
                FORMAT_VERSION
            );
        }
        if notebook.meta.created.is_empty() {
            notebook.meta.created = utc_now_rfc3339();
        }
        // Loading is not re-running: every saved output is stale by definition.
        notebook.mark_outputs_stale();
        notebook.dirty = false;
        Ok(notebook)
    }

    fn encode(&self, notebook: &Notebook) -> Result<Vec<u8>> {
        if notebook.format_version != FORMAT_VERSION {
            bail!(
                "refusing to save format_version {} (this build writes {})",
                notebook.format_version,
                FORMAT_VERSION
            );
        }
        let mut bytes = serde_json::to_vec_pretty(notebook).context("encoding notebook")?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Placeholder only.  Import/export is explicitly outside the prototype scope;
/// the trait and the extension dispatch are the seams to fill later.
#[derive(Clone, Copy, Debug, Default)]
pub struct IpynbCodec;

impl NotebookCodec for IpynbCodec {
    fn name(&self) -> &'static str {
        "ipynb"
    }

    fn decode(&self, _bytes: &[u8]) -> Result<Notebook> {
        bail!("ipynb import is not implemented in this prototype")
    }

    fn encode(&self, _notebook: &Notebook) -> Result<Vec<u8>> {
        bail!("ipynb export is not implemented in this prototype")
    }
}

static SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write a temp file in the destination directory, fsync it, then rename.
/// Both target platforms are Unix, so rename is atomic within one filesystem.
pub fn save_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "notebook".into());
    let counter = SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.tmp-{}-{}-{}",
        file_name,
        std::process::id(),
        nonce,
        counter
    ));

    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Minimal RFC3339 UTC timestamp; avoids pulling a date crate.
fn utc_now_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MimeType;

    fn sample() -> Notebook {
        let mut nb = Notebook::new("10.10");
        let id = nb.cells[0].id;
        nb.cell_mut(id).unwrap().source = "a = 6*7\na".into();
        let mut bundle = MimeBundle::new();
        bundle.insert(MimeType::TextLatex, "42");
        nb.cell_mut(id).unwrap().outputs = SavedOutput::from_bundle(&bundle, DisplayMode::Block);
        nb.cell_mut(id).unwrap().state = CellState::Ok;
        nb.add_cell("print(a)");
        nb
    }

    #[test]
    fn roundtrip_preserves_ids_cells_and_bundles() {
        let nb = sample();
        let bytes = SagenoteCodec.encode(&nb).unwrap();
        let back = SagenoteCodec.decode(&bytes).unwrap();
        assert_eq!(back.format_version, FORMAT_VERSION);
        assert_eq!(back.cells.len(), 2);
        assert_eq!(back.cells[0].id, nb.cells[0].id);
        assert_eq!(back.cells[0].source, nb.cells[0].source);
        assert_eq!(back.cells[0].outputs[0].mime, "text/latex");
        assert_eq!(back.cells[0].outputs[0].data, "42");
        assert_eq!(back.cells[0].state, CellState::Ok);
        assert!(!back.dirty);
    }

    #[test]
    fn outputs_are_stale_on_load() {
        let nb = sample();
        assert!(!nb.cells[0].outputs[0].stale);
        let bytes = SagenoteCodec.encode(&nb).unwrap();
        let back = SagenoteCodec.decode(&bytes).unwrap();
        assert!(back.cells[0].outputs[0].stale);
    }

    #[test]
    fn transient_state_is_normalised_on_load() {
        let mut nb = sample();
        nb.cells[0].state = CellState::Running;
        let back = SagenoteCodec
            .decode(&SagenoteCodec.encode(&nb).unwrap())
            .unwrap();
        assert_eq!(back.cells[0].state, CellState::Idle);
    }

    #[test]
    fn format_version_is_checked() {
        let mut nb = sample();
        nb.format_version = 99;
        let err = SagenoteCodec.encode(&nb).unwrap_err();
        assert!(err.to_string().contains("format_version"));

        let text = r#"{"format_version":99,"meta":{},"cells":[]}"#;
        let err = SagenoteCodec.decode(text.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn atomic_save_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!(
            "sagenote-cells-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notebook.sagenote");
        let mut nb = sample();
        nb.save_as(&path).unwrap();
        assert!(path.exists());
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
        assert_eq!(nb.path.as_deref(), Some(path.as_path()));
        assert!(!nb.dirty);
        let reopened = Notebook::open(&path).unwrap();
        assert_eq!(reopened.cells.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_sets_path_and_stale_outputs() {
        let dir = std::env::temp_dir().join(format!(
            "sagenote-cells-open-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.sagenote");
        sample().save_as(&path).unwrap();
        let nb = Notebook::open(&path).unwrap();
        assert_eq!(nb.path.as_deref(), Some(path.as_path()));
        assert!(nb.cells[0].outputs[0].stale);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_and_delete() {
        let mut nb = Notebook::new("10.10");
        let first = nb.cells[0].id;
        let second = nb.add_cell("x");
        assert!(nb.move_cell_down(first));
        assert_eq!(nb.cells[1].id, first);
        assert_eq!(nb.cells[0].id, second);
        assert!(!nb.move_cell_down(first));
        assert!(nb.remove_cell(second).is_some());
        assert_eq!(nb.cells.len(), 1);
        assert!(!nb.move_cell_up(first));
    }

    #[test]
    fn ipynb_codec_is_a_hook_not_an_implementation() {
        let nb = Notebook::new("10.10");
        assert!(IpynbCodec.encode(&nb).is_err());
        assert!(IpynbCodec.decode(b"{}").is_err());
        assert_eq!(codec_for_path(Path::new("x.ipynb")).name(), "ipynb");
        assert_eq!(codec_for_path(Path::new("x.sagenote")).name(), "sagenote");
    }

    #[test]
    fn timestamp_is_plausible() {
        let stamp = utc_now_rfc3339();
        assert_eq!(stamp.len(), 20);
        assert!(stamp.ends_with('Z'));
        assert!(stamp.starts_with("20"));
    }
}
