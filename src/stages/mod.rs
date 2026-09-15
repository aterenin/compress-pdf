//! One module per pipeline stage. Each exposes a unit struct implementing
//! [`crate::pipeline::Stage`] and keeps its helpers private.

pub mod fonts;
pub mod images;
pub mod strip;
pub mod structure;
pub mod usage;
