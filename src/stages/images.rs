//! Stage 2: recompress image XObjects.
//!
//! Per image, in order:
//!   1. classify: bitonal / indexed / continuous (gray or color), or
//!      unsupported (route to "kept", note why);
//!   2. decode to a raster;
//!   3. transform: downsampling when effective DPI exceeds the class
//!      threshold (color conversion, complexity reduction and clipping are
//!      not implemented yet);
//!   4. encode with every codec allowed for the class and keep the
//!      smallest; the original bytes always compete;
//!   5. rewrite the stream and its dictionary, keeping the soft mask
//!      consistent.
//!
//! Invariant: an image never gets larger, and an image we cannot fully
//! understand is left byte-for-byte untouched.

mod bitonal;
mod classify;
mod decode;
mod encode;
mod raster;
mod transform;

use std::collections::HashSet;

use anyhow::Result;
use lopdf::{Document, Object, ObjectId, Stream};

use crate::config::{Codecs, Config, Dpi};
use crate::pipeline::{Context, Stage};
use crate::report::ImageRow;
use classify::{Class, ImageInfo};
use decode::Skip;
use encode::Encoded;
use raster::{Format, Raster};

pub struct RecompressImages;

impl Stage for RecompressImages {
    fn name(&self) -> &'static str {
        "images"
    }

    fn enabled(&self, config: &Config) -> bool {
        !(config.bitonal.is_empty() && config.continuous.is_empty() && config.indexed.is_empty())
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        let masks = mask_ids(doc);
        let ids: Vec<ObjectId> = image_ids(doc)
            .into_iter()
            .filter(|id| !masks.contains(id))
            .collect();
        for id in ids {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process_image(doc, id, ctx)
            }));
            if outcome.is_err() {
                ctx.report
                    .note(format!("image {} {}: kept: decoder panic", id.0, id.1));
            }
        }
        Ok(())
    }
}

fn image_ids(doc: &Document) -> Vec<ObjectId> {
    doc.objects
        .iter()
        .filter(|(_, o)| o.as_stream().is_ok_and(|s| is_image(&s.dict)))
        .map(|(&id, _)| id)
        .collect()
}

fn is_image(dict: &lopdf::Dictionary) -> bool {
    dict.get(b"Subtype")
        .and_then(Object::as_name)
        .is_ok_and(|s| s == b"Image")
}

/// Soft masks and stencil masks attached to other images: handled with
/// their parent, never on their own.
fn mask_ids(doc: &Document) -> HashSet<ObjectId> {
    doc.objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter(|s| is_image(&s.dict))
        .flat_map(|s| [s.dict.get(b"SMask"), s.dict.get(b"Mask")])
        .filter_map(|m| m.ok().and_then(|m| m.as_reference().ok()))
        .collect()
}

// --------------------------------------------------------------- one image

/// What happened to one image, for the report row.
struct Outcome {
    action: String,
    codec: Option<&'static str>,
    bytes_out: Option<usize>,
}

impl Outcome {
    fn kept(reason: impl Into<String>) -> Outcome {
        Outcome {
            action: format!("kept: {}", reason.into()),
            codec: None,
            bytes_out: None,
        }
    }
}

fn process_image(doc: &mut Document, id: ObjectId, ctx: &mut Context<'_>) {
    let Ok(Object::Stream(stream)) = doc.get_object(id) else {
        return;
    };
    let stream = stream.clone();
    let mut row = row_for(id, &stream);
    row.effective_dpi = ctx.usage.by_object.get(&id).and_then(|u| u.min_dpi());
    let outcome = match classify::read_info(doc, &stream.dict) {
        None => Outcome::kept("unreadable dictionary"),
        Some(info) => {
            let codecs = class_codecs(ctx.config, info.class());
            if codecs.is_empty() {
                Outcome::kept("class excluded")
            } else {
                let task = Task {
                    id,
                    stream: &stream,
                    info,
                    config: ctx.config,
                    dpi: row.effective_dpi,
                };
                attempt(doc, task)
            }
        }
    };
    row.action = outcome.action;
    row.filter_out = outcome.codec.map_or(row.filter_in.clone(), String::from);
    row.bytes_out = outcome.bytes_out.unwrap_or(row.bytes_in);
    ctx.report.images.push(row);
}

/// Everything `attempt` needs about one image.
struct Task<'a> {
    id: ObjectId,
    stream: &'a Stream,
    info: ImageInfo,
    config: &'a Config,
    dpi: Option<f32>,
}

fn attempt(doc: &mut Document, task: Task<'_>) -> Outcome {
    let Task {
        id,
        stream,
        info,
        config,
        dpi,
    } = task;
    let raster = match decode::decode(doc, stream, &info) {
        Ok(r) => r,
        Err(Skip(reason)) => return Outcome::kept(reason),
    };
    let target = downsample_target(&raster, &info, config, dpi);
    let (raster, resized) = match target.and_then(|(w, h)| transform::downsample(&raster, w, h)) {
        Some(small) => (small, true),
        None => (raster, false),
    };
    let codecs = class_codecs(config, info.class());
    let lossy_ok = !info.has_color_key_mask;
    let mut candidates = candidates(&raster, codecs, lossy_ok, config.jpeg_quality);
    if !resized && info.image_codec() == Some("DCTDecode") {
        // The stored JPEG without any wrapper filters: lossless and often
        // smaller than the Flate-wrapped original.
        candidates.extend(passthrough(stream, &info));
    }
    let Some(best) = candidates.into_iter().min_by_key(|e| e.bytes.len()) else {
        return Outcome::kept("no encoder for this class yet");
    };
    if best.bytes.len() >= stream.content.len() {
        return Outcome::kept("source is smaller");
    }
    if resized && !resize_soft_mask(doc, stream, raster.width, raster.height) {
        return Outcome::kept("soft mask could not be resized");
    }
    write_back(doc, id, &raster, &best);
    Outcome {
        action: if resized { "downsampled" } else { "recoded" }.into(),
        codec: Some(best.codec),
        bytes_out: Some(best.bytes.len()),
    }
}

