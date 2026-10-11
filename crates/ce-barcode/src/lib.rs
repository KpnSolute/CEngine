//! CEngine barcode and QR generation (draft v1). Pure functions: data in, modules and SVG out.
//! No I/O, no clock, no tenant state, so every host (server, WASM, Python) gets identical
//! output.
//!
//! Symbologies: Code 128 (sets B and C), EAN-13, UPC-A, QR (error correction M).
//! Rules, all covered by tests:
//! 1. Check digits are computed for 12-digit EAN and 11-digit UPC input, and verified for
//!    full-length input; a wrong check digit is an error, never silently fixed.
//! 2. Input is validated before encoding: empty, over-long, or unsupported characters fail
//!    with the position of the first bad character.
//! 3. SVG output is built only from numbers and fixed text; no caller string reaches it.
//! 4. Output is deterministic.

mod code128;
mod ean;
mod qr;
mod svg;

pub use ean::{ean13_check_digit, upca_check_digit};
pub use svg::{render_svg, SvgOptions};

/// Longest input accepted for Code 128.
pub const MAX_CODE128_LEN: usize = 255;
/// Longest input accepted for QR, in bytes.
pub const MAX_QR_LEN: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symbology {
    Code128,
    Ean13,
    UpcA,
    Qr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BarcodeError {
    Empty,
    TooLong {
        max: usize,
    },
    /// Index of the first character the symbology cannot encode.
    BadChar {
        index: usize,
    },
    BadLength {
        expected: &'static str,
        got: usize,
    },
    BadCheckDigit {
        expected: u8,
    },
    /// The QR encoder refused the data (for example, it does not fit any QR version).
    Qr(String),
    BadOptions(&'static str),
}

/// The dark/light pattern of a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modules {
    /// One row of bars, `true` = dark, left to right, without quiet zones.
    Linear(Vec<bool>),
    /// A square matrix, row by row, `true` = dark, without quiet zone.
    Matrix { size: usize, dark: Vec<bool> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Barcode {
    pub symbology: Symbology,
    /// What the symbol encodes. For EAN-13 and UPC-A this includes the check digit.
    pub data: String,
    pub modules: Modules,
}

pub fn encode(symbology: Symbology, data: &str) -> Result<Barcode, BarcodeError> {
    if data.is_empty() {
        return Err(BarcodeError::Empty);
    }
    match symbology {
        Symbology::Code128 => code128::encode(data),
        Symbology::Ean13 => ean::encode_ean13(data),
        Symbology::UpcA => ean::encode_upca(data),
        Symbology::Qr => qr::encode(data),
    }
}
