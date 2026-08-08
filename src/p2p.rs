//! Optional, compliance-first P2P integration boundary.
//!
//! Core does not perform peer discovery or accept magnet/torrent inputs. A Host
//! may provide bytes only after creating a validated descriptor that proves the
//! application made an explicit authorization decision.

use crate::utils::error::{ProxyError, Result};

const SHA256_HEX_LENGTH: usize = 64;
const MAX_CONTENT_ID_LENGTH: usize = 512;
const MAX_AUTHORIZATION_LENGTH: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedP2pSource {
    content_id: String,
    content_length: u64,
    sha256: String,
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
        let sha256 = sha256.trim();
        if sha256.len() != SHA256_HEX_LENGTH || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ProxyError::Request(
                "P2P SHA-256 digest is invalid".to_string(),
            ));
        }
        Ok(Self {
            content_id,
            content_length,
            sha256: sha256.to_ascii_lowercase(),
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
}
