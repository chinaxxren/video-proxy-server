//! Optional, compliance-first P2P integration boundary.
//!
//! Core does not perform peer discovery or accept magnet/torrent inputs. A Host
//! may provide bytes only after creating a validated descriptor that proves the
//! application made an explicit authorization decision.

use crate::utils::digest::sha256_hex;
use crate::utils::error::{ProxyError, Result};
use std::sync::Arc;

const SHA256_HEX_LENGTH: usize = 64;
const MAX_CONTENT_ID_LENGTH: usize = 512;
const MAX_AUTHORIZATION_LENGTH: usize = 2048;
const MAX_P2P_READ_BYTES: u64 = 8 * 1024 * 1024;

pub type P2pPieceProvider = Arc<dyn Fn(usize) -> Result<Vec<u8>> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedP2pSource {
    content_id: String,
    content_length: u64,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2pPieceManifest {
    content_length: u64,
    piece_length: u64,
    piece_sha256: Vec<String>,
}

impl P2pPieceManifest {
    pub fn new(content_length: u64, piece_length: u64, piece_sha256: Vec<String>) -> Result<Self> {
        if content_length == 0 || piece_length == 0 {
            return Err(ProxyError::Request(
                "P2P piece lengths must be positive".to_string(),
            ));
        }
        let count = content_length
            .checked_add(piece_length - 1)
            .ok_or_else(|| ProxyError::Request("P2P piece count overflow".to_string()))?
            / piece_length;
        if usize::try_from(count).ok() != Some(piece_sha256.len()) {
            return Err(ProxyError::Request(
                "P2P piece digest count does not match content length".to_string(),
            ));
        }
        let piece_sha256 = piece_sha256
            .into_iter()
            .map(|digest| validate_digest(&digest))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            content_length,
            piece_length,
            piece_sha256,
        })
    }

    pub fn verify_piece(&self, index: usize, bytes: &[u8]) -> Result<()> {
        let digest = self
            .piece_sha256
            .get(index)
            .ok_or_else(|| ProxyError::Request("P2P piece index is out of bounds".to_string()))?;
        let start = (index as u64)
            .checked_mul(self.piece_length)
            .ok_or_else(|| ProxyError::Request("P2P piece offset overflow".to_string()))?;
        let expected_length = self
            .piece_length
            .min(self.content_length.saturating_sub(start));
        if bytes.len() as u64 != expected_length {
            return Err(ProxyError::Request(
                "P2P piece length does not match manifest".to_string(),
            ));
        }
        if sha256_hex(bytes) != *digest {
            return Err(ProxyError::Request(
                "P2P piece integrity check failed".to_string(),
            ));
        }
        Ok(())
    }

    pub fn read_verified_range(
        &self,
        start: u64,
        end: u64,
        provider: &P2pPieceProvider,
    ) -> Result<Vec<u8>> {
        if start > end || end >= self.content_length {
            return Err(ProxyError::InvalidRange(
                "P2P range is outside authorized content".to_string(),
            ));
        }
        let output_length = end
            .checked_sub(start)
            .and_then(|length| length.checked_add(1))
            .ok_or_else(|| ProxyError::InvalidRange("P2P range length overflow".to_string()))?;
        if output_length > MAX_P2P_READ_BYTES {
            return Err(ProxyError::InvalidRange(
                "P2P range exceeds read limit".to_string(),
            ));
        }
        let first_piece = start / self.piece_length;
        let last_piece = end / self.piece_length;
        let mut output = Vec::with_capacity(output_length as usize);
        for piece_number in first_piece..=last_piece {
            let index = usize::try_from(piece_number)
                .map_err(|_| ProxyError::Request("P2P piece index overflow".to_string()))?;
            let piece = provider(index)?;
            self.verify_piece(index, &piece)?;
            let piece_start = piece_number * self.piece_length;
            let copy_start = start.saturating_sub(piece_start) as usize;
            let copy_end = (end - piece_start + 1).min(piece.len() as u64) as usize;
            output.extend_from_slice(&piece[copy_start..copy_end]);
        }
        if output.len() as u64 != output_length {
            return Err(ProxyError::Storage(
                "P2P verified range length mismatch".to_string(),
            ));
        }
        Ok(output)
    }
}

