//! Integration tests for the supervisor and framing using the canned Python
//! driver.  These run on Linux/macOS with only `python3`; no Sage, no display.
#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use sagenote::backend::DriverConfig;
use sagenote::backend::{
    BackendEvent, CellId, EvalBackend, Frame, InterruptScope, IntrospectId, MimeType, RunId,
    StdioDriverBackend,
};

fn backend(interrupt_timeout: Duration) -> StdioDriverBackend {
    let driver = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_driver.py");
    let mut config = DriverConfig::fake("python3", driver);
    config.interrupt_timeout = interrupt_timeout;
    let mut backend = StdioDriverBackend::new(config).expect("create backend");
    backend.start().expect("fake driver ready");
    backend
}

fn collect_until(
    rx: &Receiver<BackendEvent>,
    timeout: Duration,
    mut done: impl FnMut(&[BackendEvent]) -> bool,
) -> Vec<BackendEvent> {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => {
                events.push(event);
                if done(&events) {
                    return events;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    panic!("timed out; events so far: {events:#?}");
}

fn frame_is_end(event: &BackendEvent, cell: CellId, run: RunId) -> bool {
    matches!(event, BackendEvent::Frame(Frame::End { id, run: r }) if *id == cell && *r == run)
}

fn frame_is_start(event: &BackendEvent, cell: CellId, run: RunId) -> bool {
    matches!(event, BackendEvent::Frame(Frame::Start { id, run: r }) if *id == cell && *r == run)
}

#[test]
fn ready_start_stream_result_end() {
    let mut backend = backend(Duration::from_secs(2));
    let rx = backend.events();
    let cell = CellId::new();
    let run = RunId(1);
    backend.eval(cell, run, "latex").unwrap();

    let events = collect_until(&rx, Duration::from_secs(10), |events| {
        events.iter().any(|e| frame_is_end(e, cell, run))
    });

    assert!(events.iter().any(
        |e| matches!(e, BackendEvent::Ready { sage_version, .. } if sage_version == "fake-1.0")
    ));
    assert!(events.iter().any(|e| frame_is_start(e, cell, run)));
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Frame(Frame::Result { bundle, display, .. })
            if bundle.get(MimeType::TextLatex) == Some(r"\frac{1}{2}")
                && *display == sagenote::backend::DisplayMode::Block
    )));
    assert!(events.iter().any(|e| frame_is_end(e, cell, run)));
}

#[test]
fn stream_frames_are_typed() {
    let mut backend = backend(Duration::from_secs(2));
    let rx = backend.events();
    let cell = CellId::new();
    let run = RunId(2);
    backend.eval(cell, run, "stream").unwrap();
    let events = collect_until(&rx, Duration::from_secs(10), |events| {
        events.iter().any(|e| frame_is_end(e, cell, run))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Frame(Frame::Stream { name, text })
            if *name == sagenote::backend::StreamName::Stdout && text.contains("hello from fake")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Frame(Frame::Result { bundle, .. })
            if bundle.get(MimeType::TextPlain).is_some()
    )));
}

#[test]
fn raising_cell_emits_error_then_end() {
    let mut backend = backend(Duration::from_secs(2));
    let rx = backend.events();
    let cell = CellId::new();
    let run = RunId(3);
    backend.eval(cell, run, "raise").unwrap();
    let events = collect_until(&rx, Duration::from_secs(10), |events| {
        events.iter().any(|e| frame_is_end(e, cell, run))
    });
    let error_index = events
        .iter()
        .position(|e| matches!(e, BackendEvent::Frame(Frame::Error { traceback, .. }) if traceback.contains("fake failure")))
        .expect("error frame");
    let end_index = events
        .iter()
        .position(|e| frame_is_end(e, cell, run))
        .expect("end frame");
    assert!(error_index < end_index);
}

#[test]
fn sigint_aborts_cell_and_session_survives() {
    let mut backend = backend(Duration::from_secs(5));
    let rx = backend.events();
    let cell = CellId::new();
    let run = RunId(4);
    backend.eval(cell, run, "hang").unwrap();

    collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(|e| frame_is_start(e, cell, run))
    });
    backend.interrupt(InterruptScope::Cell).unwrap();

    let events = collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(|e| frame_is_end(e, cell, run))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Frame(Frame::Error { traceback, .. }) if traceback.contains("KeyboardInterrupt")
    )));
    assert!(!events
        .iter()
        .any(|e| matches!(e, BackendEvent::Interrupted { .. })));

    let cell2 = CellId::new();
    let run2 = RunId(5);
    backend.eval(cell2, run2, "1+1").unwrap();
    let events = collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(|e| frame_is_end(e, cell2, run2))
    });
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Frame(Frame::Result { bundle, .. })
            if bundle.get(MimeType::TextPlain).is_some()
    )));
}

