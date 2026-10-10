use crate::{Barcode, BarcodeError, Modules};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvgOptions {
    /// Width of one module in SVG units (1..=32).
    pub module_size: u32,
    /// Quiet zone around the symbol, in modules (0..=20). `None` picks the standard: 10 for
    /// linear codes and 4 for QR.
    pub quiet_zone: Option<u32>,
    /// Height of the bars for linear codes, in modules (1..=400). QR is always square.
    pub bar_height: u32,
}

impl Default for SvgOptions {
    fn default() -> Self {
        SvgOptions {
            module_size: 2,
            quiet_zone: None,
            bar_height: 50,
        }
    }
}

/// Renders a symbol as a standalone SVG. Only numbers and fixed text are written; nothing the
/// caller supplied as data reaches the output.
pub fn render_svg(code: &Barcode, opt: &SvgOptions) -> Result<String, BarcodeError> {
    if !(1..=32).contains(&opt.module_size) {
        return Err(BarcodeError::BadOptions("module_size must be 1 to 32"));
    }
    if !(1..=400).contains(&opt.bar_height) {
        return Err(BarcodeError::BadOptions("bar_height must be 1 to 400"));
    }
    if opt.quiet_zone.is_some_and(|q| q > 20) {
        return Err(BarcodeError::BadOptions("quiet_zone must be 0 to 20"));
    }
    let m = opt.module_size;
    let mut body = String::new();
    let (w, h) = match &code.modules {
        Modules::Linear(bars) => {
            let q = opt.quiet_zone.unwrap_or(10);
            let mut x = 0usize;
            while x < bars.len() {
                if bars[x] {
                    let start = x;
                    while x < bars.len() && bars[x] {
                        x += 1;
                    }
                    let _ = write!(
                        body,
                        "M{} {}h{}v{}h-{}z",
                        (q as usize + start) as u32 * m,
                        q * m,
                        (x - start) as u32 * m,
                        opt.bar_height * m,
                        (x - start) as u32 * m,
                    );
                } else {
                    x += 1;
                }
            }
            (
                (bars.len() as u32 + 2 * q) * m,
                (opt.bar_height + 2 * q) * m,
            )
        }
        Modules::Matrix { size, dark } => {
            let q = opt.quiet_zone.unwrap_or(4);
            for (i, &d) in dark.iter().enumerate() {
                if d {
                    let (row, col) = (i / size, i % size);
                    let _ = write!(
                        body,
                        "M{} {}h{m}v{m}h-{m}z",
                        (q as usize + col) as u32 * m,
                        (q as usize + row) as u32 * m
                    );
                }
            }
            let side = (*size as u32 + 2 * q) * m;
            (side, side)
        }
    };
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" shape-rendering=\"crispEdges\"><rect width=\"{w}\" height=\"{h}\" fill=\"#fff\"/><path d=\"{body}\" fill=\"#000\"/></svg>"
    ))
}
