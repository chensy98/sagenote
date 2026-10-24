//! Sage discovery and the few genuinely platform-specific paths.
//!
//! Two Unix platforms, three call sites: a `trait Platform` would be ceremony
//! with no benefit.  Everything that differs is behind `cfg(target_os)` here.
//!
//! Discovery order (first *validated* hit wins):
//!   1. `SAGE_BIN`
//!   2. `~/.config/sagenote/config.toml`
//!   3. `PATH`
//!   4. macOS `/Applications/SageMath*.app` and `~/Applications/...`
//!   5. the GUI's browse button (M4) calls [`validate_sage`] and writes config.
//!
//! Every candidate is validated by **executing** `<candidate> --version`.  We
//! never use `which` or a file test to decide whether Sage is installed: a
//! candidate that is present but broken must produce a diagnosable failure, and
//! the version string is needed for the status bar.

use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How long `sage --version` may take.  The signed macOS bundle can cold-start
/// in 10–20 s; 90 s matches the protocol ready handshake.
const VERSION_TIMEOUT: Duration = Duration::from_secs(90);
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoverySource {
    Env,
    Config,
    Path,
    LoginShellPath,
    MacAppBundle,
    Explicit,
}

impl DiscoverySource {
    pub fn label(self) -> &'static str {
        match self {
            DiscoverySource::Env => "SAGE_BIN",
            DiscoverySource::Config => "config.toml",
            DiscoverySource::Path => "PATH",
            DiscoverySource::LoginShellPath => "PATH (login shell)",
            DiscoverySource::MacAppBundle => "macOS .app bundle",
            DiscoverySource::Explicit => "explicit --sage",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SageInstall {
    /// Absolute path as far as we can resolve it, for the status bar and logs.
    pub path: PathBuf,
    pub version: String,
    pub source: DiscoverySource,
    /// A login-shell PATH to splice into the child's environment.  Finder
    /// launches inherit only `/usr/bin:/bin:/usr/sbin:/sbin`.
    pub child_path: Option<std::ffi::OsString>,
}

#[derive(Clone, Debug)]
pub struct DiscoveryFailure {
    /// "SAGE_BIN", "config.toml", "PATH", ...
    pub stage: &'static str,
    /// The absolute (or plausible) candidate that failed.
    pub candidate: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct DiscoveryError {
    pub attempts: Vec<DiscoveryFailure>,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "SageMath was not found.")?;
        for attempt in &self.attempts {
            writeln!(
                f,
                "  [{}] {}: {}",
                attempt.stage, attempt.candidate, attempt.reason
            )?;
        }
        write!(
            f,
            "Install Sage, set SAGE_BIN, or choose a sage executable with Browse (or --sage in sadrive)."
        )
    }
}

impl std::error::Error for DiscoveryError {}

#[derive(Clone, Debug)]
pub struct ValidatedSage {
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug)]
pub struct ValidateError {
    pub reason: String,
    /// Spawn returned ENOENT: this candidate simply does not exist.  PATH probing
    /// uses this to avoid listing every PATH directory as a failure.
    pub not_found: bool,
}

impl ValidateError {
    fn other(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            not_found: false,
        }
    }
}

impl fmt::Display for ValidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ValidateError {}

