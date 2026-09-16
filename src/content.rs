//! Canonical form of a content stream: single spaces, no comments,
//! numbers without redundant digits, everything else verbatim.
//!
//! The rewrite works on tokens, not on parsed objects: lopdf reads reals
//! into `f32`, so re-emitting from its operator list would round
//! coordinates, while a token stays the number the producer wrote. The
//! stream is still parsed strictly before and after, and the rewrite is
//! dropped unless both parses agree operation for operation. Streams with
//! inline images are left alone, since their binary data has no token
//! boundaries. The structure stage keeps the result when it compresses
//! smaller.

use lopdf::Object;
use lopdf::content::Content;

/// The stream in canonical token form, or `None` when it does not parse
/// strictly, holds an inline image, or the rewrite fails to parse back to
/// the same operations.
pub fn canonical(content: &[u8]) -> Option<Vec<u8>> {
    let before = Content::decode_strict(content).ok()?;
    if before.operations.iter().any(|op| op.operator == "BI") {
        return None;
    }
    let out = Tokens::new(content).canonical()?;
    let after = Content::decode_strict(&out).ok()?;
    let same = before.operations.len() == after.operations.len()
        && before
            .operations
            .iter()
            .zip(&after.operations)
            .all(|(a, b)| {
                a.operator == b.operator
                    && a.operands.len() == b.operands.len()
                    && a.operands
                        .iter()
                        .zip(&b.operands)
                        .all(|(x, y)| same_operand(x, y))
            });
    same.then_some(out)
}

/// Numbers compare by value (`-0.0` and `0`, `1.0` and `1` are the same
/// operand); everything else by its printed form.
fn same_operand(a: &Object, b: &Object) -> bool {
    match (number(a), number(b)) {
        (Some(x), Some(y)) => x == y,
        _ => format!("{a:?}") == format!("{b:?}"),
    }
}

fn number(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(f64::from(*r)),
        _ => None,
    }
}

// ------------------------------------------------------------- tokens

