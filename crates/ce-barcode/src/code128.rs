use crate::{Barcode, BarcodeError, Modules, Symbology, MAX_CODE128_LEN};

/// Bar and space widths for symbol values 0..=105 (six elements, bar first), then the stop
/// pattern (seven elements). Each of the first 106 sums to 11 modules.
const PATTERNS: [&str; 107] = [
    "212222", "222122", "222221", "121223", "121322", "131222", "122213", "122312", "132212",
    "221213", "221312", "231212", "112232", "122132", "122231", "113222", "123122", "123221",
    "223211", "221132", "221231", "213212", "223112", "312131", "311222", "321122", "321221",
    "312212", "322112", "322211", "212123", "212321", "232121", "111323", "131123", "131321",
    "112313", "132113", "132311", "211313", "231113", "231311", "112133", "112331", "132131",
    "113123", "113321", "133121", "313121", "211331", "231131", "213113", "213311", "213131",
    "311123", "311321", "331121", "312113", "312311", "332111", "314111", "221411", "431111",
    "111224", "111422", "121124", "121421", "141122", "141221", "112214", "112412", "122114",
    "122411", "142112", "142211", "241211", "221114", "413111", "241112", "134111", "111242",
    "121142", "121241", "114212", "124112", "124211", "411212", "421112", "421211", "212141",
    "214121", "412121", "111143", "111341", "131141", "114113", "114311", "411113", "411311",
    "113141", "114131", "311141", "411131", "211412", "211214", "211232", "2331112",
];

const START_B: usize = 104;
const START_C: usize = 105;
const STOP: usize = 106;

pub(crate) fn pattern(symbol: usize) -> &'static str {
    PATTERNS[symbol]
}

/// Symbol values for `data`: all-digit input of even length (at least 4) uses set C, two
/// digits per symbol. Anything else is set B (printable ASCII). Switching sets in the middle
/// of a string is not attempted, so the encoding stays simple and predictable.
fn symbols(data: &str) -> Result<Vec<usize>, BarcodeError> {
    let bytes = data.as_bytes();
    for (i, c) in data.chars().enumerate() {
        if !(' '..='~').contains(&c) {
            return Err(BarcodeError::BadChar { index: i });
        }
    }
    let mut out = Vec::with_capacity(bytes.len() + 3);
    if bytes.len() >= 4 && bytes.len().is_multiple_of(2) && bytes.iter().all(u8::is_ascii_digit) {
        out.push(START_C);
        for pair in bytes.chunks(2) {
            out.push(usize::from(pair[0] - b'0') * 10 + usize::from(pair[1] - b'0'));
        }
    } else {
        out.push(START_B);
        out.extend(bytes.iter().map(|&b| usize::from(b - b' ')));
    }
    let sum: usize = out[0]
        + out[1..]
            .iter()
            .enumerate()
            .map(|(i, &v)| v * (i + 1))
            .sum::<usize>();
    out.push(sum % 103);
    out.push(STOP);
    Ok(out)
}

pub fn encode(data: &str) -> Result<Barcode, BarcodeError> {
    if data.chars().count() > MAX_CODE128_LEN {
        return Err(BarcodeError::TooLong {
            max: MAX_CODE128_LEN,
        });
    }
    let syms = symbols(data)?;
    let mut modules = Vec::with_capacity(syms.len() * 11 + 2);
    for s in syms {
        let mut dark = true;
        for w in pattern(s).bytes() {
            modules.extend(std::iter::repeat_n(dark, usize::from(w - b'0')));
            dark = !dark;
        }
    }
    Ok(Barcode {
        symbology: Symbology::Code128,
        data: data.to_string(),
        modules: Modules::Linear(modules),
    })
}
