#![doc = include_str!("../README.md")]
//!
//! ---
//!
//! The library behind the binary: a stage pipeline that shrinks PDF files.
//! `src/main.rs` and the evaluation harnesses are thin clients of it; the
//! design is described in the repository's AGENTS.md.

pub mod config;
pub mod content;
pub mod font;
pub mod pipeline;
pub mod report;
pub mod stages;
pub mod verify;
