use ce_barcode::*;
use serde_json_lite::Vectors;

/// A tiny reader for tests/vectors.json (flat string/number map) so the crate needs no JSON
/// dependency, even for tests.
mod serde_json_lite {
    use std::collections::HashMap;
    pub struct Vectors(HashMap<String, String>);
    impl Vectors {
        pub fn load() -> Vectors {
            let text =
                std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors.json"))
                    .unwrap();
            let mut map = HashMap::new();
            for line in text.lines() {
                let line = line.trim().trim_end_matches(',');
                if let Some((k, v)) = line.split_once("\": ") {
                    let k = k.trim_start_matches('"').to_string();
                    map.insert(k, v.trim_matches('"').to_string());
                }
            }
            Vectors(map)
        }
        pub fn get(&self, key: &str) -> &str {
            self.0
                .get(key)
                .unwrap_or_else(|| panic!("missing vector {key}"))
        }
    }
}

fn bits(code: &Barcode) -> String {
    match &code.modules {
        Modules::Linear(m) => m.iter().map(|&b| if b { '1' } else { '0' }).collect(),
        Modules::Matrix { dark, .. } => dark.iter().map(|&b| if b { '1' } else { '0' }).collect(),
    }
}

#[test]
fn code128_matches_reference_output() {
    let v = Vectors::load();
    for (data, key) in [
        ("Wikipedia", "code128_Wikipedia"),
        ("123456", "code128_123456"),
        ("Hello 42!", "code128_Hello 42!"),
    ] {
        let code = encode(Symbology::Code128, data).unwrap();
        assert_eq!(bits(&code), v.get(key), "{data}");
        assert_eq!(code.data, data);
    }
}

#[test]
fn ean13_and_upca_match_reference_output() {
    let v = Vectors::load();
    // 12 digits get a check digit; 13 digits are verified.
    let a = encode(Symbology::Ean13, "590123412345").unwrap();
    assert_eq!(a.data, "5901234123457");
    assert_eq!(bits(&a), v.get("ean13_590123412345"));
    let b = encode(Symbology::Ean13, "4006381333931").unwrap();
    assert_eq!(bits(&b), v.get("ean13_4006381333931"));
    let u = encode(Symbology::UpcA, "03600029145").unwrap();
    assert_eq!(u.data, "036000291452");
    assert_eq!(bits(&u), v.get("upca_03600029145"));
    assert_eq!(
        bits(&encode(Symbology::UpcA, "036000291452").unwrap()),
        bits(&u)
    );
}

#[test]
fn check_digit_helpers() {
    assert_eq!(ean13_check_digit("590123412345"), Ok(7));
    assert_eq!(upca_check_digit("03600029145"), Ok(2));
    assert_eq!(
        ean13_check_digit("59012341234"),
        Err(BarcodeError::BadLength {
            expected: "12 digits",
            got: 11
        })
    );
}

#[test]
fn wrong_check_digits_are_errors() {
    assert_eq!(
        encode(Symbology::Ean13, "5901234123450"),
        Err(BarcodeError::BadCheckDigit { expected: 7 })
    );
    assert_eq!(
        encode(Symbology::UpcA, "036000291450"),
        Err(BarcodeError::BadCheckDigit { expected: 2 })
    );
}

#[test]
fn bad_input_is_rejected_with_a_position() {
    assert_eq!(encode(Symbology::Code128, ""), Err(BarcodeError::Empty));
    assert_eq!(
        encode(Symbology::Code128, "ab\ncd"),
        Err(BarcodeError::BadChar { index: 2 })
    );
    assert_eq!(
        encode(Symbology::Code128, "caf\u{e9}"),
        Err(BarcodeError::BadChar { index: 3 })
    );
    assert_eq!(
        encode(Symbology::Ean13, "59012341234x"),
        Err(BarcodeError::BadChar { index: 11 })
    );
    assert_eq!(
        encode(Symbology::Ean13, "12345"),
        Err(BarcodeError::BadLength {
            expected: "12 or 13 digits",
            got: 5
        })
    );
    assert_eq!(
        encode(Symbology::UpcA, "1234"),
        Err(BarcodeError::BadLength {
            expected: "11 or 12 digits",
            got: 4
        })
    );
    assert_eq!(
        encode(Symbology::Code128, &"a".repeat(256)),
        Err(BarcodeError::TooLong {
            max: MAX_CODE128_LEN
        })
    );
    assert_eq!(
        encode(Symbology::Qr, &"a".repeat(MAX_QR_LEN + 1)),
        Err(BarcodeError::TooLong { max: MAX_QR_LEN })
    );
}

