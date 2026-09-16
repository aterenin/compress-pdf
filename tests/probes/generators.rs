//! Synthetic one-variable PDFs. Each generator builds a small document
//! that exercises exactly one behavior the design promises, so the probe
//! tests can assert it and `cargo evals probes` can hand the same files to
//! a reference tool. Shared by `tests/probes.rs` and `examples/evals.rs`
//! through `#[path]`.

use std::io::Write;

use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

pub struct Probe {
    pub name: &'static str,
    pub about: &'static str,
    pub doc: Document,
}

pub fn all() -> Vec<Probe> {
    vec![
        Probe {
            name: "cmyk-patch-300dpi",
            about: "an 8-bit CMYK image placed at 300 dpi",
            doc: cmyk_patch(),
        },
        Probe {
            name: "rgb-gray-300dpi",
            about: "an RGB image whose channels are equal, placed at 300 dpi",
            doc: rgb_gray(),
        },
        Probe {
            name: "bitonal-lines-600dpi",
            about: "a 1-bit line pattern placed at 600 dpi",
            doc: bitonal_lines(),
        },
        Probe {
            name: "indexed-4color-300dpi",
            about: "a 2-bit indexed image placed at 300 dpi",
            doc: indexed_patch(),
        },
        Probe {
            name: "jpeg-photo-72dpi",
            about: "a JPEG image placed at 72 dpi that needs no transform",
            doc: jpeg_photo(),
        },
        Probe {
            name: "clipped-image",
            about: "an image whose right half is outside the clip",
            doc: clipped_image(),
        },
        Probe {
            name: "smask-opaque",
            about: "an image with a soft mask that is fully opaque",
            doc: smask_opaque(),
        },
        Probe {
            name: "lab-image",
            about: "an 8-bit image in a Lab color space",
            doc: lab_image(),
        },
        Probe {
            name: "duplicate-images",
            about: "two byte-identical image objects drawn on one page",
            doc: duplicate_images(),
        },
        Probe {
            name: "unused-resources",
            about: "page resources naming an image and a font the content never uses",
            doc: unused_resources(),
        },
        Probe {
            name: "type1-partly-used",
            about: "an embedded Type 1 font with five glyphs of which two are shown",
            doc: type1_partly_used(),
        },
        Probe {
            name: "arial-embedded",
            about: "an embedded program for a font named Arial with WinAnsiEncoding",
            doc: arial_embedded(),
        },
        Probe {
            name: "metadata-thumbnail",
            about: "a document with an XMP metadata stream and a page thumbnail",
            doc: metadata_thumbnail(),
        },
        Probe {
            name: "form-default-fonts",
            about: "an AcroForm whose default resources hold a font no field's appearance names",
            doc: form_default_fonts(false),
        },
        Probe {
            name: "xfa-default-fonts",
            about: "the same form with an XFA entry, whose fonts an XFA engine may use",
            doc: form_default_fonts(true),
        },
    ]
}

// ---------------------------------------------------------------- pages

/// The width in points at which `pixels` render at `dpi`.
fn points(pixels: u32, dpi: f32) -> f32 {
    pixels as f32 * 72.0 / dpi
}

/// Add the page tree and catalog: one page (300 x 300 pt) drawing
/// `content` with the given resources. Returns the page id.
fn one_page(doc: &mut Document, content: &[u8], resources: Dictionary) -> ObjectId {
    let contents = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => contents,
        "MediaBox" => vec![0.into(), 0.into(), 300.into(), 300.into()],
        "Resources" => resources,
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    page
}

/// A page drawing one image object at `dpi` in the page's lower left.
fn image_page(doc: &mut Document, image: ObjectId, (width, height): (u32, u32), dpi: f32) {
    let (w, h) = (points(width, dpi), points(height, dpi));
    let content = format!("q {w} 0 0 {h} 10 10 cm /Im1 Do Q");
    one_page(
        doc,
        content.as_bytes(),
        dictionary! { "XObject" => dictionary! { "Im1" => image } },
    );
}