/// Discover Sage.  Returns the first candidate that successfully runs
/// `--version`, or every failed attempt for the error dialog.
pub fn discover_sage() -> std::result::Result<SageInstall, DiscoveryError> {
    let mut attempts = Vec::new();

    // 1. SAGE_BIN
    match std::env::var_os("SAGE_BIN") {
        Some(raw) if !raw.is_empty() => {
            let path = match raw.to_str() {
                Some(text) => expand_tilde(text),
                None => PathBuf::from(&raw),
            };
            match validate_sage(&path) {
                Ok(v) => {
                    return Ok(SageInstall {
                        path: v.path,
                        version: v.version,
                        source: DiscoverySource::Env,
                        child_path: None,
                    })
                }
                Err(err) => attempts.push(DiscoveryFailure {
                    stage: "SAGE_BIN",
                    candidate: raw.to_string_lossy().into_owned(),
                    reason: err.reason,
                }),
            }
        }
        _ => attempts.push(DiscoveryFailure {
            stage: "SAGE_BIN",
            candidate: "(unset)".into(),
            reason: "environment variable is not set".into(),
        }),
    }

    // 2. config.toml written by the picker
    match configured_sage_path() {
        Ok(Some(path)) => match validate_sage(&path) {
            Ok(v) => {
                return Ok(SageInstall {
                    path: v.path,
                    version: v.version,
                    source: DiscoverySource::Config,
                    child_path: None,
                })
            }
            Err(err) => attempts.push(DiscoveryFailure {
                stage: "config.toml",
                candidate: path.display().to_string(),
                reason: err.reason,
            }),
        },
        Ok(None) => attempts.push(DiscoveryFailure {
            stage: "config.toml",
            candidate: config_path().display().to_string(),
            reason: "no configured sage entry".into(),
        }),
        Err(err) => attempts.push(DiscoveryFailure {
            stage: "config.toml",
            candidate: config_path().display().to_string(),
            reason: err.to_string(),
        }),
    }

    // 3. PATH.  On macOS, if this fails, re-derive it once through the login
    // shell before blaming the user.  The derived PATH is kept for the child.
    let inherited_path = std::env::var_os("PATH");
    match discover_on_path(inherited_path.as_deref(), "PATH", &mut attempts) {
        Some(v) => {
            return Ok(SageInstall {
                path: v.path,
                version: v.version,
                source: DiscoverySource::Path,
                child_path: None,
            })
        }
        None => {
            // Add a stage summary only if no candidate was actually executed
            // (a missing `dir/sage` is not worth a line per PATH entry).
            if !attempts.iter().any(|a| a.stage == "PATH") {
                attempts.push(DiscoveryFailure {
                    stage: "PATH",
                    candidate: inherited_path
                        .as_deref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "(unset)".into()),
                    reason: "no executable sage in PATH".into(),
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    let mut derived_path: Option<std::ffi::OsString> = None;

    #[cfg(target_os = "macos")]
    {
        if let Some(login_path) = login_shell_path() {
            let before = attempts.len();
            if let Some(v) = discover_on_path(
                Some(login_path.as_os_str()),
                "PATH (login shell)",
                &mut attempts,
            ) {
                return Ok(SageInstall {
                    path: v.path,
                    version: v.version,
                    source: DiscoverySource::LoginShellPath,
                    child_path: Some(login_path),
                });
            }
            if attempts.len() == before {
                attempts.push(DiscoveryFailure {
                    stage: "PATH (login shell)",
                    candidate: login_path.to_string_lossy().into_owned(),
                    reason: "no executable sage after re-deriving PATH".into(),
                });
            }
            derived_path = Some(login_path);
        } else {
            attempts.push(DiscoveryFailure {
                stage: "PATH (login shell)",
                candidate: "$SHELL -lic 'echo \"$PATH\"'".into(),
                reason: "could not re-derive PATH".into(),
            });
        }
    }

    // 4. macOS app bundles.  Linux gets an explicit N/A line so the error
    // dialog still explains all five discovery steps.
    #[cfg(target_os = "macos")]
    {
        let before = attempts.len();
        match discover_app_bundles(&mut attempts) {
            Some(v) => {
                return Ok(SageInstall {
                    path: v.path,
                    version: v.version,
                    source: DiscoverySource::MacAppBundle,
                    child_path: derived_path,
                })
            }
            None if attempts.len() == before => attempts.push(DiscoveryFailure {
                stage: "macOS .app bundle",
                candidate: "/Applications/SageMath*.app".into(),
                reason: "no SageMath .app bundle found".into(),
            }),
            None => {}
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        attempts.push(DiscoveryFailure {
            stage: "macOS .app bundle",
            candidate: "(not applicable)".into(),
            reason: "macOS-only discovery step".into(),
        });
    }

    // 5. Browse is a UI action (M4).  It has not been attempted yet.
    attempts.push(DiscoveryFailure {
        stage: "browse",
        candidate: "(not attempted)".into(),
        reason: "no file chosen; use Browse or --sage".into(),
    });

    Err(DiscoveryError { attempts })
}

/// Validate one candidate by running `<candidate> --version` and reading its
/// stdout/stderr.  This is the only oracle; no `which`, no file existence test.
pub fn validate_sage(candidate: &Path) -> std::result::Result<ValidatedSage, ValidateError> {
    let captured = run_with_timeout(candidate, &["--version"], VERSION_TIMEOUT).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ValidateError {
                reason: format!("cannot execute {}: {}", candidate.display(), e),
                not_found: true,
            }
        } else {
            ValidateError::other(format!("cannot execute {}: {}", candidate.display(), e))
        }
    })?;

    if captured.timed_out {
        return Err(ValidateError::other(format!(
            "`{} --version` timed out after {} s",
            candidate.display(),
            VERSION_TIMEOUT.as_secs()
        )));
    }

    if !captured.status.success() {
        let detail = first_meaningful_line(&captured.stderr)
            .or_else(|| first_meaningful_line(&captured.stdout))
            .unwrap_or_else(|| "(no output)".into());
        return Err(ValidateError::other(format!(
            "`{} --version` exited with {}: {}",
            candidate.display(),
            captured
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
            truncate(&detail, 500)
        )));
    }

    let version = extract_version(&captured.stdout, &captured.stderr);
    let resolved = resolve_absolute(candidate);
    Ok(ValidatedSage {
        path: resolved,
        version,
    })
}

/// Save a picker choice for the next launch.  The filename is fixed by the
/// spec; atomic replace keeps a half-written config from breaking startup.
pub fn save_config_sage(path: &Path) -> Result<()> {
    let config = ConfigFile {
        sage: Some(path.to_string_lossy().into_owned()),
        sage_bin: None,
    };
    let text = toml::to_string_pretty(&config).context("serialising config.toml")?;
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
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

/// `~/.config/sagenote/config.toml` on both platforms, deliberately *not*
/// `dirs::config_dir()` on macOS (that would be Application Support).
pub fn config_path() -> PathBuf {
    home_dir()
        .join(".config")
        .join("sagenote")
        .join("config.toml")
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Per-pid scratch directory for the extracted driver.  `std::env::temp_dir()`
/// is correct on both platforms; `$XDG_RUNTIME_DIR` does not exist on macOS.
pub fn temp_driver_dir() -> PathBuf {
    std::env::temp_dir().join(format!("sagenote-{}", std::process::id()))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    /// The picker writes `sage = "/abs/path"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sage: Option<String>,
    /// Accepted as an alias for hand-written configs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sage_bin: Option<String>,
}

fn configured_sage_path() -> Result<Option<PathBuf>> {
    let path = config_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config: ConfigFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let entry = config.sage.or(config.sage_bin);
    Ok(entry
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .map(|s| expand_tilde(&s)))
}

/// Probe a PATH-like string.  Candidates are executed, never stat'ed first.
fn discover_on_path(
    path_env: Option<&std::ffi::OsStr>,
    stage: &'static str,
    attempts: &mut Vec<DiscoveryFailure>,
) -> Option<ValidatedSage> {
    let path_env = path_env?;
    let mut seen = std::collections::HashSet::new();
    for dir in std::env::split_paths(path_env) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let candidate = dir.join("sage");
        if !seen.insert(candidate.clone()) {
            continue;
        }
        match validate_sage(&candidate) {
            Ok(v) => return Some(v),
            Err(err) if err.not_found => {
                // Typing the path into a PATH directory is not a discovery hit.
                // Do not litter the dialog with every directory on the system.
                continue;
            }
            Err(err) => attempts.push(DiscoveryFailure {
                stage,
                candidate: candidate.display().to_string(),
                reason: err.reason,
            }),
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn login_shell_path() -> Option<std::ffi::OsString> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
    let captured = run_with_timeout(
        Path::new(&shell),
        &["-lic", "echo \"$PATH\""],
        LOGIN_SHELL_TIMEOUT,
    )
    .ok()?;
    if !captured.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&captured.stdout);
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty() && line.contains(':'))?;
    Some(std::ffi::OsString::from(line))
}

#[cfg(target_os = "macos")]
fn discover_app_bundles(attempts: &mut Vec<DiscoveryFailure>) -> Option<ValidatedSage> {
    let mut roots = vec![PathBuf::from("/Applications")];
    let home_apps = home_dir().join("Applications");
    if home_apps != PathBuf::from("/Applications") {
        roots.push(home_apps);
    }

    let mut bundles = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("SageMath") && name.ends_with(".app") {
                bundles.push(entry.path());
            }
        }
    }
    bundles.sort();

    let mut tried = std::collections::HashSet::new();
    for bundle in bundles {
        let versions = bundle.join("Contents/Frameworks/Sage.framework/Versions");
        let mut version_dirs = Vec::new();
        if let Ok(entries) = fs::read_dir(&versions) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name != "Current" && entry.path().is_dir() {
                    version_dirs.push((name, entry.path()));
                }
            }
        }
        version_dirs.sort_by(|a, b| version_sort_key(&b.0).cmp(&version_sort_key(&a.0)));

        // `Current` may be a dangling symlink and may point at an older
        // version than the highest installed one.  Try the versioned
        // directories highest-first, then Current as a last resort.
        let mut candidates = Vec::new();
        for (_, dir) in version_dirs {
            candidates.push(dir.join("local/bin/sage"));
        }
        let current = versions.join("Current/local/bin/sage");
        if current.exists() {
            candidates.push(current);
        }

        for candidate in candidates {
            if !tried.insert(candidate.clone()) {
                continue;
            }
            match validate_sage(&candidate) {
                Ok(v) => return Some(v),
                Err(err) => attempts.push(DiscoveryFailure {
                    stage: "macOS .app bundle",
                    candidate: candidate.display().to_string(),
                    reason: err.reason,
                }),
            }
        }
    }
    None
}

