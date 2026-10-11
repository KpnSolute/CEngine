use crate::{Barcode, BarcodeError, Modules, Symbology, MAX_QR_LEN};
use qrcode::{Color, EcLevel, QrCode};

pub fn encode(data: &str) -> Result<Barcode, BarcodeError> {
    if data.len() > MAX_QR_LEN {
        return Err(BarcodeError::TooLong { max: MAX_QR_LEN });
    }
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M)
        .map_err(|e| BarcodeError::Qr(e.to_string()))?;
    let size = code.width();
    let dark = code
        .to_colors()
        .into_iter()
        .map(|c| c == Color::Dark)
        .collect();
    Ok(Barcode {
        symbology: Symbology::Qr,
        data: data.to_string(),
        modules: Modules::Matrix { size, dark },
    })
}
