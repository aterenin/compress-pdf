//! Reading Type 1 font programs as PDF embeds them (`FontFile`): the
//! cleartext header, the eexec-encrypted private portion, and the
//! charstrings inside it. No PostScript is interpreted; the tokens that
//! matter are picked out of the stream, which is enough for every font a
//! PDF producer writes.
//!
//! The approach (which tokens to look for, how generators deviate from
//! the specification) was learned by studying how pdf.js loads Type 1
//! fonts; the code here is an independent implementation of those
//! lessons, not a translation. The deviations it handles:
//! PFB segment headers left in place, a wrong `Length1`, hex or binary
//! eexec data, custom names for the `RD`/`ND`/`NP` procedures, missing
//! terminators after a charstring, a second `Subrs` or `CharStrings`
//! block for a hinting variant that no viewer uses, and `lenIV -1` for
//! unencrypted charstrings.

use std::collections::HashMap;

const EEXEC_KEY: u16 = 55665;
const CHARSTRING_KEY: u16 = 4330;

/// A parsed Type 1 program: what a CFF needs from it.
#[derive(Debug, Clone, PartialEq)]
pub struct Type1Font {
    pub font_name: Vec<u8>,
    pub font_matrix: Option<[f64; 6]>,
    pub font_bbox: Option<[f64; 4]>,
    /// The built-in encoding as code to glyph name; `None` for
    /// `StandardEncoding`.
    pub encoding: Option<HashMap<u8, Vec<u8>>>,
    /// Private dictionary values by their Type 1 key, in file order.
    pub private: Vec<(String, Vec<f64>)>,
    /// Decrypted subroutines, indexed by number.
    pub subrs: Vec<Vec<u8>>,
    /// Decrypted charstrings with their glyph names, in file order.
    pub charstrings: Vec<(Vec<u8>, Vec<u8>)>,
}

pub fn parse(program: &[u8]) -> Option<Type1Font> {
    let program = strip_pfb(program);
    let split = find_eexec(&program)?;
    let mut font = Type1Font {
        font_name: Vec::new(),
        font_matrix: None,
        font_bbox: None,
        encoding: None,
        private: Vec::new(),
        subrs: Vec::new(),
        charstrings: Vec::new(),
    };
    read_header(&program[..split], &mut font);
    let decrypted = decrypt_eexec(&program[split..]);
    read_private(&decrypted, &mut font)?;
    (!font.charstrings.is_empty()).then_some(font)
}

/// A PFB file left in place has 6-byte segment headers (`0x80`, type,
/// little-endian length) around each of its segments.
fn strip_pfb(program: &[u8]) -> Vec<u8> {
    if program.get(..2) != Some(&[0x80, 0x01]) {
        return program.to_vec();
    }
    let mut out = Vec::with_capacity(program.len());
    let mut pos = 0;
    while let Some(head) = program.get(pos..pos + 6)
        && head[0] == 0x80
        && matches!(head[1], 1 | 2)
    {
        let len = u32::from_le_bytes([head[2], head[3], head[4], head[5]]) as usize;
        let Some(segment) = program.get(pos + 6..pos + 6 + len) else {
            break;
        };
        out.extend_from_slice(segment);
        pos += 6 + len;
    }
    out
}

/// Byte offset of the encrypted portion: after `eexec` and the line
/// ending or spaces that follow it. Only those four bytes count as
/// whitespace here: a binary section can begin with any byte, NUL and
/// form feed included.
fn find_eexec(program: &[u8]) -> Option<usize> {
    let at = program.windows(5).position(|w| w == b"eexec")?;
    let mut pos = at + 5;
    while pos < program.len() && matches!(program[pos], b' ' | b'\t' | b'\r' | b'\n') {
        pos += 1;
    }
    Some(pos)
}

fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0')
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// The eexec portion is binary unless its first four bytes are hex
/// digits; either way the first four decrypted bytes are random padding.
fn decrypt_eexec(data: &[u8]) -> Vec<u8> {
    let hex = data.len() >= 4 && data[..4].iter().all(|b| is_hex(*b));
    let bytes: Vec<u8> = if hex {
        let digits: Vec<u8> = data.iter().copied().filter(|b| is_hex(*b)).collect();
        digits
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let s = std::str::from_utf8(pair).unwrap_or("00");
                u8::from_str_radix(s, 16).unwrap_or(0)
            })
            .collect()
    } else {
        data.to_vec()
    };
    decrypt(&bytes, EEXEC_KEY, 4)
}

