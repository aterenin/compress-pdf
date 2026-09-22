#![doc = include_str!("../README.md")]
//!
//! ---
//!
//! The library behind the binary. [`compress::compress`] does what the
//! command line does for one document; the modules below are the pieces it
//! is built from. `src/main.rs` and the evaluation harnesses are thin
//! clients; the design is described in the repository's AGENTS.md.

/// The design notes: what each stage does, the presets, the verification
/// levels, the evals, and the decisions behind them. This page is the
/// repository's `AGENTS.md`, included as is, so the text lives in one place.
#[doc = include_str!("../AGENTS.md")]
pub mod design {}

pub mod compress;
pub mod config;
pub mod error;
pub mod pipeline;
pub mod report;
pub mod verify;

// Implementation: the stages, the font-program machinery and the content
// lexer are reachable only through the modules above.
mod content;
mod font;
mod stages;