/// Target size when the class rule says to downsample. Images with a
/// color-key mask are never resampled (the key values would change).
fn downsample_target(
    raster: &Raster,
    info: &ImageInfo,
    config: &Config,
    dpi: Option<f32>,
) -> Option<(u32, u32)> {
    let rule = class_dpi(config, info.class());
    let dpi = dpi?;
    if !rule.enabled() || dpi <= rule.threshold || info.has_color_key_mask {
        return None;
    }
    Some(transform::scaled_size(
        raster.width,
        raster.height,
        rule.target / dpi,
    ))
}

fn candidates(raster: &Raster, codecs: Codecs, lossy_ok: bool, quality: u8) -> Vec<Encoded> {
    let mut out = Vec::new();
    if codecs.contains(Codecs::FLATE) {
        out.extend(encode::flate(raster));
    }
    if codecs.contains(Codecs::G4) {
        out.extend(bitonal::encode_g4(raster));
    }
    if codecs.contains(Codecs::JBIG2) {
        out.extend(bitonal::encode_jbig2(raster));
    }
    if codecs.contains(Codecs::JPEG)
        && lossy_ok
        && matches!(raster.format, Format::Gray8 | Format::Rgb8)
    {
        out.extend(encode::jpeg(raster, quality));
    }
    out
}

fn passthrough(stream: &Stream, info: &ImageInfo) -> Option<Encoded> {
    let bytes = decode::codestream(stream, info).ok()?;
    Some(Encoded {
        codec: "DCTDecode",
        bytes,
        filter: Some(Object::Name(b"DCTDecode".to_vec())),
        decode_parms: None,
    })
}

fn row_for(id: ObjectId, stream: &Stream) -> ImageRow {
    let d = &stream.dict;
    let int = |k: &[u8]| d.get(k).and_then(Object::as_i64).unwrap_or(0);
    ImageRow {
        object: id,
        width: int(b"Width").max(0) as u32,
        height: int(b"Height").max(0) as u32,
        bits_per_component: int(b"BitsPerComponent").clamp(0, 16) as u8,
        color_space: describe(d.get(b"ColorSpace").ok()),
        filter_in: describe(d.get(b"Filter").ok()),
        effective_dpi: None,
        bytes_in: stream.content.len(),
        action: String::new(),
        filter_out: String::new(),
        bytes_out: stream.content.len(),
    }
}

fn describe(obj: Option<&Object>) -> String {
    match obj {
        Some(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
        Some(Object::Array(a)) => a
            .first()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_else(|| "array".into()),
        Some(Object::Reference(_)) => "ref".into(),
        Some(_) => "?".into(),
        None => "none".into(),
    }
}

fn class_codecs(config: &Config, class: Class) -> Codecs {
    match class {
        Class::Bitonal => config.bitonal,
        Class::Indexed => config.indexed,
        Class::Gray | Class::Color => config.continuous,
    }
}

fn class_dpi(config: &Config, class: Class) -> Dpi {
    match class {
        Class::Bitonal => config.bitonal_dpi,
        Class::Gray | Class::Indexed => config.gray_dpi,
        Class::Color => config.color_dpi,
    }
}

fn write_back(doc: &mut Document, id: ObjectId, raster: &Raster, best: &Encoded) {
    if let Ok(Object::Stream(s)) = doc.get_object_mut(id) {
        encode::apply(s, raster, best);
    }
}

/// A parent that shrinks must shrink its `/SMask` to the same size. Returns
/// false when a mask exists but cannot be handled, in which case the parent
/// is kept. Stencil `/Mask` references are not resampled yet, so parents
/// that carry one are kept as well.
fn resize_soft_mask(doc: &mut Document, parent: &Stream, width: u32, height: u32) -> bool {
    if matches!(parent.dict.get(b"Mask"), Ok(Object::Reference(_))) {
        return false;
    }
    let Ok(Object::Reference(mask_id)) = parent.dict.get(b"SMask") else {
        return true;
    };
    let mask_id = *mask_id;
    let Ok(Object::Stream(mask)) = doc.get_object(mask_id) else {
        return false;
    };
    let mask = mask.clone();
    let small = classify::read_info(doc, &mask.dict)
        .and_then(|info| decode::decode(doc, &mask, &info).ok())
        .and_then(|raster| transform::downsample(&raster, width, height));
    let Some(small) = small else {
        return false;
    };
    let Some(enc) = encode::flate(&small) else {
        return false;
    };
    write_back(doc, mask_id, &small, &enc);
    true
}
