//! Font programs, independent of the pipeline: recognizing the standard
//! 14, and (to come) parsing and writing CFF, TrueType and Type 1 so the
//! font stage can convert, merge and subset them. Nothing here reads
//! `Config` or touches a `Document` beyond resolving references.

pub mod std14;
