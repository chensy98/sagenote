//! `sadrive` — headless probe for the Sage stdio driver.
//!
//! It discovers/spawns Sage, evaluates cells, prints the typed frame stream,
//! exercises SIGINT and the watchdog, and notices a driver that dies mid-cell.
//! There is intentionally no GUI in this binary; the GUI lives in the sagenote binary.
//!
//! Examples:
//!   cargo run --bin sadrive
//!   cargo run --bin sadrive -- --fake-driver -e 'latex' -e 'stream'
//!   cargo run --bin sadrive -- -e 'while True: pass' --interrupt-after 1 -e '1+1'
//!   cargo run --bin sadrive -- -e 'ignore-sigint' --interrupt-after 1 --interrupt-timeout 2
//!   cargo run --bin sadrive -- -e 'import os; os._exit(9)'
//!
//! Over SSH/llvmpipe the GUI needs WGPU_BACKEND=gl; this binary has no wgpu.

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use sagenote::backend::{
    BackendEvent, CellId, DisplayMode, DriverConfig, EvalBackend, Frame, InterruptScope,
    IntrospectId, MimeType, RunId, StdioDriverBackend,
};
use sagenote::platform::{self, DiscoverySource, SageInstall};

const DEFAULT_CELL: &str = "a = 6 * 7\na";

fn main() {
    std::process::exit(match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("sadrive: {message}");
            2
        }
    });
}

struct Args {
    sage: Option<PathBuf>,
    driver: Option<PathBuf>,
    fake_driver: bool,
    python: Option<PathBuf>,
    evals: Vec<String>,
    script: Option<PathBuf>,
    display: DisplayMode,
    interrupt_after: Option<Duration>,
    interrupt_cell: usize,
    interrupt_timeout: Duration,
    ready_timeout: Duration,
    eval_timeout: Option<Duration>,
    introspect: bool,
    docs: bool,
    raw: bool,
    verbose: bool,
    strict: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            sage: None,
            driver: None,
            fake_driver: false,
            python: None,
            evals: Vec::new(),
            script: None,
            display: DisplayMode::Block,
            interrupt_after: None,
            interrupt_cell: 0,
            interrupt_timeout: Duration::from_secs(5),
            ready_timeout: Duration::from_secs(90),
            eval_timeout: Some(Duration::from_secs(120)),
            introspect: false,
            docs: false,
            raw: false,
            verbose: false,
            strict: false,
        }
    }
}

fn run() -> Result<i32, String> {
    let args = parse_args()?;
    if args.raw {
        eprintln!("# raw frame mode; lifecycle notes go to stderr");
    }

    let cells = load_cells(&args)?;
    let (mut backend, label) = build_backend(&args)?;
    if args.raw {
        eprintln!("{label}");
        eprintln!(
            "display={} interrupt_timeout={}s ready_timeout={}s",
            args.display.as_str(),
            args.interrupt_timeout.as_secs(),
            args.ready_timeout.as_secs()
        );
    } else {
        println!("{label}");
        println!(
            "display={} interrupt_timeout={}s ready_timeout={}s",
            args.display.as_str(),
            args.interrupt_timeout.as_secs(),
            args.ready_timeout.as_secs()
        );
        println!("starting Sage …");
    }
    backend
        .start()
        .map_err(|err| format!("could not start the Sage driver: {err:#}"))?;

    let events = backend.events();
    let mut runner = Runner {
        backend,
        events,
        args,
        next_run: 1,
        fatal: false,
        any_error: false,
        restart_pending: false,
    };
    runner.drain_pending();

    for (index, source) in cells.iter().enumerate() {
        if runner.fatal {
            break;
        }
        if !runner.args.raw {
            println!("\n=== cell {index} ===");
            println!("{}", source);
            println!("--- frames ---");
        } else {
            eprintln!("# === cell {index} ===");
        }
        match runner.run_cell(source, index) {
            Outcome::Finished => {}
            Outcome::Error => runner.any_error = true,
            Outcome::Fatal => {
                runner.fatal = true;
                break;
            }
        }
        // Interrupted/ProcessExited/Restarted often arrive together.  Drain them
        // before printing the next cell header so the transcript reads in order.
        runner.drain_pending();
        if runner.restart_pending && !runner.fatal {
            runner.wait_for_restart();
        }
    }

    if runner.args.introspect && !runner.fatal {
        runner.request_introspection();
    }
    if runner.args.docs && !runner.fatal {
        runner.request_docs();
    }

    if runner.fatal {
        return Ok(1);
    }
    if runner.any_error && runner.args.strict {
        return Ok(1);
    }
    Ok(0)
}

