//! The 14 standard fonts: recognizing their names (and the usual aliases
//! such as Arial for Helvetica) and deciding whether an embedded program
//! can be dropped in favor of the viewer's own copy.

use lopdf::{Dictionary, Document, Object};

use super::glyphnames;

/// Canonical standard-14 name for a base font name, if it is one. Subset
/// tags (`ABCDEF+`) are ignored; family aliases and style suffixes in the
/// common vendor spellings are folded.
pub fn canonical_name(base_font: &[u8]) -> Option<&'static str> {
    let name = String::from_utf8_lossy(base_font);
    let name = name.split_once('+').map_or(name.as_ref(), |(tag, rest)| {
        if tag.len() == 6 && tag.bytes().all(|b| b.is_ascii_uppercase()) {
            rest
        } else {
            name.as_ref()
        }
    });
    let folded: String = name
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | ',' | '_'))
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let (family, rest) = split_family(&folded)?;
    let style = style_of(rest)?;
    match family {
        Family::Symbol => (style == Style::Regular).then_some("Symbol"),
        Family::ZapfDingbats => (style == Style::Regular).then_some("ZapfDingbats"),
        Family::Helvetica => Some(HELVETICA[style as usize]),
        Family::Courier => Some(COURIER[style as usize]),
        Family::Times => Some(TIMES[style as usize]),
    }
}

/// Indexed by `Style`: regular, bold, italic, bold italic.
const HELVETICA: [&str; 4] = [
    "Helvetica",
    "Helvetica-Bold",
    "Helvetica-Oblique",
    "Helvetica-BoldOblique",
];
const COURIER: [&str; 4] = [
    "Courier",
    "Courier-Bold",
    "Courier-Oblique",
    "Courier-BoldOblique",
];
const TIMES: [&str; 4] = [
    "Times-Roman",
    "Times-Bold",
    "Times-Italic",
    "Times-BoldItalic",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Helvetica,
    Courier,
    Times,
    Symbol,
    ZapfDingbats,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

fn split_family(folded: &str) -> Option<(Family, &str)> {
    const FAMILIES: [(&str, Family); 7] = [
        ("helvetica", Family::Helvetica),
        ("arial", Family::Helvetica),
        ("couriernew", Family::Courier),
        ("courier", Family::Courier),
        ("timesnewroman", Family::Times),
        ("times", Family::Times),
        ("zapfdingbats", Family::ZapfDingbats),
    ];
    if folded == "symbol" {
        return Some((Family::Symbol, ""));
    }
    FAMILIES
        .iter()
        .find_map(|(prefix, family)| folded.strip_prefix(prefix).map(|rest| (*family, rest)))
}

/// The style from what follows the family name, provided every token is
/// one a vendor spelling of a standard font uses. Anything else (Narrow,
/// Condensed, Unicode, Black...) is a different font.
fn style_of(mut rest: &str) -> Option<Style> {
    const NOISE: [&str; 5] = ["psmt", "mt", "ps", "regular", "roman"];
    let (mut bold, mut italic) = (false, false);
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("bold") {
            bold = true;
            rest = r;
        } else if let Some(r) = rest
            .strip_prefix("italic")
            .or_else(|| rest.strip_prefix("oblique"))
        {
            italic = true;
            rest = r;
        } else {
            rest = NOISE.iter().find_map(|n| rest.strip_prefix(n))?;
        }
    }
    Some(match (bold, italic) {
        (false, false) => Style::Regular,
        (true, false) => Style::Bold,
        (false, true) => Style::Italic,
        (true, true) => Style::BoldItalic,
    })
}

const SYMBOLIC: i64 = 1 << 2;

/// Whether a simple font can lose its program: its name is a standard
/// font and the codes it uses are defined without it. Text fonts need an
/// encoding the viewer understands (a standard encoding name, or a
/// dictionary whose differences use known glyph names) or a non-symbolic
/// descriptor so the standard encoding applies; Symbol and ZapfDingbats
/// need their built-in encoding, so they may carry no `Encoding`.
pub fn can_unembed(
    doc: &Document,
    font: &Dictionary,
    descriptor: &Dictionary,
) -> Option<&'static str> {
    let canonical = canonical_name(font.get(b"BaseFont").ok()?.as_name().ok()?)?;
    let encoding = font.get(b"Encoding").ok().map(|e| deref(doc, e));
    let symbolic = descriptor
        .get(b"Flags")
        .and_then(Object::as_i64)
        .is_ok_and(|f| f & SYMBOLIC != 0);
    let ok = if matches!(canonical, "Symbol" | "ZapfDingbats") {
        encoding.is_none()
    } else {
        match encoding {
            None => !symbolic,
            Some(Object::Name(n)) => is_standard_encoding(n),
            Some(Object::Dictionary(d)) => encoding_dict_is_plain(doc, d),
            Some(_) => false,
        }
    };
    ok.then_some(canonical)
}