struct Captured {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

/// Spawn a command, drain both pipes on threads, and enforce a wall-clock
/// timeout.  `Command::output()` has no timeout and would make a broken Sage
/// look like a hang.
fn run_with_timeout(program: &Path, args: &[&str], timeout: Duration) -> std::io::Result<Captured> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Validation must not leave a shell-wrapper child behind if it hangs.
    command.process_group(0);
    let mut child = command.spawn()?;
    // Both pipes were requested above, so take() can only fail if the spawn
    // somehow dropped them.  Kill and reap the child rather than panicking
    // inside the GUI's startup path.
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("child pipes were not piped"));
    };
    let stdout_thread = std::thread::spawn(move || {
        let mut data = Vec::new();
        let _ = stdout.read_to_end(&mut data);
        data
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut data = Vec::new();
        let _ = stderr.read_to_end(&mut data);
        data
    });

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            let pid = child.id() as i32;
            // SAFETY: libc::kill only reads its arguments, which are valid:
            // SIGKILL is a libc constant and the child was spawned with
            // process_group(0), so -pid names the group we created.  Nothing
            // is borrowed across the call, so no aliasing is possible.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok(Captured {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

fn extract_version(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    // Prefer the canonical version line, so an import warning printed first
    // does not become the status-bar string.
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if line.contains("SageMath version") {
            return line.to_owned();
        }
    }
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if !line.is_empty() {
            return line.to_owned();
        }
    }
    "unknown".into()
}

