//! sagenote — a Rust-native SageMath notebook.
//!
//! The crate is deliberately split so that the protocol, process supervision and
//! notebook model are usable without a windowing system:
//!
//! * [`backend`] — the wire protocol and the stdio driver supervisor.
//! * [`cells`] — the notebook/cell model and its codecs.
//! * [`platform`] — Sage discovery and the few OS-specific paths.
//!
//! The `sagenote` binary adds the eframe `app`/`ui` modules, the M3 `render`
//! module and the M4 inspector/native-error state; those depend on
//! `eframe`/`egui`.  Nothing in this crate (backend, cells, platform) knows
//! they exist, which keeps `sadrive` headless.

pub mod backend;
pub mod cells;
pub mod platform;
