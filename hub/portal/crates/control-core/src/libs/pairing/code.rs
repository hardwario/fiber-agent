//! Pairing code generation
//!
//! Generates 6-character alphanumeric pairing codes excluding ambiguous characters.

use rand::Rng;

/// Character set for pairing codes (excludes: 0, O, 1, I, L)
const CHARSET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// Length of generated pairing codes
pub const CODE_LENGTH: usize = 6;

/// Generate a random 6-character pairing code
///
/// Uses cryptographically secure random number generation.
/// Excludes ambiguous characters (0, O, 1, I, L) to prevent user confusion.
pub fn generate_pairing_code() -> String {
    let mut rng = rand::thread_rng();
    (0..CODE_LENGTH)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

/// Validate that a string is a valid pairing code format
pub fn validate_pairing_code(code: &str) -> bool {
    if code.len() != CODE_LENGTH {
        return false;
    }

    code.chars().all(|c| CHARSET.contains(&(c as u8)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_length() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), CODE_LENGTH);
    }

    #[test]
    fn test_code_excludes_ambiguous_chars() {
        // Generate many codes to statistically verify exclusions
        for _ in 0..1000 {
            let code = generate_pairing_code();
            assert!(!code.contains('0'), "Code contains '0': {}", code);
            assert!(!code.contains('O'), "Code contains 'O': {}", code);
            assert!(!code.contains('1'), "Code contains '1': {}", code);
            assert!(!code.contains('I'), "Code contains 'I': {}", code);
            assert!(!code.contains('L'), "Code contains 'L': {}", code);
        }
    }

    #[test]
    fn test_code_only_valid_chars() {
        for _ in 0..100 {
            let code = generate_pairing_code();
            assert!(validate_pairing_code(&code), "Invalid code: {}", code);
        }
    }

    #[test]
    fn test_validate_valid_codes() {
        assert!(validate_pairing_code("ABC234"));
        assert!(validate_pairing_code("XY7K9M"));
        assert!(validate_pairing_code("HJKMNP"));
    }

    #[test]
    fn test_validate_invalid_codes() {
        // Wrong length
        assert!(!validate_pairing_code("ABC"));
        assert!(!validate_pairing_code("ABC2345"));

        // Contains ambiguous chars
        assert!(!validate_pairing_code("ABC0DE")); // Contains 0
        assert!(!validate_pairing_code("ABCODE")); // Contains O
        assert!(!validate_pairing_code("ABC1DE")); // Contains 1
        assert!(!validate_pairing_code("ABCIDE")); // Contains I
        assert!(!validate_pairing_code("ABCLDE")); // Contains L

        // Contains lowercase
        assert!(!validate_pairing_code("abc234"));
    }

    #[test]
    fn test_codes_are_unique() {
        use std::collections::HashSet;
        let codes: HashSet<String> = (0..100).map(|_| generate_pairing_code()).collect();
        // With 30^6 = 729 million combinations, 100 codes should all be unique
        assert_eq!(codes.len(), 100);
    }
}