fn first_meaningful_line(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn expand_tilde(text: &str) -> PathBuf {
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if text == "~" {
        return home_dir();
    }
    PathBuf::from(text)
}

/// Best-effort absolute path for display.  Validation already ran the binary;
/// this fallback searches PATH only to name it in the status bar.
fn resolve_absolute(candidate: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(candidate) {
        return path;
    }
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return candidate.to_path_buf();
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let joined = dir.join(candidate);
            if joined.is_file() {
                return fs::canonicalize(&joined).unwrap_or(joined);
            }
        }
    }
    candidate.to_path_buf()
}

/// Natural sort key for `10.10 > 9.8 > 9.6`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn version_sort_key(version: &str) -> Vec<u64> {
    let mut parts = Vec::new();
    let mut current = 0u64;
    let mut in_number = false;
    for ch in version.chars() {
        if let Some(digit) = ch.to_digit(10) {
            current = current.saturating_mul(10).saturating_add(digit as u64);
            in_number = true;
        } else if in_number {
            parts.push(current);
            current = 0;
            in_number = false;
        }
    }
    if in_number {
        parts.push(current);
    }
    if parts.is_empty() {
        parts.push(0);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_version_sort_prefers_10_10_over_9_8() {
        let mut versions = vec!["9.8", "10.10", "9.10", "10.4", "Current"];
        versions.sort_by_key(|v| version_sort_key(v));
        assert_eq!(versions, vec!["Current", "9.8", "9.10", "10.4", "10.10"]);
    }

    #[test]
    fn version_text_is_the_first_nonempty_line() {
        assert_eq!(
            extract_version(b"\nSageMath version 10.10, Release Date: 2025-01-01\n", b""),
            "SageMath version 10.10, Release Date: 2025-01-01"
        );
        assert_eq!(extract_version(b"", b""), "unknown");
        assert_eq!(extract_version(b"", b"warning\n10.10\n"), "warning");
        assert_eq!(
            extract_version(b"warning first\nSageMath version 10.10\n", b""),
            "SageMath version 10.10"
        );
    }

    #[test]
    fn config_default_is_empty() {
        let config: ConfigFile = toml::from_str("").unwrap();
        assert!(config.sage.is_none());
    }

    #[test]
    fn config_alias_is_accepted() {
        let config: ConfigFile = toml::from_str("sage_bin = \"/opt/sage\"").unwrap();
        assert_eq!(config.sage_bin.as_deref(), Some("/opt/sage"));
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn validation_runs_version_not_file_test() {
        let dir = std::env::temp_dir().join(format!(
            "sagenote-platform-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let good = write_script(&dir, "sage", "#!/bin/sh\necho '10.10'\n");
        let validated = validate_sage(&good).unwrap();
        assert_eq!(validated.version, "10.10");
        assert!(validated.path.is_absolute());

        let broken = write_script(&dir, "broken-sage", "#!/bin/sh\necho 'boom' >&2\nexit 3\n");
        let err = validate_sage(&broken).unwrap_err();
        assert!(err.reason.contains("boom"), "{}", err.reason);
        assert!(!err.not_found);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tilde_expands_to_home() {
        let expanded = expand_tilde("~/Applications/SageMath.app");
        assert!(expanded.starts_with(home_dir()));
    }
}
