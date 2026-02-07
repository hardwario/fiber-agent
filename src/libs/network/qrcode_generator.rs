use anyhow::Result;
use qrcode::QrCode;

/// QR Code generator for Bluetooth/WiFi configuration
///
/// Generates a scannable QR code containing device connection info as JSON:
/// {"t":"fiber","v":1,"m":"AA:BB:CC:DD:EE:FF","p":"123456","n":"FIBER-001"}
pub struct QrCodeGenerator {
    mac_address: String,
    pin: String,
    device_name: String,
    qr_matrix: Vec<Vec<bool>>,
}

impl QrCodeGenerator {
    /// Create a new QR code generator
    ///
    /// # Arguments
    /// * `mac_address` - Bluetooth MAC address (e.g., "AA:BB:CC:DD:EE:FF")
    /// * `pin` - PIN for authentication (e.g., "123456")
    /// * `device_name` - Device name/hostname (e.g., "FIBER-001")
    pub fn new(mac_address: String, pin: String, device_name: String) -> Result<Self> {
        // Generate QR code data in JSON format
        // t = type ("fiber"), v = protocol version, m = MAC, p = PIN, n = name
        let qr_content = format!(
            r#"{{"t":"fiber","v":1,"m":"{}","p":"{}","n":"{}"}}"#,
            mac_address, pin, device_name
        );
        let code = QrCode::new(&qr_content)?;

        // Convert QR code to boolean matrix for rendering
        let qr_matrix = Self::qr_to_matrix(&code);

        Ok(Self {
            mac_address,
            pin,
            device_name,
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

    /// Get MAC address
    pub fn get_mac_address(&self) -> &str {
        &self.mac_address
    }

    /// Get PIN
    pub fn get_pin(&self) -> &str {
        &self.pin
    }

    /// Get device name
    pub fn get_device_name(&self) -> &str {
        &self.device_name
    }

    /// Get the full QR content string (JSON format)
    pub fn get_content(&self) -> String {
        format!(
            r#"{{"t":"fiber","v":1,"m":"{}","p":"{}","n":"{}"}}"#,
            self.mac_address, self.pin, self.device_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qr_generator_creation() {
        let generator = QrCodeGenerator::new(
            "AA:BB:CC:DD:EE:FF".to_string(),
            "123456".to_string(),
            "FIBER-001".to_string(),
        ).expect("Failed to create QR generator");

        assert_eq!(generator.get_mac_address(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(generator.get_pin(), "123456");
        assert_eq!(generator.get_device_name(), "FIBER-001");
        assert_eq!(
            generator.get_content(),
            r#"{"t":"fiber","v":1,"m":"AA:BB:CC:DD:EE:FF","p":"123456","n":"FIBER-001"}"#
        );
    }

    #[test]
    fn test_qr_matrix_is_square() {
        let generator = QrCodeGenerator::new(
            "11:22:33:44:55:66".to_string(),
            "999999".to_string(),
            "TEST-DEVICE".to_string(),
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
