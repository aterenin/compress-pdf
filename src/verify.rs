//! Output verification, structural level (AGENTS.md, "Output verification").
//!
//! Re-reads the serialized output with `hayro-syntax`, a parser that shares
//! no code with lopdf, and checks that the file is usable: it loads, it has
//! the expected page count, every page's content stream decodes, and every
//! stream (images included) decodes with its declared filters. A problem
//! here is a bug in the pipeline, never a warning: callers must not write
//! the file.
//!
//! The visual level (render both documents and compare pages with SSIM)
//! lives in [`render`].

pub mod render;

use std::fmt;

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::ImageDecodeParams;
use hayro_syntax::object::{Array, Dict, Name};

/// Problems are capped so a badly broken file does not flood the report.
const MAX_PROBLEMS: usize = 50;

#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Verification {
    pub pages: usize,
    pub objects: usize,
    pub streams_checked: usize,
    pub problems: Vec<String>,
}

impl Verification {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }

    fn problem(&mut self, msg: String) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(msg);
        }
    }
}

impl fmt::Display for Verification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "verify: {} pages, {} objects, {} streams decoded, {} problems",
            self.pages,
            self.objects,
            self.streams_checked,
            self.problems.len()
        )?;
        for p in &self.problems {
            write!(f, "\n  - {p}")?;
        }
        Ok(())
    }
}

/// Problem categories, for comparing an output against its input. Object
/// numbers change between the two, so comparison is by category count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Category {
    Parse,
    PageCount,
    ContentStream,
    Stream,
    Image,
}

impl Verification {
    /// Problems in `self` that go beyond what `baseline` (normally the
    /// verification of the input) already had, per category:
    /// `(category, count here, count in baseline)`.
    pub fn regressions_from(&self, baseline: &Verification) -> Vec<(Category, usize, usize)> {
        let theirs = baseline.category_counts();
        self.category_counts()
            .into_iter()
            .filter_map(|(cat, n)| {
                let base = theirs
                    .iter()
                    .find(|(c, _)| *c == cat)
                    .map_or(0, |(_, b)| *b);
                (n > base).then_some((cat, n, base))
            })
            .collect()
    }

    fn category_counts(&self) -> Vec<(Category, usize)> {
        let mut counts: Vec<(Category, usize)> = Vec::new();
        for p in &self.problems {
            let cat = categorize(p);
            match counts.iter_mut().find(|(c, _)| *c == cat) {
                Some(entry) => entry.1 += 1,
                None => counts.push((cat, 1)),
            }
        }
        counts
    }
}

fn categorize(problem: &str) -> Category {
    if problem.contains("does not parse") {
        Category::Parse
    } else if problem.contains("page count") {
        Category::PageCount
    } else if problem.contains("content stream") {
        Category::ContentStream
    } else if problem.contains("(ImageDecode)") {
        Category::Image
    } else {
        Category::Stream
    }
}

/// Check `output` as an independent reader would see it. `expected_pages`
/// is the input's page count.
pub fn verify(output: &[u8], expected_pages: usize) -> Verification {
    let mut v = Verification::default();
    let pdf = match Pdf::new(output.to_vec()) {
        Ok(pdf) => pdf,
        Err(e) => {
            v.problem(format!("output does not parse: {e:?}"));
            return v;
        }
    };
    check_pages(&pdf, expected_pages, &mut v);
    check_streams(&pdf, &mut v);
    v
}

fn check_pages(pdf: &Pdf, expected: usize, v: &mut Verification) {
    let pages = pdf.pages();
    v.pages = pages.len();
    if pages.len() != expected {
        v.problem(format!(
            "page count is {} but the input had {expected}",
            pages.len()
        ));
    }
    for (i, page) in pages.iter().enumerate() {
        if page.raw().contains_key(b"Contents") && page.page_stream().is_none() {
            v.problem(format!("page {}: content stream does not decode", i + 1));
        }
    }
}

fn check_streams(pdf: &Pdf, v: &mut Verification) {
    for object in pdf.objects() {
        v.objects += 1;
        let Some(stream) = object.into_stream() else {
            continue;
        };
        let dict = stream.dict();
        if name_is(dict, b"Type", "XRef") {
            continue;
        }
        let result = if name_is(dict, b"Subtype", "Image") {
            stream.decoded_image(&image_params(dict)).map(|_| ())
        } else {
            stream.decoded().map(|_| ())
        };
        v.streams_checked += 1;
        if let Err(e) = result {
            v.problem(format!(
                "object {:?}: stream does not decode ({e:?})",
                stream.obj_id()
            ));
        }
    }
}

fn name_is(dict: &Dict<'_>, key: &[u8], value: &str) -> bool {
    dict.get::<Name<'_>>(key)
        .is_some_and(|n| n.as_str() == value)
}

/// What the image decoders need to know up front; everything else they read
/// from the codestream.
fn image_params(dict: &Dict<'_>) -> ImageDecodeParams {
    let cs = color_space_family(dict);
    let num_components = match cs.as_deref() {
        Some("DeviceGray" | "CalGray") => Some(1),
        Some("DeviceRGB" | "CalRGB" | "Lab") => Some(3),
        Some("DeviceCMYK") => Some(4),
        _ => None,
    };
    ImageDecodeParams {
        is_indexed: cs.as_deref() == Some("Indexed"),
        bpc: dict.get::<u8>(b"BitsPerComponent"),
        num_components,
        width: dict.get::<u32>(b"Width").unwrap_or(0),
        height: dict.get::<u32>(b"Height").unwrap_or(0),
        ..ImageDecodeParams::default()
    }
}

/// The color space's family name: the name itself, or the first element of
/// an array color space such as `[/Indexed ...]` or `[/ICCBased ...]`.
fn color_space_family(dict: &Dict<'_>) -> Option<String> {
    if let Some(name) = dict.get::<Name<'_>>(b"ColorSpace") {
        return Some(name.as_str().to_owned());
    }
    let array = dict.get::<Array<'_>>(b"ColorSpace")?;
    let first = array.iter::<Name<'_>>().next()?;
    Some(first.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use lopdf::dictionary;

    use super::*;

    #[test]
    fn garbage_does_not_verify() {
        let v = verify(b"not a pdf at all", 1);
        assert!(!v.is_ok());
        assert!(v.problems[0].contains("does not parse"), "{v}");
    }

    #[test]
    fn minimal_document_verifies() {
        let mut doc = lopdf::Document::with_version("1.5");
        let content = doc.add_object(lopdf::Stream::new(
            dictionary! {},
            b"0 0 m 10 10 l S".to_vec(),
        ));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Contents" => content,
        });
        doc.objects.insert(
            pages_id,
            lopdf::Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page.into()],
                "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let v = verify(&bytes, 1);
        assert!(v.is_ok(), "{v}");
        assert_eq!(v.pages, 1);
        assert!(!verify(&bytes, 2).is_ok());
    }
}