impl AuthorizedP2pSource {
    pub fn new(
        content_id: &str,
        content_length: u64,
        sha256: &str,
        authorization_reference: &str,
        explicitly_authorized: bool,
    ) -> Result<Self> {
        if !explicitly_authorized {
            return Err(ProxyError::Request(
                "P2P source is not explicitly authorized".to_string(),
            ));
        }
        let content_id = validate_text(content_id, MAX_CONTENT_ID_LENGTH, "P2P content ID")?;
        let authorization_reference = validate_text(
            authorization_reference,
            MAX_AUTHORIZATION_LENGTH,
            "P2P authorization reference",
        )?;
        if authorization_reference.starts_with("magnet:") {
            return Err(ProxyError::Request(
                "Magnet links are not accepted by the P2P core".to_string(),
            ));
        }
        if content_length == 0 {
            return Err(ProxyError::Request(
                "P2P content length must be positive".to_string(),
            ));
        }
        let sha256 = validate_digest(sha256)?;
        Ok(Self {
            content_id,
            content_length,
            sha256,
        })
    }

    pub fn content_id(&self) -> &str {
        &self.content_id
    }

    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

fn validate_digest(value: &str) -> Result<String> {
    let value = value.trim();
    if value.len() != SHA256_HEX_LENGTH || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProxyError::Request(
            "P2P SHA-256 digest is invalid".to_string(),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_text(value: &str, max_length: usize, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > max_length
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(ProxyError::Request(format!("{label} is invalid")));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn accepts_explicitly_authorized_fixed_content() {
        let source = AuthorizedP2pSource::new("asset-1", 1024, DIGEST, "license-42", true).unwrap();
        assert_eq!(source.content_id(), "asset-1");
        assert_eq!(source.content_length(), 1024);
        assert_eq!(source.sha256(), DIGEST);
    }

    #[test]
    fn rejects_unauthorized_magnet_and_unverifiable_content() {
        assert!(AuthorizedP2pSource::new("asset", 1, DIGEST, "license", false).is_err());
        assert!(
            AuthorizedP2pSource::new("asset", 1, DIGEST, "magnet:?xt=urn:btih:x", true).is_err()
        );
        assert!(AuthorizedP2pSource::new("asset", 0, DIGEST, "license", true).is_err());
        assert!(AuthorizedP2pSource::new("asset", 1, "bad", "license", true).is_err());
    }

    #[test]
    fn verifies_each_piece_and_short_final_piece() {
        let first = sha256_hex(b"abcd");
        let second = sha256_hex(b"ef");
        let manifest = P2pPieceManifest::new(6, 4, vec![first, second]).unwrap();
        assert!(manifest.verify_piece(0, b"abcd").is_ok());
        assert!(manifest.verify_piece(1, b"ef").is_ok());
        assert!(manifest.verify_piece(1, b"efgh").is_err());
        assert!(manifest.verify_piece(2, b"").is_err());
        assert!(manifest.verify_piece(0, b"abce").is_err());
    }

    #[test]
    fn rejects_inconsistent_piece_manifests() {
        assert!(P2pPieceManifest::new(0, 4, vec![]).is_err());
        assert!(P2pPieceManifest::new(8, 0, vec![]).is_err());
        assert!(P2pPieceManifest::new(8, 4, vec![DIGEST.to_string()]).is_err());
        assert!(P2pPieceManifest::new(4, 4, vec!["bad".to_string()]).is_err());
    }

    #[test]
    fn assembles_verified_ranges_across_piece_boundaries() {
        let pieces = [b"abcd".to_vec(), b"efgh".to_vec(), b"ij".to_vec()];
        let digests = pieces.iter().map(|piece| sha256_hex(piece)).collect();
        let manifest = P2pPieceManifest::new(10, 4, digests).unwrap();
        let provider_pieces = pieces.clone();
        let provider: P2pPieceProvider = Arc::new(move |index| {
            provider_pieces
                .get(index)
                .cloned()
                .ok_or_else(|| ProxyError::Request("missing piece".to_string()))
        });
        assert_eq!(
            manifest.read_verified_range(2, 8, &provider).unwrap(),
            b"cdefghi"
        );
        assert_eq!(manifest.read_verified_range(9, 9, &provider).unwrap(), b"j");
        assert!(manifest.read_verified_range(9, 10, &provider).is_err());
    }

    #[test]
    fn corrupt_provider_bytes_are_never_returned() {
        let manifest = P2pPieceManifest::new(4, 4, vec![sha256_hex(b"good")]).unwrap();
        let provider: P2pPieceProvider = Arc::new(|_| Ok(b"evil".to_vec()));
        assert!(manifest.read_verified_range(0, 3, &provider).is_err());
    }
}
