//! compress-pdf: a stage pipeline that shrinks PDF files.
//!
//! The binary (`src/main.rs`) and the evaluation harnesses are thin clients
//! of this crate. See CLAUDE.md for the design.

pub mod config;
pub mod content;
pub mod font;
pub mod pipeline;
pub mod report;
pub mod stages;
pub mod verify;