#[test]
fn watchdog_kills_stuck_driver_and_restarts() {
    let mut backend = backend(Duration::from_millis(500));
    let rx = backend.events();
    let cell = CellId::new();
    let run = RunId(6);
    backend.eval(cell, run, "ignore-sigint").unwrap();

    collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(|e| frame_is_start(e, cell, run))
    });
    backend.interrupt(InterruptScope::Cell).unwrap();

    let events = collect_until(&rx, Duration::from_secs(8), |events| {
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::Restarted { .. }))
    });
    assert!(events
        .iter()
        .any(|e| matches!(e, BackendEvent::WatchdogFired { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, BackendEvent::Interrupted { .. })));

    let cell2 = CellId::new();
    let run2 = RunId(7);
    backend.eval(cell2, run2, "1+1").unwrap();
    let events = collect_until(&rx, Duration::from_secs(8), |events| {
        events.iter().any(|e| frame_is_end(e, cell2, run2))
    });
    assert!(events.iter().any(|e| frame_is_start(e, cell2, run2)));
}

#[test]
fn dead_driver_is_not_waited_on_forever() {
    let mut backend = backend(Duration::from_secs(2));
    let rx = backend.events();
    let cell = CellId::new();
    let run = RunId(8);
    backend.eval(cell, run, "die").unwrap();

    let events = collect_until(&rx, Duration::from_secs(10), |events| {
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::ProcessExited { .. }))
    });
    assert!(events
        .iter()
        .any(|e| matches!(e, BackendEvent::ProcessExited { code: Some(9), .. })));
    assert!(events.iter().any(
        |e| matches!(e, BackendEvent::Interrupted { cell: c, run: r } if *c == cell && *r == run)
    ));
}

#[test]
fn queued_cell_is_dropped_on_interrupt() {
    let mut backend = backend(Duration::from_secs(5));
    let rx = backend.events();
    let active = CellId::new();
    let active_run = RunId(9);
    let queued = CellId::new();
    let queued_run = RunId(10);
    backend.eval(active, active_run, "hang").unwrap();
    collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(|e| frame_is_start(e, active, active_run))
    });
    backend.eval(queued, queued_run, "1+1").unwrap();

    collect_until(&rx, Duration::from_secs(5), |events| {
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::Queued { cell, run } if *cell == queued && *run == queued_run))
    });
    backend.interrupt(InterruptScope::Cell).unwrap();

    let events = collect_until(&rx, Duration::from_secs(5), |events| {
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::Interrupted { cell, run } if *cell == queued && *run == queued_run))
            && events.iter().any(|e| frame_is_end(e, active, active_run))
    });
    assert!(events
        .iter()
        .any(|e| matches!(e, BackendEvent::Interrupted { .. })));
}

#[test]
fn introspection_and_completion() {
    let mut backend = backend(Duration::from_secs(2));
    let rx = backend.events();
    let request = IntrospectId(1);
    backend.introspect(request).unwrap();
    let events = collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(
            |e| matches!(e, BackendEvent::Frame(Frame::Introspect { id, .. }) if *id == request),
        )
    });
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Frame(Frame::Introspect { vars, .. })
            if vars.iter().any(|v| v.name == "x" && v.value == "42")
    )));

    let cell = CellId::new();
    backend.completion(cell, "pri", 3).unwrap();
    let events = collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(|e| {
            matches!(
                e,
                BackendEvent::Completion { cell: c, offset: 3, items }
                    if *c == cell && items.iter().any(|item| item.name == "print")
            )
        })
    });
    // The candidate's signature rides along on the Complete frame.
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Completion { items, .. }
            if items.iter().any(|item| item.name == "print" && item.signature.starts_with('('))
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Completion { items, .. } if items.len() >= 2
    )));
}

#[test]
fn documentation_is_forwarded_as_a_frame() {
    let mut backend = backend(Duration::from_secs(2));
    let rx = backend.events();
    let cell = CellId::new();
    backend.docs(cell, "pri", 3, None).unwrap();
    let events = collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(|e| {
            matches!(
                e,
                BackendEvent::Frame(Frame::Doc { id, offset: 3, name, .. })
                    if *id == cell && name == "pri"
            )
        })
    });
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Frame(Frame::Doc { text, signature, .. })
            if text.contains("Print") && signature.contains("print")
    )));
}

#[test]
fn named_documentation_lookup_round_trips() {
    let mut backend = backend(Duration::from_secs(2));
    let rx = backend.events();
    let cell = CellId::new();
    backend.docs(cell, "pri", 3, Some("plot")).unwrap();
    let events = collect_until(&rx, Duration::from_secs(5), |events| {
        events.iter().any(|e| {
            matches!(
                e,
                BackendEvent::Frame(Frame::Doc { id, name, .. })
                    if *id == cell && name == "plot"
            )
        })
    });
    assert!(events.iter().any(|e| matches!(
        e,
        BackendEvent::Frame(Frame::Doc { text, .. }) if text.contains("plot")
    )));
}

#[test]
fn declared_mimetypes_include_the_ui_types() {
    let backend = backend(Duration::from_secs(1));
    let declared = backend.declared_mimetypes();
    for mime in [
        MimeType::TextLatex,
        MimeType::ImageSvg,
        MimeType::ImagePng,
        MimeType::TextPlain,
    ] {
        assert!(declared.contains(&mime));
    }
}
