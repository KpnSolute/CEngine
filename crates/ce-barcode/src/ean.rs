use crate::{Barcode, BarcodeError, Modules, Symbology};

/// Left-hand digit patterns, odd parity ("L"). 7 modules each, 1 = dark.
const L: [&str; 10] = [
    "0001101", "0011001", "0010011", "0111101", "0100011", "0110001", "0101111", "0111011",
    "0110111", "0001011",
];
/// Left-hand even parity ("G"): the reverse of the R pattern.
const G: [&str; 10] = [
    "0100111", "0110011", "0011011", "0100001", "0011101", "0111001", "0000101", "0010001",
    "0001001", "0010111",
];
/// Right-hand patterns ("R"): the complement of L.
const R: [&str; 10] = [
    "1110010", "1100110", "1101100", "1000010", "1011100", "1001110", "1010000", "1000100",
    "1001000", "1110100",
];
/// Parity of the six left digits, chosen by the first digit. `L` = false, `G` = true.
const PARITY: [[bool; 6]; 10] = [
    [false, false, false, false, false, false],
    [false, false, true, false, true, true],
    [false, false, true, true, false, true],
    [false, false, true, true, true, false],
    [false, true, false, false, true, true],
    [false, true, true, false, false, true],
    [false, true, true, true, false, false],
    [false, true, false, true, false, true],
    [false, true, false, true, true, false],
    [false, true, true, false, true, false],
];

fn digits(s: &str) -> Result<Vec<u8>, BarcodeError> {
    s.chars()
        .enumerate()
        .map(|(i, c)| {
            c.to_digit(10)
                .map(|d| d as u8)
                .ok_or(BarcodeError::BadChar { index: i })
        })
        .collect()
}

/// Check digit for the first 12 digits of an EAN-13.
pub fn ean13_check_digit(first12: &str) -> Result<u8, BarcodeError> {
    let d = digits(first12)?;
    if d.len() != 12 {
        return Err(BarcodeError::BadLength {
            expected: "12 digits",
            got: d.len(),
        });
    }
    let sum: u32 = d
        .iter()
        .enumerate()
        .map(|(i, &x)| u32::from(x) * if i % 2 == 0 { 1 } else { 3 })
        .sum();
    Ok(((10 - sum % 10) % 10) as u8)
}

/// Check digit for the first 11 digits of a UPC-A.
pub fn upca_check_digit(first11: &str) -> Result<u8, BarcodeError> {
    let d = digits(first11)?;
    if d.len() != 11 {
        return Err(BarcodeError::BadLength {
            expected: "11 digits",
            got: d.len(),
        });
    }
    let sum: u32 = d
        .iter()
        .enumerate()
        .map(|(i, &x)| u32::from(x) * if i % 2 == 0 { 3 } else { 1 })
        .sum();
    Ok(((10 - sum % 10) % 10) as u8)
}

fn bits(pattern: &str, out: &mut Vec<bool>) {
    out.extend(pattern.bytes().map(|b| b == b'1'));
}

fn modules13(d: &[u8]) -> Vec<bool> {
    let mut m = Vec::with_capacity(95);
    bits("101", &mut m);
    let parity = PARITY[d[0] as usize];
    for (i, &digit) in d[1..7].iter().enumerate() {
        bits(
            if parity[i] {
                G[digit as usize]
            } else {
                L[digit as usize]
            },
            &mut m,
        );
    }
    bits("01010", &mut m);
    for &digit in &d[7..13] {
        bits(R[digit as usize], &mut m);
    }
    bits("101", &mut m);
    m
}

pub fn encode_ean13(data: &str) -> Result<Barcode, BarcodeError> {
    let mut d = digits(data)?;
    match d.len() {
        12 => {
            d.push(ean13_check_digit(data)?);
        }
        13 => {
            let expected = ean13_check_digit(&data[..12])?;
            if d[12] != expected {
                return Err(BarcodeError::BadCheckDigit { expected });
            }
        }
        got => {
            return Err(BarcodeError::BadLength {
                expected: "12 or 13 digits",
                got,
            })
        }
    }
    Ok(Barcode {
        symbology: Symbology::Ean13,
        data: d.iter().map(|x| char::from(b'0' + x)).collect(),
        modules: Modules::Linear(modules13(&d)),
    })
}

/// UPC-A is an EAN-13 whose first digit is 0.
pub fn encode_upca(data: &str) -> Result<Barcode, BarcodeError> {
    let mut d = digits(data)?;
    match d.len() {
        11 => {
            d.push(upca_check_digit(data)?);
        }
        12 => {
            let expected = upca_check_digit(&data[..11])?;
            if d[11] != expected {
                return Err(BarcodeError::BadCheckDigit { expected });
            }
        }
        got => {
            return Err(BarcodeError::BadLength {
                expected: "11 or 12 digits",
                got,
            })
        }
    }
    let mut full = vec![0u8];
    full.extend(&d);
    Ok(Barcode {
        symbology: Symbology::UpcA,
        data: d.iter().map(|x| char::from(b'0' + x)).collect(),
        modules: Modules::Linear(modules13(&full)),
    })
}