fn image(doc: &mut Document, dict: Dictionary, samples: Vec<u8>) -> ObjectId {
    let mut dict = dict;
    dict.set("Type", "XObject");
    dict.set("Subtype", "Image");
    let mut stream = Stream::new(dict, samples);
    stream.compress().ok();
    doc.add_object(stream)
}

fn gradient(
    width: u32,
    height: u32,
    channels: usize,
    f: impl Fn(u32, u32, usize) -> u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height) as usize * channels);
    for y in 0..height {
        for x in 0..width {
            for c in 0..channels {
                out.push(f(x, y, c));
            }
        }
    }
    out
}

// --------------------------------------------------------------- images

pub fn cmyk_patch() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (300, 200);
    let samples = gradient(w, h, 4, |x, y, c| match c {
        0 => (x * 255 / w) as u8,
        1 => (y * 255 / h) as u8,
        2 => 40,
        _ => 20,
    });
    let img = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => "DeviceCMYK", "BitsPerComponent" => 8 },
        samples,
    );
    image_page(&mut doc, img, (w, h), 300.0);
    doc
}

pub fn rgb_gray() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (300, 200);
    let samples = gradient(w, h, 3, |x, y, _| ((x + y) * 255 / (w + h)) as u8);
    let img = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8 },
        samples,
    );
    image_page(&mut doc, img, (w, h), 300.0);
    doc
}

pub fn bitonal_lines() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (1200u32, 600u32);
    let stride = (w as usize).div_ceil(8);
    let mut samples = vec![0xFFu8; stride * h as usize];
    // Scan-like content: a few hundred random strokes six pixels wide,
    // which survive downsampling the way text does but do not compress
    // into almost nothing the way a regular pattern would.
    let mut seed = 0x2545_F491u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let set = |x: i64, y: i64, samples: &mut [u8]| {
        if (0..w as i64).contains(&x) && (0..h as i64).contains(&y) {
            samples[y as usize * stride + x as usize / 8] &= !(0x80 >> (x % 8));
        }
    };
    for _ in 0..400 {
        let (x0, y0) = ((next() % w) as i64, (next() % h) as i64);
        let (dx, dy) = ((next() % 121) as i64 - 60, (next() % 61) as i64 - 30);
        let steps = dx.abs().max(dy.abs()).max(1);
        for t in 0..=steps {
            let (x, y) = (x0 + dx * t / steps, y0 + dy * t / steps);
            for oy in 0..6 {
                for ox in 0..6 {
                    set(x + ox, y + oy, &mut samples);
                }
            }
        }
    }
    let img = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 1 },
        samples,
    );
    image_page(&mut doc, img, (w, h), 600.0);
    doc
}

pub fn indexed_patch() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (400u32, 300u32);
    let stride = (w as usize * 2).div_ceil(8);
    let mut samples = vec![0u8; stride * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let index = ((x / 50 + y / 50) % 4) as u8;
            samples[y * stride + x / 4] |= index << (6 - 2 * (x % 4));
        }
    }
    let palette = Object::string_literal(vec![255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]);
    let img = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "BitsPerComponent" => 2,
        "ColorSpace" => vec!["Indexed".into(), "DeviceRGB".into(), 3.into(), palette] },
        samples,
    );
    image_page(&mut doc, img, (w, h), 300.0);
    doc
}

pub fn jpeg_photo() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (256u32, 192u32);
    let rgb = gradient(w, h, 3, |x, y, c| {
        let noise = ((x * 31 + y * 17) % 23) as u8;
        match c {
            0 => (x * 255 / w) as u8 ^ noise,
            1 => (y * 255 / h) as u8,
            _ => 128u8.wrapping_add(noise * 3),
        }
    });
    let jpeg = encode_jpeg(&rgb, w, h);
    let stream = Stream::new(
        dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => w, "Height" => h,
        "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8, "Filter" => "DCTDecode" },
        jpeg,
    );
    let img = doc.add_object(stream);
    image_page(&mut doc, img, (w, h), 72.0);
    doc
}

fn encode_jpeg(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    comp.set_size(width as usize, height as usize);
    comp.set_quality(90.0);
    let mut comp = comp.start_compress(Vec::new()).expect("mozjpeg start");
    comp.write_scanlines(rgb).expect("mozjpeg scanlines");
    comp.finish().expect("mozjpeg finish")
}

