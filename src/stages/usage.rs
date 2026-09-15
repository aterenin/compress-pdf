//! Stage 1: find every placement of every image and compute its effective
//! resolution.
//!
//! Walks each page's content stream tracking the current transformation
//! matrix through `q`/`Q`/`cm`, descends into form XObjects with their
//! `/Matrix`, and at every `Do` of an image records the rendered size in
//! points. Effective DPI = pixel width / (rendered width / 72). An image used
//! in several places keeps the *minimum*, since that is the placement that
//! would suffer first from downsampling.
//!
//! Nothing here mutates the document.

use std::collections::HashMap;

use anyhow::Result;
use lopdf::{Document, ObjectId};

use crate::config::Config;
use crate::pipeline::{Context, Stage};

// The data model below is populated by this stage and read by the image
// stage. Until the image stage lands nothing outside the tests reads it, so
// the dead-code lint is silenced here rather than at crate level. Remove
// these attributes when `stages::images` starts consuming `ImageUsage`.

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    /// Rendered width and height in points.
    pub width_pt: f32,
    pub height_pt: f32,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub placements: Vec<Placement>,
    /// Pixel dimensions, copied here so consumers do not re-read the dictionary.
    pub pixels: (u32, u32),
}

impl Usage {
    /// Minimum effective resolution over all placements, on the tighter axis.
    #[allow(dead_code)]
    pub fn min_dpi(&self) -> Option<f32> {
        let (w, h) = self.pixels;
        self.placements
            .iter()
            .map(|p| {
                let dx = w as f32 / (p.width_pt / 72.0);
                let dy = h as f32 / (p.height_pt / 72.0);
                dx.min(dy)
            })
            .fold(None, |acc, d| Some(acc.map_or(d, |a: f32| a.min(d))))
    }
}

#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct ImageUsage {
    pub by_object: HashMap<ObjectId, Usage>,
}

pub struct AnalyzeUsage;

impl Stage for AnalyzeUsage {
    fn name(&self) -> &'static str {
        "usage"
    }

    fn enabled(&self, config: &Config) -> bool {
        // Only needed when some image class can be downsampled.
        config.bitonal_dpi.enabled() || config.gray_dpi.enabled() || config.color_dpi.enabled()
    }

    fn run(&self, _doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        // TODO(stage: usage): content-stream walk. Until then every image
        // reports an unknown DPI and the image stage must treat it as
        // "below threshold" (never downsample blind).
        ctx.report
            .note("usage analysis not implemented; downsampling disabled");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_dpi_takes_the_largest_placement() {
        // A 600x300 pixel image drawn once at 2x1 inches (300 dpi) and once
        // stretched to 4x2 inches (150 dpi). The stretched placement wins.
        let mut usage = ImageUsage::default();
        usage.by_object.insert(
            (7, 0),
            Usage {
                pixels: (600, 300),
                placements: vec![
                    Placement {
                        width_pt: 144.0,
                        height_pt: 72.0,
                    },
                    Placement {
                        width_pt: 288.0,
                        height_pt: 144.0,
                    },
                ],
            },
        );
        let dpi = usage.by_object[&(7, 0)].min_dpi().unwrap();
        assert!((dpi - 150.0).abs() < 1e-3, "got {dpi}");
    }

    #[test]
    fn min_dpi_uses_the_tighter_axis() {
        // Non-uniform scale: 100x100 px drawn at 1in x 2in -> 100 dpi x 50 dpi.
        let u = Usage {
            pixels: (100, 100),
            placements: vec![Placement {
                width_pt: 72.0,
                height_pt: 144.0,
            }],
        };
        assert!((u.min_dpi().unwrap() - 50.0).abs() < 1e-3);
    }

    #[test]
    fn no_placements_means_unknown() {
        assert!(Usage::default().min_dpi().is_none());
    }
}
