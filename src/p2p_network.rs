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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerResponse {
    pub interval_secs: u64,
    pub min_interval_secs: Option<u64>,
    pub peers: Vec<std::net::SocketAddr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerAnnounce {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub numwant: Option<u16>,
}

pub fn build_tracker_announce_url(tracker: &Url, request: &TrackerAnnounce) -> Result<Url> {
    if !matches!(tracker.scheme(), "http" | "https") || tracker.host().is_none() {
        return Err(ProxyError::Parse("HTTP tracker URL is required".into()));
    }
    let mut url = tracker.clone();
    let mut query = url.query().unwrap_or_default().to_string();
    if !query.is_empty() {
        query.push('&');
    }
    query.push_str("info_hash=");
    query.push_str(&percent_encode_bytes(&request.info_hash));
    query.push_str("&peer_id=");
    query.push_str(&percent_encode_bytes(&request.peer_id));
    query.push_str(&format!(
        "&port={}&uploaded={}&downloaded={}&left={}&compact=1",
        request.port, request.uploaded, request.downloaded, request.left
    ));
    if let Some(numwant) = request.numwant {
        query.push_str(&format!("&numwant={numwant}"));
    }
    url.set_query(Some(&query));
    Ok(url)
}

fn percent_encode_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        output.push('%');
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitTorrentHandshake {
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerMessage {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        index: u32,
        begin: u32,
        block: Vec<u8>,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    Port(u16),
}

const MAX_PEER_MESSAGE_BYTES: usize = 1024 * 1024;

pub fn encode_handshake(handshake: &BitTorrentHandshake) -> [u8; 68] {
    let mut output = [0u8; 68];
    output[0] = 19;
    output[1..20].copy_from_slice(b"BitTorrent protocol");
    output[20..28].copy_from_slice(&handshake.reserved);
    output[28..48].copy_from_slice(&handshake.info_hash);
    output[48..68].copy_from_slice(&handshake.peer_id);
    output
}

pub fn parse_handshake(input: &[u8]) -> Result<BitTorrentHandshake> {
    if input.len() != 68 || input[0] != 19 || &input[1..20] != b"BitTorrent protocol" {
        return Err(ProxyError::Parse("invalid BitTorrent handshake".into()));
    }
    Ok(BitTorrentHandshake {
        reserved: input[20..28].try_into().unwrap(),
        info_hash: input[28..48].try_into().unwrap(),
        peer_id: input[48..68].try_into().unwrap(),
    })
}

pub fn parse_peer_message(input: &[u8]) -> Result<PeerMessage> {
    if input.len() < 4 {
        return Err(ProxyError::Parse("truncated peer message length".into()));
    }
    let length = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
    if length > MAX_PEER_MESSAGE_BYTES || input.len() != length + 4 {
        return Err(ProxyError::Parse("invalid peer message length".into()));
    }
    if length == 0 {
        return Ok(PeerMessage::KeepAlive);
    }
    let payload = &input[4..];
    let id = payload[0];
    let body = &payload[1..];
    let u32_at = |bytes: &[u8]| -> Result<u32> {
        if bytes.len() != 4 {
            return Err(ProxyError::Parse("invalid peer message fields".into()));
        }
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    };
    match id {
        0 if body.is_empty() => Ok(PeerMessage::Choke),
        1 if body.is_empty() => Ok(PeerMessage::Unchoke),
        2 if body.is_empty() => Ok(PeerMessage::Interested),
        3 if body.is_empty() => Ok(PeerMessage::NotInterested),
        4 => Ok(PeerMessage::Have(u32_at(body)?)),
        5 => Ok(PeerMessage::Bitfield(body.to_vec())),
        6 if body.len() == 12 => Ok(PeerMessage::Request {
            index: u32_at(&body[..4])?,
            begin: u32_at(&body[4..8])?,
            length: u32_at(&body[8..])?,
        }),
        7 if body.len() >= 8 => Ok(PeerMessage::Piece {
            index: u32_at(&body[..4])?,
            begin: u32_at(&body[4..8])?,
            block: body[8..].to_vec(),
        }),
        8 if body.len() == 12 => Ok(PeerMessage::Cancel {
            index: u32_at(&body[..4])?,
            begin: u32_at(&body[4..8])?,
            length: u32_at(&body[8..])?,
        }),
        9 if body.len() == 2 => Ok(PeerMessage::Port(u16::from_be_bytes(
            body.try_into().unwrap(),
        ))),
        _ => Err(ProxyError::Parse(
            "unsupported or malformed peer message".into(),
        )),
    }
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

/// Parses a bencoded HTTP/UDP tracker response without allocating arbitrary
/// nested structures. Compact IPv4 and IPv6 peer lists are supported.
pub fn parse_tracker_response(input: &[u8]) -> Result<TrackerResponse> {
    let mut parser = BencodeParser {
        input,
        offset: 0,
        depth: 0,
    };
    let root = parser.value()?;
    if parser.offset != input.len() {
        return Err(ProxyError::Parse(
            "tracker response has trailing bytes".into(),
        ));
    }
    let BValue::Dict(entries) = root else {
        return Err(ProxyError::Parse(
            "tracker response must be a dictionary".into(),
        ));
    };
    if let Some(BValue::Bytes(reason)) = entries.get(b"failure reason" as &[u8]) {
        return Err(ProxyError::Request(
            String::from_utf8_lossy(reason).into_owned(),
        ));
    }
    let interval_secs = entries
        .get(b"interval" as &[u8])
        .and_then(BValue::integer)
        .filter(|value| *value > 0)
        .ok_or_else(|| ProxyError::Parse("tracker response has invalid interval".into()))?;
    let min_interval_secs = entries
        .get(b"min interval" as &[u8])
        .and_then(BValue::integer)
        .filter(|value| *value > 0);
    let mut peers = Vec::new();
    if let Some(BValue::Bytes(compact)) = entries.get(b"peers" as &[u8]) {
        if compact.len() % 6 != 0 {
            return Err(ProxyError::Parse("invalid compact IPv4 peers".into()));
        }
        for chunk in compact.chunks_exact(6) {
            peers.push(std::net::SocketAddr::from((
                std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]),
                u16::from_be_bytes([chunk[4], chunk[5]]),
            )));
        }
    }
    if let Some(BValue::Bytes(compact)) = entries.get(b"peers6" as &[u8]) {
        if compact.len() % 18 != 0 {
            return Err(ProxyError::Parse("invalid compact IPv6 peers".into()));
        }
        for chunk in compact.chunks_exact(18) {
            let mut address = [0u8; 16];
            address.copy_from_slice(&chunk[..16]);
            peers.push(std::net::SocketAddr::from((
                std::net::Ipv6Addr::from(address),
                u16::from_be_bytes([chunk[16], chunk[17]]),
            )));
        }
    }
    Ok(TrackerResponse {
        interval_secs,
        min_interval_secs,
        peers,
    })
}

#[derive(Debug)]
enum BValue {
    Integer(i64),
    Bytes(Vec<u8>),
    Dict(std::collections::BTreeMap<Vec<u8>, BValue>),
    List,
}

impl BValue {
    fn integer(&self) -> Option<u64> {
        match self {
            Self::Integer(value) => (*value).try_into().ok(),
            _ => None,
        }
    }
}

struct BencodeParser<'a> {
    input: &'a [u8],
    offset: usize,
    depth: usize,
}