fn is_standard_encoding(name: &[u8]) -> bool {
    matches!(
        name,
        b"WinAnsiEncoding" | b"MacRomanEncoding" | b"StandardEncoding"
    )
}

fn encoding_dict_is_plain(doc: &Document, dict: &Dictionary) -> bool {
    if let Ok(base) = dict.get(b"BaseEncoding")
        && !base.as_name().is_ok_and(is_standard_encoding)
    {
        return false;
    }
    match dict.get(b"Differences").map(|d| deref(doc, d)) {
        Err(_) => true,
        Ok(Object::Array(items)) => items.iter().all(|item| match item {
            Object::Integer(_) => true,
            Object::Name(n) => glyphnames::unicode(n).is_some(),
            _ => false,
        }),
        Ok(_) => false,
    }
}

fn deref<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    doc.dereference(obj).map(|(_, o)| o).unwrap_or(obj)
}

#[cfg(test)]
mod tests {
    use lopdf::dictionary;

    use super::*;

    #[test]
    fn names_and_aliases_fold_to_the_canonical_font() {
        assert_eq!(canonical_name(b"Helvetica"), Some("Helvetica"));
        assert_eq!(
            canonical_name(b"ABCDEF+Arial-BoldMT"),
            Some("Helvetica-Bold")
        );
        assert_eq!(
            canonical_name(b"Arial,BoldItalic"),
            Some("Helvetica-BoldOblique")
        );
        assert_eq!(
            canonical_name(b"TimesNewRomanPS-ItalicMT"),
            Some("Times-Italic")
        );
        assert_eq!(canonical_name(b"Times-Roman"), Some("Times-Roman"));
        assert_eq!(canonical_name(b"CourierNewPSMT"), Some("Courier"));
        assert_eq!(
            canonical_name(b"Courier-BoldOblique"),
            Some("Courier-BoldOblique")
        );
        assert_eq!(canonical_name(b"Symbol"), Some("Symbol"));
        assert_eq!(canonical_name(b"ZapfDingbats"), Some("ZapfDingbats"));
        assert_eq!(canonical_name(b"ArialNarrow"), None);
        assert_eq!(canonical_name(b"Helvetica-Condensed-Bold"), None);
        assert_eq!(canonical_name(b"ArialUnicodeMS"), None);
        assert_eq!(canonical_name(b"Symbol-Bold"), None);
        assert_eq!(canonical_name(b"CMR10"), None);
    }

    #[test]
    fn unembedding_needs_a_trustworthy_encoding() {
        let doc = Document::with_version("1.5");
        let plain = dictionary! { "Flags" => 32 };
        let symbolic = dictionary! { "Flags" => 4 };
        let font = |enc: Option<Object>| {
            let mut d =
                dictionary! { "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "Arial" };
            if let Some(e) = enc {
                d.set("Encoding", e);
            }
            d
        };
        assert_eq!(can_unembed(&doc, &font(None), &plain), Some("Helvetica"));
        assert_eq!(can_unembed(&doc, &font(None), &symbolic), None);
        assert_eq!(
            can_unembed(&doc, &font(Some("WinAnsiEncoding".into())), &symbolic),
            Some("Helvetica")
        );
        let diffs =
            dictionary! { "Differences" => vec![65.into(), "eacute".into(), "bullet".into()] };
        assert_eq!(
            can_unembed(&doc, &font(Some(diffs.into())), &plain),
            Some("Helvetica")
        );
        let odd = dictionary! { "Differences" => vec![65.into(), "g123".into()] };
        assert_eq!(can_unembed(&doc, &font(Some(odd.into())), &plain), None);
        let symbol = dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Symbol" };
        assert_eq!(can_unembed(&doc, &symbol, &symbolic), Some("Symbol"));
        let mut symbol_enc = symbol.clone();
        symbol_enc.set("Encoding", "WinAnsiEncoding");
        assert_eq!(can_unembed(&doc, &symbol_enc, &symbolic), None);
    }
}
