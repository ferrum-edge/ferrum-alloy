//! Versioned, language-neutral diagnostic evidence for Ferrum Alloy.
//!
//! This crate has no HTTP or async dependencies so that offline tools (the
//! `ferrum-alloy diagnose` command, CI checks, other products) can read and
//! explain reports without the service stack.
//!
//! * [`model`] defines schema `ferrum.diagnostic_report` v1.
//! * [`parse`] reads untrusted files with explicit bounds and downgrades
//!   unverifiable provenance claims.
//! * [`rules`] derives findings deterministically; no network, no AI service.
//! * [`otlp`] converts OTLP/JSON trace exports into observations.
//! * [`render`] produces deterministic human-readable output.

mod open_enum;

pub mod catalog;
pub mod edge_record;
pub mod model;
pub mod otlp;
pub mod parse;
pub mod render;
pub mod rules;

pub use model::{DiagnosticReport, Finding, Observation};
pub use parse::{Limits, ParsedReport, ReportError, parse_offline};
pub use rules::{Thresholds, analyze};
