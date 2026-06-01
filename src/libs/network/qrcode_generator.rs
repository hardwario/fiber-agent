use anyhow::Result;
use qrcode::QrCode;

/// QR Code generator for Bluetooth/WiFi configuration.
///
/// Emits a v:2 payload carrying an ephemeral provisioning token + expiry.
/// Photographing an old QR no longer compromises BLE access — the BLE side
/// rejects tokens once the provisioning session ends or expires.
///
/// Payload:
/// {"t":"fiber","v":2,"m":"AA:BB:CC:DD:EE:FF","tok":"ABC123","n":"FIBER-001","exp":1735689600}
pub struct QrCodeGenerator {
    mac_address: String,
    token: String,
    expires_at_unix: u64,
    device_name: String,
    qr_matrix: Vec<Vec<bool>>,
}

impl QrCodeGenerator {
    /// Create a new QR code generator.
    ///
    /// # Arguments
    /// * `mac_address`     - Bluetooth MAC (e.g., "AA:BB:CC:DD:EE:FF")
    /// * `token`           - Ephemeral provisioning token (e.g., 6-char A-Z0-9)
    /// * `expires_at_unix` - Token expiry as a Unix timestamp (seconds)
    /// * `device_name`     - Device hostname (e.g., "FIBER-001")
    pub fn new(
        mac_address: String,
        token: String,
        expires_at_unix: u64,
        device_name: String,
    ) -> Result<Self> {
        let qr_content = Self::build_payload(&mac_address, &token, expires_at_unix, &device_name);
        let code = QrCode::new(&qr_content)?;
        let qr_matrix = Self::qr_to_matrix(&code);

        Ok(Self {
            mac_address,
            token,
            expires_at_unix,
            device_name,
            qr_matrix,
        })
    }

    fn build_payload(mac: &str, tok: &str, exp: u64, name: &str) -> String {
        format!(
            r#"{{"t":"fiber","v":2,"m":"{}","tok":"{}","n":"{}","exp":{}}}"#,
            mac, tok, name, exp
        )
    }

    fn qr_to_matrix(code: &QrCode) -> Vec<Vec<bool>> {
        let rendered = code.render::<char>()
            .quiet_zone(false)
            .build();

        let mut matrix = Vec::new();
        for line in rendered.lines() {
            let row: Vec<bool> = line.chars()
                .map(|c| c == '█')
                .collect();
            matrix.push(row);
        }
        matrix
    }

    /// Get the QR matrix as a 2D boolean array (true = black pixel).
    pub fn get_qr_matrix(&self) -> &Vec<Vec<bool>> {
        &self.qr_matrix
    }

    /// QR side length in modules (always square).
    pub fn get_qr_dimension(&self) -> usize {
        if self.qr_matrix.is_empty() { 0 } else { self.qr_matrix.len() }
    }

    pub fn get_mac_address(&self) -> &str {
        &self.mac_address
    }

    pub fn get_token(&self) -> &str {
        &self.token
    }

    pub fn get_expires_at_unix(&self) -> u64 {
        self.expires_at_unix
    }

    pub fn get_device_name(&self) -> &str {
        &self.device_name
    }

    /// Full QR content string (JSON, v:2).
    pub fn get_content(&self) -> String {
        Self::build_payload(
            &self.mac_address,
            &self.token,
            self.expires_at_unix,
            &self.device_name,
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
            "ABC123".to_string(),
            1_735_689_600,
            "FIBER-001".to_string(),
        ).expect("Failed to create QR generator");

        assert_eq!(generator.get_mac_address(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(generator.get_token(), "ABC123");
        assert_eq!(generator.get_expires_at_unix(), 1_735_689_600);
        assert_eq!(generator.get_device_name(), "FIBER-001");
        assert_eq!(
            generator.get_content(),
            r#"{"t":"fiber","v":2,"m":"AA:BB:CC:DD:EE:FF","tok":"ABC123","n":"FIBER-001","exp":1735689600}"#
        );
    }

    #[test]
    fn test_qr_matrix_is_square() {
        let generator = QrCodeGenerator::new(
            "11:22:33:44:55:66".to_string(),
            "ZZZ999".to_string(),
            42,
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

    #[test]
    fn test_payload_is_valid_json_shape() {
        let content = QrCodeGenerator::build_payload("AA:BB", "TOK001", 1700000000, "N");
        // schema bumped to v:2 — guard against accidental regression to v:1
        assert!(content.contains(r#""v":2"#));
        assert!(content.contains(r#""tok":"TOK001""#));
        assert!(content.contains(r#""exp":1700000000"#));
        assert!(!content.contains(r#""p":"#));
    }
}