pub fn clipped_image() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (400u32, 400u32);
    let samples = gradient(w, h, 1, |x, y, _| ((x ^ y) & 0xFF) as u8);
    let img = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
        samples,
    );
    // Drawn 200 pt wide, clipped to its left half.
    one_page(
        &mut doc,
        b"q 10 10 100 200 re W n 200 0 0 200 10 10 cm /Im1 Do Q",
        dictionary! { "XObject" => dictionary! { "Im1" => img } },
    );
    doc
}

pub fn smask_opaque() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (200u32, 150u32);
    let mask = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
        vec![255u8; (w * h) as usize],
    );
    let samples = gradient(w, h, 3, |x, y, c| ((x * (c as u32 + 1) + y) % 256) as u8);
    let img = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
        "SMask" => mask },
        samples,
    );
    image_page(&mut doc, img, (w, h), 100.0);
    doc
}

pub fn lab_image() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (200u32, 100u32);
    // L rising left to right, a and b centered.
    let samples = gradient(w, h, 3, |x, _, c| match c {
        0 => (x * 255 / w) as u8,
        _ => 128,
    });
    let lab: Object = vec![
        "Lab".into(),
        dictionary! { "WhitePoint" => vec![0.9505.into(), 1.0.into(), 1.089.into()],
        "Range" => vec![(-100).into(), 100.into(), (-100).into(), 100.into()] }
        .into(),
    ]
    .into();
    let img = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => lab, "BitsPerComponent" => 8 },
        samples,
    );
    image_page(&mut doc, img, (w, h), 100.0);
    doc
}

pub fn duplicate_images() -> Document {
    let mut doc = Document::with_version("1.5");
    let (w, h) = (100u32, 100u32);
    let samples = gradient(w, h, 1, |x, y, _| ((x + y) % 256) as u8);
    let a = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
        samples.clone(),
    );
    let b = image(
        &mut doc,
        dictionary! { "Width" => w, "Height" => h, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
        samples,
    );
    one_page(
        &mut doc,
        b"q 72 0 0 72 10 10 cm /ImA Do Q q 72 0 0 72 150 10 cm /ImB Do Q",
        dictionary! { "XObject" => dictionary! { "ImA" => a, "ImB" => b } },
    );
    doc
}