fn build_backend(args: &Args) -> Result<(StdioDriverBackend, String), String> {
    if args.fake_driver {
        let python = args
            .python
            .clone()
            .unwrap_or_else(|| PathBuf::from("python3"));
        let driver = args.driver.clone().unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_driver.py")
        });
        if !driver.exists() {
            return Err(format!(
                "fake driver fixture not found at {} (pass --driver)",
                driver.display()
            ));
        }
        let mut config = DriverConfig::fake(python, driver);
        config.display = args.display;
        config.ready_timeout = args.ready_timeout;
        config.interrupt_timeout = args.interrupt_timeout;
        let label = format!(
            "sage: FAKE driver={} python={}",
            config.driver_override.as_ref().unwrap().display(),
            config.sage_path.display()
        );
        let backend = StdioDriverBackend::new(config)
            .map_err(|err| format!("creating fake backend: {err:#}"))?;
        return Ok((backend, label));
    }

    let install = if let Some(path) = &args.sage {
        let validated = platform::validate_sage(path).map_err(|err| {
            format!(
                "{} is not a working Sage executable: {}",
                path.display(),
                err.reason
            )
        })?;
        SageInstall {
            path: validated.path,
            version: validated.version,
            source: DiscoverySource::Explicit,
            child_path: None,
        }
    } else {
        platform::discover_sage().map_err(|err| err.to_string())?
    };

    let mut config = DriverConfig::new(install.path.clone());
    config.sage_path = install.path.clone();
    config.driver_override = args.driver.clone();
    config.display = args.display;
    config.ready_timeout = args.ready_timeout;
    config.interrupt_timeout = args.interrupt_timeout;
    config.child_path = install.child_path.clone();

    let label = format!(
        "sage: {}  (version {}; found via {})",
        install.path.display(),
        install.version,
        install.source.label()
    );
    let backend =
        StdioDriverBackend::new(config).map_err(|err| format!("creating backend: {err:#}"))?;
    Ok((backend, label))
}

enum Outcome {
    Finished,
    Error,
    Fatal,
}

struct Runner {
    backend: StdioDriverBackend,
    events: Receiver<BackendEvent>,
    args: Args,
    next_run: u64,
    fatal: bool,
    any_error: bool,
    restart_pending: bool,
}

impl Runner {
    fn drain_pending(&mut self) {
        let mut finished = false;
        let mut had_error = false;
        while let Ok(event) = self.events.try_recv() {
            self.handle_event(event, None, &mut finished, &mut had_error);
        }
    }

