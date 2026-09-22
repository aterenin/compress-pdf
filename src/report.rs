//! What the pipeline did, in a form a human can check against the reference
//! tool. Every stage appends to this; nothing else prints.

use std::fmt;
use std::time::Duration;

use lopdf::ObjectId;

/// One row per image XObject the image stage looked at.
#[derive(Debug, Clone)]
pub struct ImageRow {
    pub object: ObjectId,
    pub width: u32,
    pub height: u32,
    pub bits_per_component: u8,
    pub color_space: String,
    pub filter_in: String,
    /// Minimum effective resolution across all placements, if known.
    pub effective_dpi: Option<f32>,
    pub bytes_in: usize,
    /// Short verb: "kept", "downsampled", "recoded", "gray", "skipped: `<why>`".
    pub action: String,
    pub filter_out: String,
    pub bytes_out: usize,
}

/// One row per embedded font program the font stage looked at.
#[derive(Debug, Clone)]
pub struct FontRow {
    pub object: ObjectId,
    pub name: String,
    /// Program kind: Type1, TrueType, CFF, CIDFontType0C, OpenType.
    pub program: String,
    pub bytes_in: usize,
    /// Short verb: "kept", "unembedded", "subset", "merged", "cff", "kept: `<why>`".
    pub action: String,
    pub bytes_out: usize,
}

#[derive(Debug, Clone)]
pub struct StageSummary {
    pub name: &'static str,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
pub struct Report {
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub images: Vec<ImageRow>,
    pub fonts: Vec<FontRow>,
    pub stages: Vec<StageSummary>,
    /// Free-form observations (unsupported features hit, fallbacks taken).
    pub notes: Vec<String>,
}

impl Report {
    pub fn new(input_bytes: usize) -> Report {
        Report {
            input_bytes,
            ..Report::default()
        }
    }

    pub fn note(&mut self, msg: impl Into<String>) {
        self.notes.push(msg.into());
    }
}

fn human(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn pct(before: usize, after: usize) -> String {
    if before == 0 {
        return "-".into();
    }
    format!(
        "{:+.1}%",
        (after as f64 - before as f64) / before as f64 * 100.0
    )
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.images.is_empty() {
            writeln!(
                f,
                "{:<9} {:>11} {:>4} {:<12} {:<10} {:>7} {:>10} {:<14} {:<10} {:>10}",
                "object",
                "size",
                "bpc",
                "colorspace",
                "filter",
                "dpi",
                "bytes in",
                "action",
                "filter out",
                "bytes out"
            )?;
            for r in &self.images {
                writeln!(
                    f,
                    "{:<9} {:>11} {:>4} {:<12} {:<10} {:>7} {:>10} {:<14} {:<10} {:>10}",
                    format!("{} {}", r.object.0, r.object.1),
                    format!("{}x{}", r.width, r.height),
                    r.bits_per_component,
                    r.color_space,
                    r.filter_in,
                    r.effective_dpi
                        .map(|d| format!("{d:.0}"))
                        .unwrap_or_else(|| "?".into()),
                    r.bytes_in,
                    r.action,
                    r.filter_out,
                    r.bytes_out,
                )?;
            }
            writeln!(f)?;
        }

        if !self.fonts.is_empty() {
            writeln!(
                f,
                "{:<9} {:<32} {:<14} {:>10} {:<22} {:>10}",
                "object", "font", "program", "bytes in", "action", "bytes out"
            )?;
            for r in &self.fonts {
                writeln!(
                    f,
                    "{:<9} {:<32} {:<14} {:>10} {:<22} {:>10}",
                    format!("{} {}", r.object.0, r.object.1),
                    r.name,
                    r.program,
                    r.bytes_in,
                    r.action,
                    r.bytes_out,
                )?;
            }
            writeln!(f)?;
        }

        if !self.stages.is_empty() {
            writeln!(
                f,
                "{:<12} {:>12} {:>12} {:>8} {:>8}",
                "stage", "before", "after", "delta", "time"
            )?;
            for s in &self.stages {
                writeln!(
                    f,
                    "{:<12} {:>12} {:>12} {:>8} {:>7.0}ms",
                    s.name,
                    human(s.bytes_before),
                    human(s.bytes_after),
                    pct(s.bytes_before, s.bytes_after),
                    s.elapsed.as_secs_f64() * 1000.0,
                )?;
            }
            writeln!(f)?;
        }

        for n in &self.notes {
            writeln!(f, "note: {n}")?;
        }

        writeln!(
            f,
            "total: {} -> {} ({})",
            human(self.input_bytes),
            human(self.output_bytes),
            pct(self.input_bytes, self.output_bytes)
        )
    }
}
