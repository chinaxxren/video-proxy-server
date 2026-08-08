use sha2::{Digest, Sha256};

pub fn sha256_first_byte(input: &[u8]) -> u8 {
    Sha256::digest(input)[0]
}

pub fn sha256_hex(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(input);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard_test_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_first_byte(b"abc"), 0xba);
    }

    #[test]
    fn hex_output_is_fixed_width() {
        assert_eq!(sha256_hex(b"").len(), 64);
        assert_eq!(sha256_hex(&[0; 1024]).len(), 64);
    }
}