    fn run_cell(&mut self, source: &str, index: usize) -> Outcome {
        let cell = CellId::new();
        let run = RunId(self.next_run);
        self.next_run += 1;
        if let Err(err) = self.backend.eval(cell, run, source) {
            eprintln!("sadrive: eval request failed: {err:#}");
            return Outcome::Fatal;
        }

        let started = Instant::now();
        let mut interrupt_at = if index == self.args.interrupt_cell {
            self.args.interrupt_after.map(|after| started + after)
        } else {
            None
        };
        let mut deadline = self.args.eval_timeout.map(|timeout| started + timeout);
        let mut forced_deadline: Option<Instant> = None;
        let mut had_error = false;
        let mut finished = false;
        let mut fatal = false;

        while !finished && !fatal {
            let now = Instant::now();

            if let Some(at) = interrupt_at {
                if now >= at {
                    if !self.args.raw {
                        println!("--- SIGINT (interrupt_after) ---");
                    } else {
                        eprintln!("# SIGINT (interrupt_after)");
                    }
                    let _ = self.backend.interrupt(InterruptScope::Cell);
                    interrupt_at = None;
                    forced_deadline =
                        Some(now + self.args.interrupt_timeout + Duration::from_secs(3));
                }
            }

            if let Some(at) = deadline {
                if now >= at {
                    if !self.args.raw {
                        println!("--- eval timeout; sending SIGINT ---");
                    } else {
                        eprintln!("# eval timeout; sending SIGINT");
                    }
                    let _ = self.backend.interrupt(InterruptScope::Cell);
                    deadline = None;
                    forced_deadline =
                        Some(now + self.args.interrupt_timeout + Duration::from_secs(3));
                }
            }

            if let Some(at) = forced_deadline {
                if now >= at {
                    eprintln!("sadrive: no end after interrupt; giving up");
                    return Outcome::Fatal;
                }
            }

            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(event) => self.handle_event(
                    event,
                    Some((cell, run, started)),
                    &mut finished,
                    &mut had_error,
                ),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    eprintln!("sadrive: supervisor event channel disconnected");
                    return Outcome::Fatal;
                }
            }
            if self.fatal {
                fatal = true;
            }
        }

        if fatal {
            Outcome::Fatal
        } else if had_error {
            Outcome::Error
        } else {
            Outcome::Finished
        }
    }

    fn handle_event(
        &mut self,
        event: BackendEvent,
        current: Option<(CellId, RunId, Instant)>,
        finished: &mut bool,
        had_error: &mut bool,
    ) {
        match event {
            BackendEvent::Ready {
                sage_path,
                sage_version,
            } => {
                if self.args.raw {
                    println!(
                        "{{\"type\":\"ready\",\"sage_version\":{}}}",
                        serde_json::to_string(&sage_version).unwrap_or_else(|_| "\"?\"".into())
                    );
                    eprintln!("# sage_path={}", sage_path.display());
                } else {
                    println!("ready: Sage {}  ({})", sage_version, sage_path.display());
                }
            }
            BackendEvent::Frame(frame) => {
                if self.args.raw {
                    if let Ok(line) = frame.to_json_line() {
                        println!("{line}");
                    }
                } else {
                    print_frame(&frame, current.map(|(c, r, _)| (c, r)));
                }
                if let Some((cell, run, started)) = current {
                    match &frame {
                        Frame::End { id, run: ended } if *id == cell && *ended == run => {
                            if self.args.raw {
                                eprintln!("# end ({:.3}s)", started.elapsed().as_secs_f64());
                            } else {
                                println!("--- end ({:.3}s) ---", started.elapsed().as_secs_f64());
                            }
                            *finished = true;
                        }
                        Frame::Error {
                            id, run: errored, ..
                        } if *id == cell && *errored == run => {
                            *had_error = true;
                        }
                        _ => {}
                    }
                }
            }
            BackendEvent::Queued { cell, run } => {
                if !self.args.raw {
                    println!("queued cell={cell} run={run}");
                }
            }
            BackendEvent::Interrupted { cell, run } => {
                if self.args.raw {
                    eprintln!("# interrupted cell={cell} run={run}");
                } else {
                    println!("interrupted cell={cell} run={run}");
                }
                if let Some((current_cell, current_run, _)) = current {
                    if current_cell == cell && current_run == run {
                        *finished = true;
                    }
                }
            }
            BackendEvent::WatchdogFired { cells, timeout } => {
                self.restart_pending = true;
                if self.args.raw {
                    eprintln!(
                        "# watchdog fired after {:?} for {} cell(s)",
                        timeout,
                        cells.len()
                    );
                } else {
                    println!(
                        "watchdog: no end after {:?}; SIGKILL + restart ({} cell(s))",
                        timeout,
                        cells.len()
                    );
                }
            }
            BackendEvent::Restarted { sage_version } => {
                self.restart_pending = false;
                if self.args.raw {
                    eprintln!("# restarted: {sage_version}");
                } else {
                    println!("restarted: Sage {sage_version}");
                }
            }
            BackendEvent::Completion {
                cell,
                offset,
                items,
            } => {
                if self.args.raw {
                    eprintln!(
                        "# completion cell={cell} offset={offset} items={}",
                        items.len()
                    );
                } else {
                    println!("completion cell={cell} offset={offset} items={items:?}");
                }
            }
            BackendEvent::Stderr { text } => {
                if self.args.verbose {
                    eprint!("[driver stderr] {text}");
                }
            }
            BackendEvent::ProtocolWarning { message } => {
                eprintln!("[protocol warning] {message}");
            }
            BackendEvent::ProcessExited { code, message } => {
                eprintln!(
                    "sadrive: {message}{}",
                    code.map(|c| format!(" (code {c})")).unwrap_or_default()
                );
                self.fatal = true;
            }
            BackendEvent::Fatal { message } => {
                eprintln!("sadrive: fatal: {message}");
                self.fatal = true;
            }
        }
    }

    fn wait_for_restart(&mut self) {
        let deadline = Instant::now() + self.args.ready_timeout + Duration::from_secs(5);
        while self.restart_pending && !self.fatal && Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    let mut finished = false;
                    let mut had_error = false;
                    self.handle_event(event, None, &mut finished, &mut had_error);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if self.restart_pending && !self.fatal {
            eprintln!("sadrive: timed out waiting for the watchdog restart");
            self.fatal = true;
        }
        self.restart_pending = false;
    }

    fn request_introspection(&mut self) {
        let request = IntrospectId(1);
        if let Err(err) = self.backend.introspect(request) {
            eprintln!("sadrive: introspect request failed: {err:#}");
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    let mut finished = false;
                    let mut had_error = false;
                    let is_introspect = matches!(
                        &event,
                        BackendEvent::Frame(Frame::Introspect { id, .. }) if *id == request
                    );
                    self.handle_event(event, None, &mut finished, &mut had_error);
                    if is_introspect {
                        return;
                    }
                    if self.fatal {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        eprintln!("sadrive: timed out waiting for introspection");
    }

    fn request_docs(&mut self) {
        let cell = CellId::new();
        if let Err(err) = self.backend.docs(cell, "plot", 4, Some("plot")) {
            eprintln!("sadrive: docs request failed: {err:#}");
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    let mut finished = false;
                    let mut had_error = false;
                    let is_doc = matches!(
                        &event,
                        BackendEvent::Frame(Frame::Doc { id, .. }) if *id == cell
                    );
                    self.handle_event(event, None, &mut finished, &mut had_error);
                    if is_doc {
                        return;
                    }
                    if self.fatal {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        eprintln!("sadrive: timed out waiting for documentation");
    }
}

fn print_frame(frame: &Frame, current: Option<(CellId, RunId)>) {
    match frame {
        Frame::Ready { sage_version } => println!("ready: {sage_version}"),
        Frame::Start { id, run } => {
            if current
                .map(|(c, r)| (c == *id, r == *run))
                .unwrap_or((false, false))
                != (true, true)
            {
                println!("start cell={id} run={run} (stale)");
            } else {
                println!("start cell={id} run={run}");
            }
        }
        Frame::Stream { name, text } => {
            println!("[{}]", name.as_str());
            print_block(text, 8_000);
        }
        Frame::Result {
            id,
            run,
            bundle,
            display,
        } => {
            let stale = current.map(|(c, r)| c != *id || r != *run).unwrap_or(false);
            println!(
                "result cell={id} run={run} display={} {}",
                display.as_str(),
                if stale { "(stale)" } else { "" }
            );
            for (mime, data) in bundle.iter() {
                if mime == MimeType::ImagePng.as_str() {
                    println!("  [{mime}] {} base64 characters", data.len());
                } else {
                    println!("  [{mime}]");
                    print_block(data, 4_000);
                }
            }
        }
        Frame::Error { id, run, traceback } => {
            println!("error cell={id} run={run}");
            print_block(traceback, 16_000);
        }
        Frame::Introspect { id, vars } => {
            println!("introspect id={id} ({} variables)", vars.len());
            for var in vars {
                println!("  {:<20} {:<20} {}", var.name, var.type_name, var.value);
            }
        }
        Frame::Complete { id, offset, items } => {
            println!("complete cell={id} offset={offset} ({} items)", items.len());
            for item in items {
                let sig = if item.signature.is_empty() {
                    String::new()
                } else {
                    format!(" {}", item.signature)
                };
                println!("  {}{} [{:?}]", item.name, sig, item.kind);
            }
        }
        Frame::Doc {
            id,
            offset,
            name,
            type_name,
            signature,
            text,
        } => {
            println!("doc cell={id} offset={offset} name={name} type={type_name}");
            if !signature.is_empty() {
                println!("  signature: {signature}");
            }
            print_block(text, 8_000);
        }
        Frame::End { id, run } => println!("end cell={id} run={run}"),
        Frame::Pong => println!("pong"),
        Frame::Unknown => println!("unknown frame (ignored)"),
    }
}

fn print_block(text: &str, max: usize) {
    let text = if text.chars().count() > max {
        let mut truncated: String = text.chars().take(max).collect();
        truncated.push_str("\n[... sadrive display truncated ...]");
        truncated
    } else {
        text.to_owned()
    };
    for line in text.trim_end_matches('\n').lines() {
        println!("  {line}");
    }
}

fn load_cells(args: &Args) -> Result<Vec<String>, String> {
    let mut cells = Vec::new();
    if let Some(path) = &args.script {
        let text = if path == std::path::Path::new("-") {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|err| format!("reading stdin: {err}"))?;
            text
        } else {
            if path.extension().and_then(|e| e.to_str()) == Some("ipynb") {
                return Err("ipynb import is not implemented in this prototype".into());
            }
            std::fs::read_to_string(path)
                .map_err(|err| format!("reading {}: {err}", path.display()))?
        };
        cells.extend(split_cells(&text));
    }
    cells.extend(args.evals.iter().cloned());
    if cells.is_empty() {
        cells.push(DEFAULT_CELL.to_owned());
    }
    Ok(cells)
}

/// Jupyter-style `# %%` separators; a file without markers is one cell.
fn split_cells(text: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("# %%") || line.trim() == "#%%" {
            if !current.trim().is_empty() {
                cells.push(current.trim_end().to_owned());
            }
            current.clear();
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        cells.push(current.trim_end().to_owned());
    }
    cells
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut positional: VecDeque<String> = VecDeque::new();
    let mut iter = std::env::args().skip(1);
    let mut positional_only = false;

    while let Some(arg) = iter.next() {
        if positional_only {
            positional.push_back(arg);
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (arg.clone(), None),
        };
        match flag.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--version" => {
                println!("sadrive {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--sage" => args.sage = Some(PathBuf::from(need(inline, &mut iter, "--sage")?)),
            "--driver" => args.driver = Some(PathBuf::from(need(inline, &mut iter, "--driver")?)),
            "--fake-driver" => args.fake_driver = true,
            "--python" => args.python = Some(PathBuf::from(need(inline, &mut iter, "--python")?)),
            "-e" | "--eval" => args.evals.push(need(inline, &mut iter, "--eval")?),
            "--script" => args.script = Some(PathBuf::from(need(inline, &mut iter, "--script")?)),
            "--display" => args.display = parse_display(&need(inline, &mut iter, "--display")?)?,
            "--interrupt-after" => {
                args.interrupt_after =
                    Some(seconds(&need(inline, &mut iter, "--interrupt-after")?)?)
            }
            "--interrupt-cell" => {
                args.interrupt_cell = need(inline, &mut iter, "--interrupt-cell")?
                    .parse()
                    .map_err(|_| "--interrupt-cell expects a non-negative index".to_owned())?
            }
            "--interrupt-timeout" => {
                args.interrupt_timeout = seconds(&need(inline, &mut iter, "--interrupt-timeout")?)?
            }
            "--ready-timeout" => {
                args.ready_timeout = seconds(&need(inline, &mut iter, "--ready-timeout")?)?
            }
            "--eval-timeout" => {
                let value = seconds(&need(inline, &mut iter, "--eval-timeout")?)?;
                args.eval_timeout = (!value.is_zero()).then_some(value);
            }
            "--introspect" => args.introspect = true,
            "--docs" => args.docs = true,
            "--raw" | "--json" => args.raw = true,
            "--verbose" | "-v" => args.verbose = true,
            "--strict" => args.strict = true,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option {other}; try --help"))
            }
            _ => positional.push_back(arg),
        }
    }

    if positional.len() > 1 {
        return Err("expected at most one script file".into());
    }
    args.script = positional.pop_front().map(PathBuf::from).or(args.script);
    Ok(args)
}

fn need<T: Iterator<Item = String>>(
    inline: Option<String>,
    iter: &mut T,
    flag: &str,
) -> Result<String, String> {
    inline
        .or_else(|| iter.next())
        .ok_or_else(|| format!("{flag} expects a value"))
}

fn parse_display(value: &str) -> Result<DisplayMode, String> {
    match value {
        "block" => Ok(DisplayMode::Block),
        "inline" => Ok(DisplayMode::Inline),
        other => Err(format!("display must be block or inline, got {other:?}")),
    }
}

fn seconds(value: &str) -> Result<Duration, String> {
    let seconds: f64 = value
        .parse()
        .map_err(|_| format!("expected seconds, got {value:?}"))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("expected non-negative seconds, got {value:?}"));
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| format!("seconds value is out of range: {value:?}"))
}