impl<'a> BencodeParser<'a> {
    fn value(&mut self) -> Result<BValue> {
        if self.depth >= 32 {
            return Err(ProxyError::Parse(
                "tracker response nesting is too deep".into(),
            ));
        }
        self.depth += 1;
        let result = match self.input.get(self.offset).copied() {
            Some(b'i') => self.integer(),
            Some(b'l') => self.list(),
            Some(b'd') => self.dict(),
            Some(b'0'..=b'9') => self.bytes(),
            _ => Err(ProxyError::Parse("invalid bencode value".into())),
        };
        self.depth -= 1;
        result
    }

    fn integer(&mut self) -> Result<BValue> {
        self.offset += 1;
        let end = self.input[self.offset..]
            .iter()
            .position(|byte| *byte == b'e')
            .ok_or_else(|| ProxyError::Parse("unterminated bencode integer".into()))?
            + self.offset;
        let value = std::str::from_utf8(&self.input[self.offset..end])
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| ProxyError::Parse("invalid bencode integer".into()))?;
        self.offset = end + 1;
        Ok(BValue::Integer(value))
    }

    fn bytes(&mut self) -> Result<BValue> {
        let colon = self.input[self.offset..]
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(|| ProxyError::Parse("invalid bencode byte string".into()))?
            + self.offset;
        let length: usize = std::str::from_utf8(&self.input[self.offset..colon])
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| ProxyError::Parse("invalid bencode byte length".into()))?;
        self.offset = colon + 1;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| ProxyError::Parse("bencode byte string is too large".into()))?;
        let bytes = self
            .input
            .get(self.offset..end)
            .ok_or_else(|| ProxyError::Parse("truncated bencode byte string".into()))?
            .to_vec();
        self.offset = end;
        Ok(BValue::Bytes(bytes))
    }

    fn list(&mut self) -> Result<BValue> {
        self.offset += 1;
        while self.input.get(self.offset) != Some(&b'e') {
            self.value()?;
        }
        self.offset += 1;
        Ok(BValue::List)
    }

    fn dict(&mut self) -> Result<BValue> {
        self.offset += 1;
        let mut values = std::collections::BTreeMap::new();
        while self.input.get(self.offset) != Some(&b'e') {
            let BValue::Bytes(key) = self.bytes()? else {
                unreachable!()
            };
            values.insert(key, self.value()?);
        }
        self.offset += 1;
        Ok(BValue::Dict(values))
    }
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

    #[test]
    fn parses_compact_tracker_peers() {
        let response =
            parse_tracker_response(b"d8:intervali30e5:peers6:\x7f\x00\x00\x01\x1a\xe1e").unwrap();
        assert_eq!(response.interval_secs, 30);
        assert_eq!(response.peers, vec!["127.0.0.1:6881".parse().unwrap()]);
    }

    #[test]
    fn rejects_tracker_failure_and_malformed_peers() {
        assert!(parse_tracker_response(b"d14:failure reason4:faile").is_err());
        assert!(parse_tracker_response(b"d8:intervali30e5:peers2:xxe").is_err());
    }

    #[test]
    fn round_trips_handshake_and_peer_messages() {
        let handshake = BitTorrentHandshake {
            reserved: [1; 8],
            info_hash: [2; 20],
            peer_id: [3; 20],
        };
        assert_eq!(
            parse_handshake(&encode_handshake(&handshake)).unwrap(),
            handshake
        );
        assert_eq!(
            parse_peer_message(b"\0\0\0\0").unwrap(),
            PeerMessage::KeepAlive
        );
        assert_eq!(
            parse_peer_message(b"\0\0\0\x01\x01").unwrap(),
            PeerMessage::Unchoke
        );
        assert_eq!(
            parse_peer_message(b"\0\0\0\x0d\x06\0\0\0\x01\0\0\0\x02\0\0\0\x03").unwrap(),
            PeerMessage::Request {
                index: 1,
                begin: 2,
                length: 3
            }
        );
    }

    #[test]
    fn rejects_invalid_handshake_and_oversized_peer_message() {
        assert!(parse_handshake(&[0; 68]).is_err());
        assert!(parse_peer_message(&[0x00, 0x20, 0x00, 0x01]).is_err());
    }

    #[test]
    fn builds_binary_safe_tracker_announce_query() {
        let tracker = Url::parse("https://tracker.example/announce").unwrap();
        let request = TrackerAnnounce {
            info_hash: [0x01; 20],
            peer_id: [0xFF; 20],
            port: 6881,
            uploaded: 2,
            downloaded: 3,
            left: 4,
            numwant: Some(30),
        };
        let url = build_tracker_announce_url(&tracker, &request).unwrap();
        let query = url.query().unwrap();
        assert!(query.contains("info_hash=%01%01%01"));
        assert!(query.contains("peer_id=%FF%FF%FF"));
        assert!(query.contains("compact=1"));
        assert!(build_tracker_announce_url(
            &Url::parse("udp://tracker.example:80").unwrap(),
            &request
        )
        .is_err());
    }
}
