use anyhow::Result;
use qrcode::QrCode;

/// QR Code generator for Bluetooth/WiFi configuration
///
/// Generates a scannable QR code containing device ID and pairing code
/// in the format: BT:DEVICE_ID:PAIRING_CODE
pub struct QrCodeGenerator {
    device_id: String,
    pairing_code: String,
    qr_matrix: Vec<Vec<bool>>,
}

impl QrCodeGenerator {
    /// Create a new QR code generator
    ///
    /// # Arguments
    /// * `device_id` - Unique device identifier (e.g., "FIBER_001")
    /// * `pairing_code` - Bluetooth pairing code (e.g., "1234")
    pub fn new(device_id: String, pairing_code: String) -> Result<Self> {
        // Generate QR code data in format: BT:DEVICE_ID:PAIRING_CODE
        let qr_content = format!("BT:{}:{}", device_id, pairing_code);
        let code = QrCode::new(&qr_content)?;

        // Convert QR code to boolean matrix for rendering
        let qr_matrix = Self::qr_to_matrix(&code);

        Ok(Self {
            device_id,
            pairing_code,
            qr_matrix,
        })
    }

    /// Convert QR code to a boolean matrix
    fn qr_to_matrix(code: &QrCode) -> Vec<Vec<bool>> {
        // Get the rendered content of the QR code
        let rendered = code.render::<char>()
            .quiet_zone(false)
            .build();

        let mut matrix = Vec::new();

        for line in rendered.lines() {
            let row: Vec<bool> = line.chars()
                .map(|c| c == '█')  // Black block is represented as '█'
                .collect();
            matrix.push(row);
        }

        matrix
    }

    /// Get the QR code matrix as a 2D boolean array
    ///
    /// Returns a vector of rows, where each row is a vector of booleans.
    /// `true` = black pixel, `false` = white pixel
    pub fn get_qr_matrix(&self) -> &Vec<Vec<bool>> {
        &self.qr_matrix
    }

    /// Get the dimension of the QR code (always square)
    pub fn get_qr_dimension(&self) -> usize {
        if self.qr_matrix.is_empty() {
            0
        } else {
            self.qr_matrix.len()
        }
    }

    /// Get device ID
    pub fn get_device_id(&self) -> &str {
        &self.device_id
    }

    /// Get pairing code
    pub fn get_pairing_code(&self) -> &str {
        &self.pairing_code
    }

    /// Get the full QR content string
    pub fn get_content(&self) -> String {
        format!("BT:{}:{}", self.device_id, self.pairing_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qr_generator_creation() {
        let generator = QrCodeGenerator::new(
            "FIBER_001".to_string(),
            "1234".to_string(),
        ).expect("Failed to create QR generator");

        assert_eq!(generator.get_device_id(), "FIBER_001");
        assert_eq!(generator.get_pairing_code(), "1234");
        assert_eq!(generator.get_content(), "BT:FIBER_001:1234");
    }

    #[test]
    fn test_qr_matrix_is_square() {
        let generator = QrCodeGenerator::new(
            "TEST_DEVICE".to_string(),
            "9999".to_string(),
        ).expect("Failed to create QR generator");

        let matrix = generator.get_qr_matrix();
        let dim = generator.get_qr_dimension();

        assert!(!matrix.is_empty(), "QR matrix should not be empty");
        assert_eq!(matrix.len(), dim, "QR matrix should be square");

        for row in matrix {
            assert_eq!(row.len(), dim, "Each row should have the correct width");
        }
    }
}