/// The Type 1 cipher: each plaintext byte is the cipher byte xor the high
/// byte of a running key that the cipher byte then advances.
pub fn decrypt(data: &[u8], key: u16, skip: usize) -> Vec<u8> {
    let mut r = key;
    let mut out = Vec::with_capacity(data.len().saturating_sub(skip));
    for (i, &c) in data.iter().enumerate() {
        let p = c ^ (r >> 8) as u8;
        r = (u16::from(c).wrapping_add(r))
            .wrapping_mul(52845)
            .wrapping_add(22719);
        if i >= skip {
            out.push(p);
        }
    }
    out
}

// ------------------------------------------------------------ tokenizer

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token<'a> {
    /// A name after `/` (the slash itself is not included).
    Name(&'a [u8]),
    /// Anything else: keywords, numbers, `[`, `]`, `{`, `}`, `(`, `)`.
    Word(&'a [u8]),
}

struct Tokenizer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(data: &'a [u8]) -> Tokenizer<'a> {
        Tokenizer { data, pos: 0 }
    }

    fn next(&mut self) -> Option<Token<'a>> {
        self.skip_space_and_comments();
        let start = self.pos;
        let first = *self.data.get(self.pos)?;
        self.pos += 1;
        if first == b'/' {
            let end = self.word_end();
            return Some(Token::Name(&self.data[self.pos..end])).inspect(|_| self.pos = end);
        }
        if b"[]{}()".contains(&first) {
            return Some(Token::Word(&self.data[start..self.pos]));
        }
        let end = self.word_end();
        self.pos = end;
        Some(Token::Word(&self.data[start..end]))
    }

    fn word_end(&self) -> usize {
        let mut end = self.pos;
        while end < self.data.len()
            && !is_whitespace(self.data[end])
            && !b"/[]{}()".contains(&self.data[end])
        {
            end += 1;
        }
        end
    }

    fn skip_space_and_comments(&mut self) {
        while let Some(&b) = self.data.get(self.pos) {
            if b == b'%' {
                while self.pos < self.data.len() && !matches!(self.data[self.pos], b'\n' | b'\r') {
                    self.pos += 1;
                }
            } else if is_whitespace(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Binary data follows the `RD` procedure token and exactly one space.
    fn binary(&mut self, len: usize) -> Option<&'a [u8]> {
        let start = self.pos + 1;
        let bytes = self.data.get(start..start.checked_add(len)?)?;
        self.pos = start + len;
        Some(bytes)
    }

    fn number(&mut self) -> Option<f64> {
        match self.next()? {
            Token::Word(w) => parse_number(w),
            Token::Name(_) => None,
        }
    }

    /// `[ ... ]` or `{ ... }` of numbers; the opening bracket is consumed.
    fn number_array(&mut self) -> Vec<f64> {
        let mut out = Vec::new();
        let Some(Token::Word(open)) = self.next() else {
            return out;
        };
        if !matches!(open, b"[" | b"{") {
            return out;
        }
        while let Some(Token::Word(w)) = self.next() {
            if matches!(w, b"]" | b"}") {
                break;
            }
            if let Some(n) = parse_number(w) {
                out.push(n);
            }
        }
        out
    }
}

fn parse_number(word: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(word).ok()?;
    // PostScript radix numbers (16#FF) do not occur in the keys read here.
    s.parse::<f64>().ok()
}

// --------------------------------------------------------------- header

fn read_header(cleartext: &[u8], font: &mut Type1Font) {
    let mut t = Tokenizer::new(cleartext);
    while let Some(token) = t.next() {
        let Token::Name(key) = token else {
            continue;
        };
        match key {
            b"FontName" => {
                if let Some(Token::Name(n)) = t.next() {
                    font.font_name = n.to_vec();
                }
            }
            b"FontMatrix" => {
                let m = t.number_array();
                if m.len() == 6 {
                    font.font_matrix = Some([m[0], m[1], m[2], m[3], m[4], m[5]]);
                }
            }
            b"FontBBox" => {
                let b = t.number_array();
                if b.len() == 4 {
                    font.font_bbox = Some([b[0], b[1], b[2], b[3]]);
                }
            }
            b"Encoding" => font.encoding = read_encoding(&mut t),
            _ => {}
        }
    }
}

/// `/Encoding StandardEncoding def`, or `/Encoding N array` followed by
/// `dup code /name put` entries up to `readonly def` (or `def`).
fn read_encoding(t: &mut Tokenizer<'_>) -> Option<HashMap<u8, Vec<u8>>> {
    match t.next()? {
        Token::Word(b"StandardEncoding") => return None,
        Token::Word(w) if parse_number(w).is_some() => {}
        _ => return None,
    }
    let mut map = HashMap::new();
    loop {
        match t.next()? {
            Token::Word(b"dup") => {
                let code = t.number()?;
                let Token::Name(name) = t.next()? else {
                    continue;
                };
                if (0.0..=255.0).contains(&code) {
                    map.insert(code as u8, name.to_vec());
                }
            }
            Token::Word(b"def") | Token::Word(b"readonly") => break,
            _ => {}
        }
    }
    Some(map)
}

// -------------------------------------------------------------- private

const ARRAY_KEYS: [&str; 6] = [
    "BlueValues",
    "OtherBlues",
    "FamilyBlues",
    "FamilyOtherBlues",
    "StemSnapH",
    "StemSnapV",
];
const NUMBER_KEYS: [&str; 5] = [
    "BlueScale",
    "BlueShift",
    "BlueFuzz",
    "LanguageGroup",
    "ExpansionFactor",
];
/// Written as one-element arrays in Type 1, as numbers in CFF.
const SINGLE_KEYS: [&str; 2] = ["StdHW", "StdVW"];

fn read_private(data: &[u8], font: &mut Type1Font) -> Option<()> {
    let mut t = Tokenizer::new(data);
    let mut len_iv: i64 = 4;
    let (mut seen_subrs, mut seen_charstrings) = (false, false);
    while let Some(token) = t.next() {
        let Token::Name(key) = token else {
            continue;
        };
        match key {
            b"lenIV" => len_iv = t.number()? as i64,
            b"Subrs" if !seen_subrs => {
                seen_subrs = true;
                read_subrs(&mut t, len_iv, &mut font.subrs)?;
            }
            b"CharStrings" if !seen_charstrings => {
                seen_charstrings = true;
                read_charstrings(&mut t, len_iv, &mut font.charstrings)?;
            }
            _ => private_value(&mut t, key, &mut font.private)?,
        }
    }
    Some(())
}

/// One hinting-related private entry, when `key` names one.
fn private_value(
    t: &mut Tokenizer<'_>,
    key: &[u8],
    private: &mut Vec<(String, Vec<f64>)>,
) -> Option<()> {
    let name = String::from_utf8_lossy(key).into_owned();
    let values = if ARRAY_KEYS.contains(&name.as_str()) {
        t.number_array()
    } else if SINGLE_KEYS.contains(&name.as_str()) {
        t.number_array().into_iter().take(1).collect()
    } else if NUMBER_KEYS.contains(&name.as_str()) {
        vec![t.number()?]
    } else if key == b"ForceBold" {
        let value = matches!(t.next(), Some(Token::Word(b"true")));
        vec![f64::from(u8::from(value))]
    } else {
        return Some(());
    };
    private.push((name, values));
    Some(())
}

fn decrypt_charstring(data: &[u8], len_iv: i64) -> Vec<u8> {
    if len_iv < 0 {
        data.to_vec()
    } else {
        decrypt(data, CHARSTRING_KEY, len_iv as usize)
    }
}

/// `/Subrs N array` then `dup index length RD <bytes> NP` entries.
fn read_subrs(t: &mut Tokenizer<'_>, len_iv: i64, subrs: &mut Vec<Vec<u8>>) -> Option<()> {
    t.number()?;
    t.next()?; // array
    while let Some(Token::Word(b"dup")) = t.next() {
        let index = t.number()? as usize;
        let len = t.number()? as usize;
        t.next()?; // RD or -|
        let data = t.binary(len)?;
        if index < 65536 {
            if subrs.len() <= index {
                subrs.resize(index + 1, Vec::new());
            }
            subrs[index] = decrypt_charstring(data, len_iv);
        }
        // NP, |, or `noaccess put`: whatever follows is skipped by the
        // loop condition looking for the next `dup`.
        if let Some(Token::Word(b"noaccess")) = t.next() {
            t.next()?;
        }
    }
    Some(())
}

/// `/CharStrings N dict dup begin` then `/name length RD <bytes> ND`
/// entries up to `end`.
fn read_charstrings(
    t: &mut Tokenizer<'_>,
    len_iv: i64,
    out: &mut Vec<(Vec<u8>, Vec<u8>)>,
) -> Option<()> {
    loop {
        match t.next()? {
            Token::Word(b"begin") => break,
            Token::Word(b"end") | Token::Name(_) => return None,
            _ => {}
        }
    }
    loop {
        let name = match t.next()? {
            Token::Word(b"end") => break,
            Token::Name(n) => n,
            _ => continue,
        };
        let len = t.number()? as usize;
        t.next()?; // RD or -|
        let data = t.binary(len)?;
        out.push((name.to_vec(), decrypt_charstring(data, len_iv)));
        // ND, |-, or `noaccess def`. When the terminator is missing the
        // next token is already the next glyph's name; put it back.
        let after = t.pos;
        match t.next() {
            Some(Token::Word(b"noaccess")) => {
                t.next()?;
            }
            Some(Token::Name(_)) => t.pos = after,
            _ => {}
        }
    }
    Some(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The inverse of [`decrypt`], for building test fonts.
    pub(crate) fn encrypt(data: &[u8], key: u16, lead: usize) -> Vec<u8> {
        let mut r = key;
        let mut out = Vec::new();
        for &p in std::iter::repeat_n(&0u8, lead).chain(data) {
            let c = p ^ (r >> 8) as u8;
            r = (u16::from(c).wrapping_add(r))
                .wrapping_mul(52845)
                .wrapping_add(22719);
            out.push(c);
        }
        out
    }

    /// A Type 1 charstring from a readable operator list; numbers are
    /// encoded in the single-byte and two-byte forms.
    pub(crate) fn charstring(source: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for word in source.split_whitespace() {
            match word {
                "hsbw" => out.push(13),
                "rmoveto" => out.push(21),
                "hmoveto" => out.push(22),
                "vmoveto" => out.push(4),
                "rlineto" => out.push(5),
                "hlineto" => out.push(6),
                "vlineto" => out.push(7),
                "rrcurveto" => out.push(8),
                "closepath" => out.push(9),
                "callsubr" => out.push(10),
                "return" => out.push(11),
                "endchar" => out.push(14),
                "hstem" => out.push(1),
                "vstem" => out.push(3),
                "dotsection" => out.extend([12, 0]),
                "seac" => out.extend([12, 6]),
                "sbw" => out.extend([12, 7]),
                "div" => out.extend([12, 12]),
                "callothersubr" => out.extend([12, 16]),
                "pop" => out.extend([12, 17]),
                "setcurrentpoint" => out.extend([12, 33]),
                n => {
                    let v: i32 = n.parse().unwrap();
                    if (-107..=107).contains(&v) {
                        out.push((v + 139) as u8);
                    } else if (108..=1131).contains(&v) {
                        let v = v - 108;
                        out.extend([(v >> 8) as u8 + 247, (v & 0xff) as u8]);
                    } else if (-1131..=-108).contains(&v) {
                        let v = -v - 108;
                        out.extend([(v >> 8) as u8 + 251, (v & 0xff) as u8]);
                    } else {
                        out.push(255);
                        out.extend(v.to_be_bytes());
                    }
                }
            }
        }
        out
    }

    /// A complete two-glyph Type 1 program (a square `A` and `.notdef`)
    /// with a custom encoding mapping code 65 to `A`, eexec-encrypted in
    /// binary form.
    pub(crate) fn tiny_type1(hex: bool) -> Vec<u8> {
        let glyph_a = charstring(
            "50 600 hsbw 0 0 rmoveto 400 hlineto 400 vlineto -400 hlineto closepath endchar",
        );
        let notdef = charstring("0 500 hsbw endchar");
        let subr = charstring("100 100 rlineto return");
        let mut private = Vec::new();
        private.extend_from_slice(b"dup /Private 8 dict dup begin\n/RD {string currentfile exch readstring pop} executeonly def\n/ND {noaccess def} executeonly def\n/NP {noaccess put} executeonly def\n/BlueValues [ -10 0 700 710 ] def\n/StdHW [ 30 ] def\n/lenIV 4 def\n");
        private.extend_from_slice(b"/Subrs 1 array\n");
        let enc = encrypt(&subr, CHARSTRING_KEY, 4);
        private.extend_from_slice(format!("dup 0 {} RD ", enc.len()).as_bytes());
        private.extend_from_slice(&enc);
        private.extend_from_slice(b" NP\nND\nend\n/CharStrings 2 dict dup begin\n");
        for (name, cs) in [(".notdef", &notdef), ("A", &glyph_a)] {
            let enc = encrypt(cs, CHARSTRING_KEY, 4);
            private.extend_from_slice(format!("/{name} {} RD ", enc.len()).as_bytes());
            private.extend_from_slice(&enc);
            private.extend_from_slice(b" ND\n");
        }
        private.extend_from_slice(b"end\nend\nmark currentfile closefile\n");
        let encrypted = encrypt(&private, EEXEC_KEY, 4);
        let mut font = Vec::new();
        font.extend_from_slice(b"%!PS-AdobeFont-1.0: Tiny\n/FontName /Tiny def\n/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n/FontBBox {0 0 400 400} readonly def\n/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\ndup 65 /A put\nreadonly def\ncurrentdict end\ncurrentfile eexec\n");
        if hex {
            for b in &encrypted {
                font.extend_from_slice(format!("{b:02x}").as_bytes());
            }
        } else {
            font.extend_from_slice(&encrypted);
        }
        font
    }

    #[test]
    fn cipher_round_trips() {
        let plain = b"hello charstrings";
        let enc = encrypt(plain, CHARSTRING_KEY, 4);
        assert_eq!(decrypt(&enc, CHARSTRING_KEY, 4), plain);
    }

    #[test]
    fn parses_binary_and_hex_programs() {
        for hex in [false, true] {
            let font = parse(&tiny_type1(hex)).unwrap();
            assert_eq!(font.font_name, b"Tiny");
            assert_eq!(font.font_matrix, Some([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]));
            assert_eq!(font.font_bbox, Some([0.0, 0.0, 400.0, 400.0]));
            assert_eq!(font.encoding.as_ref().unwrap()[&65], b"A".to_vec());
            assert_eq!(font.subrs.len(), 1);
            assert_eq!(font.subrs[0], charstring("100 100 rlineto return"));
            assert_eq!(font.charstrings.len(), 2);
            assert_eq!(font.charstrings[1].0, b"A");
            assert_eq!(
                font.charstrings[1].1,
                charstring(
                    "50 600 hsbw 0 0 rmoveto 400 hlineto 400 vlineto -400 hlineto closepath endchar"
                )
            );
            assert_eq!(
                font.private[0],
                ("BlueValues".to_string(), vec![-10.0, 0.0, 700.0, 710.0])
            );
            assert_eq!(font.private[1], ("StdHW".to_string(), vec![30.0]));
        }
    }

    #[test]
    fn pfb_segments_are_stripped_and_garbage_is_rejected() {
        let plain = tiny_type1(false);
        let split = find_eexec(&plain).unwrap();
        let mut pfb = vec![0x80, 1];
        pfb.extend((split as u32).to_le_bytes());
        pfb.extend_from_slice(&plain[..split]);
        pfb.extend([0x80, 2]);
        pfb.extend(((plain.len() - split) as u32).to_le_bytes());
        pfb.extend_from_slice(&plain[split..]);
        pfb.extend([0x80, 3]);
        assert_eq!(parse(&pfb).unwrap().charstrings.len(), 2);
        assert!(parse(b"%!PS not a font at all").is_none());
    }
}