pub fn unused_resources() -> Document {
    let mut doc = Document::with_version("1.5");
    let unused = image(
        &mut doc,
        dictionary! { "Width" => 8, "Height" => 8, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
        vec![0u8; 64],
    );
    let used = image(
        &mut doc,
        dictionary! { "Width" => 8, "Height" => 8, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
        vec![128u8; 64],
    );
    let font = doc.add_object(
        dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
    );
    one_page(
        &mut doc,
        b"q 72 0 0 72 10 10 cm /Used Do Q",
        dictionary! { "XObject" => dictionary! { "Used" => used, "Unused" => unused },
        "Font" => dictionary! { "F1" => font } },
    );
    doc
}

// ---------------------------------------------------------------- fonts

/// A Type 1 program with box-shaped glyphs named A to E, encoded at codes
/// 65 to 69, eexec-encrypted in binary form.
pub fn type1_program() -> Vec<u8> {
    let glyph_names = ["A", "B", "C", "D", "E"];
    let mut private = Vec::new();
    private.extend_from_slice(
        b"dup /Private 8 dict dup begin\n/RD {string currentfile exch readstring pop} executeonly def\n/ND {noaccess def} executeonly def\n/NP {noaccess put} executeonly def\n/BlueValues [ -10 0 700 710 ] def\n/lenIV 4 def\n/Subrs 0 array\nND\nend\n",
    );
    private.extend_from_slice(
        format!("/CharStrings {} dict dup begin\n", glyph_names.len() + 1).as_bytes(),
    );
    let mut glyphs: Vec<(&str, Vec<u8>)> = vec![(
        ".notdef",
        t1_charstring(&[(0, 'n'), (500, 'n'), (13, 'o'), (14, 'o')]),
    )];
    for (i, name) in glyph_names.iter().enumerate() {
        let size = 300 + 60 * i as i32;
        // sbx wx hsbw; 50 50 rmoveto; size hlineto; size vlineto; -size hlineto; closepath; endchar
        glyphs.push((
            name,
            t1_charstring(&[
                (50, 'n'),
                (600, 'n'),
                (13, 'o'),
                (50, 'n'),
                (50, 'n'),
                (21, 'o'),
                (size, 'n'),
                (6, 'o'),
                (size, 'n'),
                (7, 'o'),
                (-size, 'n'),
                (6, 'o'),
                (9, 'o'),
                (14, 'o'),
            ]),
        ));
    }
    for (name, cs) in glyphs {
        let enc = t1_encrypt(&cs, 4330, 4);
        private.extend_from_slice(format!("/{name} {} RD ", enc.len()).as_bytes());
        private.extend_from_slice(&enc);
        private.extend_from_slice(b" ND\n");
    }
    private.extend_from_slice(b"end\nend\nmark currentfile closefile\n");
    let mut font = Vec::new();
    font.extend_from_slice(b"%!PS-AdobeFont-1.0: Boxes\n/FontName /Boxes def\n/FontType 1 def\n/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n/FontBBox {0 0 700 700} readonly def\n/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\n");
    for (i, name) in glyph_names.iter().enumerate() {
        font.extend_from_slice(format!("dup {} /{name} put\n", 65 + i).as_bytes());
    }
    font.extend_from_slice(b"readonly def\ncurrentdict end\ncurrentfile eexec\n");
    font.extend_from_slice(&t1_encrypt(&private, 55665, 4));
    font
}

/// Type 1 charstring bytes from (value, kind) pairs: `n` for a number,
/// `o` for a single-byte operator.
fn t1_charstring(items: &[(i32, char)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(v, kind) in items {
        if kind == 'o' {
            out.push(v as u8);
        } else {
            out.extend(t1_number(v));
        }
    }
    out
}

fn t1_number(v: i32) -> Vec<u8> {
    match v {
        -107..=107 => vec![(v + 139) as u8],
        108..=1131 => vec![((v - 108) >> 8) as u8 + 247, (v - 108) as u8],
        -1131..=-108 => vec![((-v - 108) >> 8) as u8 + 251, (-v - 108) as u8],
        _ => {
            let mut out = vec![255];
            out.extend(v.to_be_bytes());
            out
        }
    }
}

fn t1_encrypt(plain: &[u8], key: u16, lead: usize) -> Vec<u8> {
    let mut r = key;
    let mut out = Vec::with_capacity(plain.len() + lead);
    for &p in std::iter::repeat_n(&0u8, lead).chain(plain) {
        let c = p ^ (r >> 8) as u8;
        r = (u16::from(c).wrapping_add(r))
            .wrapping_mul(52845)
            .wrapping_add(22719);
        out.push(c);
    }
    out
}

fn text_page(doc: &mut Document, font: ObjectId, text: &str) {
    let content = format!("BT /F1 24 Tf 20 150 Td ({text}) Tj ET");
    one_page(
        doc,
        content.as_bytes(),
        dictionary! { "Font" => dictionary! { "F1" => font } },
    );
}

pub fn type1_partly_used() -> Document {
    let mut doc = Document::with_version("1.5");
    let program = type1_program();
    let mut stream = Stream::new(dictionary! { "Length1" => program.len() as i64 }, program);
    stream.compress().ok();
    let program = doc.add_object(stream);
    let descriptor = doc.add_object(dictionary! {
        "Type" => "FontDescriptor", "FontName" => "Boxes", "Flags" => 4,
        "FontBBox" => vec![0.into(), 0.into(), 700.into(), 700.into()],
        "ItalicAngle" => 0, "Ascent" => 700, "Descent" => 0, "CapHeight" => 700, "StemV" => 80,
        "FontFile" => program,
    });
    let widths: Vec<Object> = (0..5).map(|_| 600.into()).collect();
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Boxes",
        "FirstChar" => 65, "LastChar" => 69, "Widths" => widths, "FontDescriptor" => descriptor,
    });
    text_page(&mut doc, font, "AB");
    doc
}

pub fn arial_embedded() -> Document {
    let mut doc = Document::with_version("1.5");
    // A valid program (the box glyphs) under a standard font's name: the
    // name and encoding, not the outlines, decide unembedding.
    let program = type1_program();
    let mut stream = Stream::new(dictionary! { "Length1" => program.len() as i64 }, program);
    stream.compress().ok();
    let program = doc.add_object(stream);
    let descriptor = doc.add_object(dictionary! {
        "Type" => "FontDescriptor", "FontName" => "Arial", "Flags" => 32,
        "FontBBox" => vec![0.into(), 0.into(), 700.into(), 700.into()],
        "ItalicAngle" => 0, "Ascent" => 700, "Descent" => 0, "CapHeight" => 700, "StemV" => 80,
        "FontFile" => program,
    });
    let widths: Vec<Object> = (65..=69).map(|_| 600.into()).collect();
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Arial", "Encoding" => "WinAnsiEncoding",
        "FirstChar" => 65, "LastChar" => 69, "Widths" => widths, "FontDescriptor" => descriptor,
    });
    text_page(&mut doc, font, "ABC");
    doc
}