fn is_whitespace(b: u8) -> bool {
    matches!(b, b'\0' | b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn is_regular(b: u8) -> bool {
    !is_whitespace(b) && !is_delimiter(b)
}

/// A lexer over content-stream bytes that writes tokens back out with the
/// least separation the syntax needs.
struct Tokens<'a> {
    src: &'a [u8],
    pos: usize,
    out: Vec<u8>,
    /// The previous token ended with a regular character, so a following
    /// regular token needs a space.
    open: bool,
}

impl<'a> Tokens<'a> {
    fn new(src: &'a [u8]) -> Self {
        Tokens {
            src,
            pos: 0,
            out: Vec::with_capacity(src.len()),
            open: false,
        }
    }

    fn canonical(mut self) -> Option<Vec<u8>> {
        while self.pos < self.src.len() {
            self.step(self.src[self.pos])?;
        }
        Some(self.out)
    }

    /// Consume one token (or whitespace, or a comment) starting with `b`.
    fn step(&mut self, b: u8) -> Option<()> {
        match b {
            _ if is_whitespace(b) => self.pos += 1,
            b'%' => self.skip_comment(),
            b'(' => self.literal_string()?,
            b'<' => self.angle()?,
            b'/' => self.name(),
            _ if is_delimiter(b) => self.delimiter(b)?,
            _ => self.regular(),
        }
        Some(())
    }

    fn emit(&mut self, token: &[u8], regular: bool) {
        if regular && self.open {
            self.out.push(b' ');
        }
        self.out.extend_from_slice(token);
        self.open = regular;
    }

    fn skip_comment(&mut self) {
        while self.pos < self.src.len() && !matches!(self.src[self.pos], b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    /// `(...)` with nested parentheses and backslash escapes, verbatim.
    fn literal_string(&mut self) -> Option<()> {
        let start = self.pos;
        let mut depth = 0usize;
        loop {
            let b = *self.src.get(self.pos)?;
            self.pos += 1;
            match b {
                b'\\' => self.pos += 1,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        let token = self.src[start..self.pos.min(self.src.len())].to_vec();
        self.emit(&token, false);
        Some(())
    }

    /// `<<`, or a hex string `<...>` with its whitespace removed.
    fn angle(&mut self) -> Option<()> {
        if self.src.get(self.pos + 1) == Some(&b'<') {
            self.pos += 2;
            self.emit(b"<<", false);
            return Some(());
        }
        let mut token = vec![b'<'];
        self.pos += 1;
        loop {
            let b = *self.src.get(self.pos)?;
            self.pos += 1;
            if b == b'>' {
                break;
            }
            if !is_whitespace(b) {
                token.push(b);
            }
        }
        token.push(b'>');
        self.emit(&token, false);
        Some(())
    }

    fn name(&mut self) {
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.src.len() && is_regular(self.src[self.pos]) {
            self.pos += 1;
        }
        let token = self.src[start..self.pos].to_vec();
        // A name starts with a delimiter but ends with regular characters.
        self.emit(&token, false);
        self.open = true;
    }

    fn delimiter(&mut self, b: u8) -> Option<()> {
        let token: &[u8] = match b {
            b'[' => b"[",
            b']' => b"]",
            b'{' => b"{",
            b'}' => b"}",
            b'>' if self.src.get(self.pos + 1) == Some(&b'>') => b">>",
            _ => return None,
        };
        self.pos += token.len();
        self.emit(token, false);
        Some(())
    }

    /// A number or an operator.
    fn regular(&mut self) {
        let start = self.pos;
        while self.pos < self.src.len() && is_regular(self.src[self.pos]) {
            self.pos += 1;
        }
        let token = &self.src[start..self.pos];
        let token = canonical_number(token).unwrap_or_else(|| token.to_vec());
        self.emit(&token, true);
    }
}

/// A numeric token without its redundant characters: sign of zero,
/// leading zeros of the integer part, trailing zeros of the fraction, a
/// trailing point. `None` for tokens that are not plain numbers.
fn canonical_number(token: &[u8]) -> Option<Vec<u8>> {
    let (negative, digits) = match token.first()? {
        b'-' => (true, &token[1..]),
        b'+' => (false, &token[1..]),
        _ => (false, token),
    };
    let dots = digits.iter().filter(|&&b| b == b'.').count();
    if digits.is_empty() || dots > 1 || !digits.iter().all(|b| b.is_ascii_digit() || *b == b'.') {
        return None;
    }
    let (int, frac) = match digits.iter().position(|&b| b == b'.') {
        Some(i) => (&digits[..i], &digits[i + 1..]),
        None => (digits, &digits[digits.len()..]),
    };
    let int = &int[int.iter().take_while(|&&b| b == b'0').count()..];
    let frac = &frac[..frac.len() - frac.iter().rev().take_while(|&&b| b == b'0').count()];
    let mut out = Vec::with_capacity(token.len());
    if negative && !(int.is_empty() && frac.is_empty()) {
        out.push(b'-');
    }
    if int.is_empty() && frac.is_empty() {
        out.push(b'0');
    } else {
        out.extend_from_slice(int);
        if !frac.is_empty() {
            out.push(b'.');
            out.extend_from_slice(frac);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(s: &str) -> String {
        String::from_utf8(canonical(s.as_bytes()).expect("canonical")).unwrap()
    }

    #[test]
    fn whitespace_comments_and_numbers_are_normalized() {
        assert_eq!(
            canon("q\r\n  1.50000 0.000 +0 -0.0 10.00 20.5000 cm % comment\n/Im1   Do\nQ"),
            "q 1.5 0 0 0 10 20.5 cm/Im1 Do Q"
        );
        assert_eq!(
            canon("BT /F1 12 Tf ( a\\)(b) ) Tj [ (x) -20 <41 42> ] TJ ET"),
            "BT/F1 12 Tf( a\\)(b) )Tj[(x)-20<4142>]TJ ET"
        );
        assert_eq!(
            canon("/Span << /MCID 3 >> BDC EMC"),
            "/Span<</MCID 3>>BDC EMC"
        );
        assert_eq!(canon(".5 -.25 007.10 -0 w"), ".5 -.25 7.1 0 w");
    }

    #[test]
    fn unparseable_and_inline_image_streams_are_left_alone() {
        assert!(canonical(b"-. 0 Td").is_none());
        assert!(canonical(b"(unterminated").is_none());
        assert!(canonical(b"BI /W 1 /H 1 /CS /G /BPC 8 ID \x00 EI").is_none());
        assert!(canonical(b"").is_some());
    }

    #[test]
    fn number_edge_cases() {
        let n = |s: &str| canonical_number(s.as_bytes()).map(|v| String::from_utf8(v).unwrap());
        assert_eq!(n("+12"), Some("12".into()));
        assert_eq!(n("-000"), Some("0".into()));
        assert_eq!(n("100"), Some("100".into()));
        assert_eq!(n("1.0"), Some("1".into()));
        assert_eq!(n("1.2.3"), None);
        assert_eq!(n("Tj"), None);
        assert_eq!(n("-"), None);
    }
}