fn print_help() {
    println!(
        r##"sadrive — headless Sage driver probe

USAGE:
    sadrive [OPTIONS] [SCRIPT]

    A SCRIPT may contain Jupyter-style "# %%" cell separators; without them the
    whole file is one cell.  With no SCRIPT and no -e, a small demo cell runs.

SAGE:
    --sage PATH            validate and use this sage (skips discovery)
    --driver PATH          driver.py override (explicit; beats SAGE_DRIVER_OVERRIDE)
    --fake-driver          run tests/fixtures/fake_driver.py under python3
    --python PATH          python for --fake-driver (default: python3)

EVALUATION:
    -e, --eval SOURCE      evaluate SOURCE (repeatable)
    --script PATH          read cells from PATH ("-" reads stdin)
    --display block|inline driver display hint for result frames
    --interrupt-after SECS send SIGINT this long after cell --interrupt-cell starts
    --interrupt-cell N     zero-based cell for --interrupt-after (default 0)
    --interrupt-timeout S  watchdog grace after SIGINT (default 5)
    --ready-timeout S      ready handshake timeout (default 90)
    --eval-timeout S       per-cell wall clock before SIGINT; 0 disables (default 120)
    --introspect           request the variable inspector once at the end
    --docs                 request documentation for `plot` once at the end

OUTPUT:
    --raw, --json          print one JSON frame per line (lifecycle notes on stderr)
    -v, --verbose          show driver stderr
    --strict               exit 1 if any cell produced an error frame
    -h, --help             this text
    --version              version

EXAMPLES:
    cargo run --bin sadrive
    cargo run --bin sadrive -- --fake-driver -e latex -e raise -e '1+1' --introspect
    cargo run --bin sadrive -- -e 'while True: pass' --interrupt-after 1 -e '1+1'
    cargo run --bin sadrive -- -e 'import os; os._exit(9)'
"##
    );
}
