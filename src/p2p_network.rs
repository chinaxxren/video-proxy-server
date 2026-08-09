//! Strict, network-free BitTorrent magnet metadata parsing.
//!
//! This module deliberately stops at parsing. It does not contact trackers,
//! perform DHT lookups, connect to peers, download, upload, or seed content.

use crate::utils::error::{ProxyError, Result};
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagnetRequest {
    pub info_hash: [u8; 20],
    pub display_name: Option<String>,
    pub trackers: Vec<Url>,
}

pub fn parse_magnet_uri(input: &str) -> Result<MagnetRequest> {
    let url = Url::parse(input).map_err(|_| ProxyError::Parse("invalid magnet URI".into()))?;
    if url.scheme() != "magnet" || url.host().is_some() || url.path() != "" {
        return Err(ProxyError::Parse(
            "magnet URI must not contain a host or path".into(),
        ));
    }

    let mut info_hash = None;
    let mut display_name = None;
    let mut trackers = Vec::new();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "xt" if value.starts_with("urn:btih:") => {
                if info_hash.is_some() {
                    return Err(ProxyError::Parse(
                        "magnet URI contains duplicate info hash".into(),
                    ));
                }
                info_hash = Some(parse_info_hash(&value[9..])?);
            }
            "dn" if display_name.is_none() && !value.is_empty() => {
                display_name = Some(value.into_owned());
            }
            "tr" => {
                let tracker = Url::parse(&value)
                    .map_err(|_| ProxyError::Parse("magnet URI contains invalid tracker".into()))?;
                if !matches!(tracker.scheme(), "http" | "https" | "udp") || tracker.host().is_none()
                {
                    return Err(ProxyError::Parse(
                        "magnet tracker must use http, https, or udp".into(),
                    ));
                }
                if !trackers.contains(&tracker) {
                    trackers.push(tracker);
                }
            }
            _ => {}
        }
    }
    let info_hash =
        info_hash.ok_or_else(|| ProxyError::Parse("magnet URI is missing btih".into()))?;
    Ok(MagnetRequest {
        info_hash,
        display_name,
        trackers,
    })
}

fn parse_info_hash(value: &str) -> Result<[u8; 20]> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let mut hash = [0u8; 20];
        for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
            hash[index] = (hex_nibble(chunk[0]) << 4) | hex_nibble(chunk[1]);
        }
        return Ok(hash);
    }
    if value.len() == 32 {
        let decoded = base32_decode(value)?;
        let hash: [u8; 20] = decoded
            .try_into()
            .map_err(|_| ProxyError::Parse("invalid base32 btih".into()))?;
        return Ok(hash);
    }
    Err(ProxyError::Parse(
        "btih must be 40 hex or 32 base32 characters".into(),
    ))
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => unreachable!(),
    }
}

fn base32_decode(input: &str) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(20);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in input.bytes() {
        let value = match byte.to_ascii_uppercase() {
            b'A'..=b'Z' => byte.to_ascii_uppercase() - b'A',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => return Err(ProxyError::Parse("invalid base32 btih".into())),
        } as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    if bits > 0 && buffer != 0 {
        return Err(ProxyError::Parse("non-zero base32 padding".into()));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_magnet_and_deduplicates_trackers() {
        let request = parse_magnet_uri("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=sample%20file&tr=https%3A%2F%2Ftracker.example%2Fannounce&tr=https%3A%2F%2Ftracker.example%2Fannounce").unwrap();
        assert_eq!(request.info_hash[0], 0x01);
        assert_eq!(request.display_name.as_deref(), Some("sample file"));
        assert_eq!(request.trackers.len(), 1);
    }

    #[test]
    fn parses_base32_magnet() {
        let request =
            parse_magnet_uri("magnet:?xt=urn:btih:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        assert_eq!(request.info_hash, [0; 20]);
    }

    #[test]
    fn rejects_unsafe_or_incomplete_magnets() {
        for value in [
            "https://example.invalid/?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            "magnet:?dn=missing-hash",
            "magnet:?xt=urn:btih:short&tr=file:///tmp/tracker",
        ] {
            assert!(parse_magnet_uri(value).is_err(), "accepted {value}");
        }
    }
}