#[test]
fn qr_matches_reference_output() {
    let v = Vectors::load();
    let code = encode(Symbology::Qr, "HELLO WORLD").unwrap();
    assert_eq!(bits(&code), v.get("qr_HELLO WORLD"));
}

#[test]
fn qr_has_finder_patterns_and_a_valid_size() {
    let code = encode(
        Symbology::Qr,
        "https://example.com/some/longer/path?with=query&and=more",
    )
    .unwrap();
    let Modules::Matrix { size, dark } = &code.modules else {
        panic!("matrix expected")
    };
    assert_eq!((size - 17) % 4, 0, "size is 17 + 4 * version");
    // Each corner has a 7x7 finder: dark ring, light ring, dark 3x3 core.
    for (r0, c0) in [(0, 0), (0, size - 7), (size - 7, 0)] {
        for r in 0..7 {
            for c in 0..7 {
                let ring = r == 0 || r == 6 || c == 0 || c == 6;
                let core = (2..=4).contains(&r) && (2..=4).contains(&c);
                assert_eq!(
                    dark[(r0 + r) * size + c0 + c],
                    ring || core,
                    "finder at ({r0},{c0}) cell ({r},{c})"
                );
            }
        }
    }
}

#[test]
fn every_code128_symbol_is_well_formed() {
    // Round trip: encode every printable ASCII character alone and check the layout:
    // start(11) + one symbol(11) + check(11) + stop(13) modules, begins and ends with a bar.
    for c in ' '..='~' {
        let code = encode(Symbology::Code128, &c.to_string()).unwrap();
        let Modules::Linear(m) = &code.modules else {
            panic!()
        };
        assert_eq!(m.len(), 11 + 11 + 11 + 13, "{c:?}");
        assert!(m[0] && m[m.len() - 1]);
    }
    // Quiet-free stop pattern is the same in every symbol.
    let a = bits(&encode(Symbology::Code128, "a").unwrap());
    let b = bits(&encode(Symbology::Code128, "b").unwrap());
    assert_eq!(a[a.len() - 13..], b[b.len() - 13..]);
}

#[test]
fn svg_is_deterministic_and_contains_no_input() {
    let code = encode(Symbology::Code128, "<script>alert(1)</script>").unwrap();
    let a = render_svg(&code, &SvgOptions::default()).unwrap();
    let b = render_svg(&code, &SvgOptions::default()).unwrap();
    assert_eq!(a, b);
    assert!(a.starts_with("<svg "));
    assert!(!a.contains("script") && !a.contains("alert"));
    assert_eq!(
        a.matches('<').count(),
        4,
        "svg, rect, path and the closing tag only"
    );

    let qr = encode(Symbology::Qr, "x").unwrap();
    let s = render_svg(
        &qr,
        &SvgOptions {
            module_size: 4,
            quiet_zone: Some(2),
            bar_height: 1,
        },
    )
    .unwrap();
    // version 1 is 21 modules; (21 + 2 * 2) * 4 = 100
    assert!(s.contains("viewBox=\"0 0 100 100\""), "{s}");
}

#[test]
fn svg_size_follows_options() {
    let e = encode(Symbology::Ean13, "590123412345").unwrap();
    let s = render_svg(
        &e,
        &SvgOptions {
            module_size: 3,
            quiet_zone: None,
            bar_height: 40,
        },
    )
    .unwrap();
    // width (95 + 2 * 10) * 3 = 345, height (40 + 2 * 10) * 3 = 180
    assert!(s.contains("viewBox=\"0 0 345 180\""), "{s}");
}

#[test]
fn svg_options_are_bounded() {
    let e = encode(Symbology::Ean13, "590123412345").unwrap();
    for opt in [
        SvgOptions {
            module_size: 0,
            ..Default::default()
        },
        SvgOptions {
            module_size: 33,
            ..Default::default()
        },
        SvgOptions {
            bar_height: 0,
            ..Default::default()
        },
        SvgOptions {
            bar_height: 401,
            ..Default::default()
        },
        SvgOptions {
            quiet_zone: Some(21),
            ..Default::default()
        },
    ] {
        assert!(
            matches!(render_svg(&e, &opt), Err(BarcodeError::BadOptions(_))),
            "{opt:?}"
        );
    }
}