// ------------------------------------------------------------ structure

pub fn metadata_thumbnail() -> Document {
    let mut doc = Document::with_version("1.5");
    let thumb = image(
        &mut doc,
        dictionary! { "Width" => 16, "Height" => 16, "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8 },
        vec![200u8; 256],
    );
    let page = one_page(&mut doc, b"0 0 1 rg 20 20 100 100 re f", dictionary! {});
    doc.get_dictionary_mut(page).unwrap().set("Thumb", thumb);
    let xmp = b"<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?><x:xmpmeta xmlns:x='adobe:ns:meta/'></x:xmpmeta><?xpacket end='w'?>".to_vec();
    let metadata = doc.add_object(Stream::new(
        dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
        xmp,
    ));
    let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    doc.get_dictionary_mut(root)
        .unwrap()
        .set("Metadata", metadata);
    doc
}

pub fn form_default_fonts(xfa: bool) -> Document {
    let mut doc = Document::with_version("1.5");
    let page = one_page(&mut doc, b"", dictionary! {});
    let helv = doc.add_object(
        dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
    );
    let program = type1_program();
    let mut cour_program = Stream::new(dictionary! { "Length1" => program.len() as i64 }, program);
    cour_program.compress().ok();
    let cour_program = doc.add_object(cour_program);
    let cour_desc = doc.add_object(dictionary! { "Type" => "FontDescriptor", "FontName" => "Boxes", "Flags" => 4,
        "FontBBox" => vec![0.into(), 0.into(), 700.into(), 700.into()], "ItalicAngle" => 0, "Ascent" => 700,
        "Descent" => 0, "CapHeight" => 700, "StemV" => 80, "FontFile" => cour_program });
    let cour = doc.add_object(
        dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Boxes",
        "FontDescriptor" => cour_desc },
    );
    let field = doc.add_object(dictionary! {
        "FT" => "Tx", "T" => Object::string_literal("name"), "V" => Object::string_literal("hello"),
        "DA" => Object::string_literal("/Helv 12 Tf 0 g"), "Subtype" => "Widget", "P" => page,
        "Rect" => vec![20.into(), 200.into(), 200.into(), 230.into()],
    });
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![field.into()]);
    let mut acro = dictionary! {
        "Fields" => vec![field.into()],
        "DA" => Object::string_literal("/Helv 0 Tf 0 g"),
        "DR" => dictionary! { "Font" => dictionary! { "Helv" => helv, "Cour" => cour } },
    };
    if xfa {
        let xdp = doc.add_object(Stream::new(
            dictionary! {},
            b"<xdp:xdp xmlns:xdp='http://ns.adobe.com/xdp/'></xdp:xdp>".to_vec(),
        ));
        acro.set("XFA", xdp);
    }
    let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    doc.get_dictionary_mut(root).unwrap().set("AcroForm", acro);
    doc
}

/// Serialize a probe the way a reference tool would receive it.
pub fn to_bytes(doc: &mut Document) -> Vec<u8> {
    let mut out = Vec::new();
    doc.save_to(&mut out).expect("probe serializes");
    out.flush().ok();
    out
}
