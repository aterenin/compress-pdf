//! Why a document was not compressed, or its output not accepted. One
//! enum, so a program can react to a specific outcome (skip files that
//! need a password, treat a verification regression as a bug) without
//! comparing message text; `Display` gives the message the command prints.

use std::fmt;

use crate::verify::Category;
use crate::verify::render::Comparison;

#[derive(Debug)]
#[non_exhaustive]
pub enum Refusal {
    /// The input does not parse as a PDF.
    Unparseable(lopdf::Error),
    /// A password is needed to open the input.
    PasswordRequired,
    /// The input is encrypted with crypt filters the parser could not
    /// read, so it was not decrypted and would be written as garbage.
    UndecryptedCryptFilters,
    /// The page tree refers to objects the parser could not load.
    DamagedPageTree,
    /// Page content or resources refer to objects the parser could not load.
    DamagedResources,
    /// The output verifies worse than the input: per category, the
    /// input's problem count and the output's.
    VerificationRegressed(Vec<(Category, usize, usize)>),
    /// Pages fell below the similarity floor under strict verification.
    BelowSimilarityFloor(Comparison),
    /// A stage or the writer failed.
    Internal(anyhow::Error),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Unparseable(e) => write!(f, "parsing the input: {e}"),
            Refusal::PasswordRequired => {
                f.write_str("input is encrypted with a password that is needed to open it")
            }
            Refusal::UndecryptedCryptFilters => f.write_str(
                "input is encrypted with crypt filters the parser could not read, so it was not decrypted",
            ),
            Refusal::DamagedPageTree => {
                f.write_str("page tree refers to objects the parser could not load")
            }
            Refusal::DamagedResources => {
                f.write_str("page content or resources refer to objects the parser could not load")
            }
            Refusal::VerificationRegressed(regressions) => write!(
                f,
                "output failed verification with new problems {regressions:?}; nothing written (this is a bug, please report it)"
            ),
            Refusal::BelowSimilarityFloor(_) => {
                f.write_str("pages below the similarity floor; nothing written (--strict)")
            }
            Refusal::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Refusal::Unparseable(e) => Some(e),
            Refusal::Internal(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

/// Stage and writer errors become `Internal`, so `?` works inside the
/// pipeline.
impl From<anyhow::Error> for Refusal {
    fn from(e: anyhow::Error) -> Self {
        Refusal::Internal(e)
    }
}
