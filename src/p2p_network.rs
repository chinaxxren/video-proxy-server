//! Strict, network-free BitTorrent magnet metadata parsing.
//!
//! This module deliberately stops at parsing. It does not contact trackers,
//! perform DHT lookups, connect to peers, download, upload, or seed content.

use crate::utils::error::{ProxyError, Result};
use crate::utils::network_policy::NetworkPolicy;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use sha1::{Digest as Sha1Digest, Sha1};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagnetRequest {
    pub info_hash: [u8; 20],
    pub display_name: Option<String>,
    pub trackers: Vec<Url>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentMetadata {
    pub info_hash: [u8; 20],
    pub name: String,
    pub total_length: u64,
    pub piece_length: u32,
    pub piece_sha1: Vec<[u8; 20]>,
    pub files: Vec<TorrentFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentFile {
    pub path: Vec<String>,
    pub length: u64,
}

#[derive(Clone, Debug)]
pub struct TorrentPieceStore {
    directory: std::path::PathBuf,
}

#[cfg(feature = "p2p-upload")]
#[derive(Clone, Debug)]
pub struct UploadSession {
    authorized: bool,
    max_bytes: u64,
    uploaded: u64,
}

#[cfg(feature = "p2p-upload")]
impl UploadSession {
    pub fn new(authorized: bool, max_bytes: u64) -> Result<Self> {
        if max_bytes == 0 {
            return Err(ProxyError::Request("upload quota must be positive".into()));
        }
        Ok(Self {
            authorized,
            max_bytes,
            uploaded: 0,
        })
    }

    pub fn serve_block(
        &mut self,
        metadata: &TorrentMetadata,
        store: &TorrentPieceStore,
        index: u32,
        begin: u32,
        length: u32,
    ) -> Result<Vec<u8>> {
        if !self.authorized {
            return Err(ProxyError::Request(
                "torrent upload is not authorized".into(),
            ));
        }
        validate_block_request(begin, length)?;
        let piece_length = torrent_piece_length(metadata, index)?;
        let end = begin
            .checked_add(length)
            .ok_or_else(|| ProxyError::Request("upload block range overflow".into()))?;
        if end > piece_length {
            return Err(ProxyError::Request("upload block exceeds piece".into()));
        }
        let next_total = self
            .uploaded
            .checked_add(length as u64)
            .ok_or_else(|| ProxyError::Request("upload quota overflow".into()))?;
        if next_total > self.max_bytes {
            return Err(ProxyError::Request("upload quota exceeded".into()));
        }
        let hash = metadata
            .piece_sha1
            .get(index as usize)
            .ok_or_else(|| ProxyError::Request("upload piece index out of range".into()))?;
        let piece = store
            .load(index, hash)?
            .ok_or_else(|| ProxyError::Request("upload piece is unavailable".into()))?;
        let block = piece
            .get(begin as usize..end as usize)
            .ok_or_else(|| ProxyError::Request("upload block range is invalid".into()))?
            .to_vec();
        self.uploaded = next_total;
        Ok(block)
    }

    pub fn uploaded_bytes(&self) -> u64 {
        self.uploaded
    }
}

impl TorrentPieceStore {
    pub fn new(root: &std::path::Path, info_hash: &[u8; 20]) -> Result<Self> {
        if !root.is_absolute() {
            return Err(ProxyError::Storage(
                "torrent piece root must be absolute".into(),
            ));
        }
        let name = info_hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).map_err(|error| {
            ProxyError::Storage(format!("create torrent piece directory failed: {error}"))
        })?;
        Ok(Self { directory })
    }

    pub fn store(&self, index: u32, bytes: &[u8], expected_sha1: &[u8; 20]) -> Result<()> {
        if bytes.is_empty() || bytes.len() > 8 * 1024 * 1024 {
            return Err(ProxyError::Storage("invalid torrent piece size".into()));
        }
        verify_piece_sha1(bytes, expected_sha1)?;
        let final_path = self.directory.join(format!("{index}.piece"));
        let temp_path = self.directory.join(format!("{index}.piece.tmp"));
        std::fs::write(&temp_path, bytes)
            .map_err(|error| ProxyError::Storage(format!("write torrent piece failed: {error}")))?;
        std::fs::rename(&temp_path, &final_path).map_err(|error| {
            let _ = std::fs::remove_file(&temp_path);
            ProxyError::Storage(format!("commit torrent piece failed: {error}"))
        })
    }

    pub fn load(&self, index: u32, expected_sha1: &[u8; 20]) -> Result<Option<Vec<u8>>> {
        let path = self.directory.join(format!("{index}.piece"));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ProxyError::Storage(format!(
                    "read torrent piece failed: {error}"
                )))
            }
        };
        if bytes.is_empty()
            || bytes.len() > 8 * 1024 * 1024
            || verify_piece_sha1(&bytes, expected_sha1).is_err()
        {
            let _ = std::fs::remove_file(path);
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    pub fn completed(&self, hashes: &[[u8; 20]]) -> Result<Vec<u32>> {
        let mut completed = Vec::new();
        for (index, hash) in hashes.iter().enumerate() {
            if self.load(index as u32, hash)?.is_some() {
                completed.push(index as u32);
            }
        }
        Ok(completed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtendedHandshake {
    pub ut_metadata_id: u8,
    pub metadata_size: usize,
}

pub fn parse_extended_handshake(payload: &[u8]) -> Result<ExtendedHandshake> {
    let mut parser = BencodeParser {
        input: payload,
        offset: 0,
        depth: 0,
    };
    let root = parser.value()?;
    if parser.offset != payload.len() {
        return Err(ProxyError::Parse(
            "extended handshake has trailing bytes".into(),
        ));
    }
    let BValue::Dict(root) = root else {
        return Err(ProxyError::Parse(
            "extended handshake must be a dictionary".into(),
        ));
    };
    let metadata_size = root
        .get(b"metadata_size" as &[u8])
        .and_then(BValue::integer)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0 && *value <= 4 * 1024 * 1024)
        .ok_or_else(|| ProxyError::Parse("invalid metadata_size".into()))?;
    let BValue::Dict(extensions) = root
        .get(b"m" as &[u8])
        .ok_or_else(|| ProxyError::Parse("extended handshake is missing m".into()))?
    else {
        return Err(ProxyError::Parse("extended handshake m is invalid".into()));
    };
    let ut_metadata_id = extensions
        .get(b"ut_metadata" as &[u8])
        .and_then(BValue::integer)
        .and_then(|value| u8::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or_else(|| ProxyError::Parse("extended handshake lacks ut_metadata".into()))?;
    Ok(ExtendedHandshake {
        ut_metadata_id,
        metadata_size,
    })
}

pub fn encode_ut_metadata_request(extension_id: u8, piece: u32) -> Result<PeerMessage> {
    if extension_id == 0 {
        return Err(ProxyError::Request(
            "invalid ut_metadata extension ID".into(),
        ));
    }
    Ok(PeerMessage::Extended {
        extension_id,
        payload: format!("d8:msg_typei0e5:piecei{piece}ee").into_bytes(),
    })
}

pub fn encode_extended_handshake() -> PeerMessage {
    PeerMessage::Extended {
        extension_id: 0,
        payload: b"d1:md11:ut_metadatai1eee".to_vec(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataExtensionResponse {
    Data {
        piece: u32,
        total_size: usize,
        bytes: Vec<u8>,
    },
    Reject {
        piece: u32,
    },
}

pub fn parse_ut_metadata_response(payload: &[u8]) -> Result<MetadataExtensionResponse> {
    let mut parser = BencodeParser {
        input: payload,
        offset: 0,
        depth: 0,
    };
    let header = parser.value()?;
    let BValue::Dict(header) = header else {
        return Err(ProxyError::Parse("ut_metadata header is invalid".into()));
    };
    let message_type = header
        .get(b"msg_type" as &[u8])
        .and_then(BValue::integer)
        .ok_or_else(|| ProxyError::Parse("ut_metadata msg_type is missing".into()))?;
    let piece = header
        .get(b"piece" as &[u8])
        .and_then(BValue::integer)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| ProxyError::Parse("ut_metadata piece is invalid".into()))?;
    match message_type {
        1 => {
            let total_size = header
                .get(b"total_size" as &[u8])
                .and_then(BValue::integer)
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value > 0 && *value <= 4 * 1024 * 1024)
                .ok_or_else(|| ProxyError::Parse("ut_metadata total_size is invalid".into()))?;
            let bytes = payload[parser.offset..].to_vec();
            if bytes.is_empty() || bytes.len() > 16 * 1024 {
                return Err(ProxyError::Parse(
                    "ut_metadata piece length is invalid".into(),
                ));
            }
            Ok(MetadataExtensionResponse::Data {
                piece,
                total_size,
                bytes,
            })
        }
        2 if parser.offset == payload.len() => Ok(MetadataExtensionResponse::Reject { piece }),
        _ => Err(ProxyError::Parse("unsupported ut_metadata response".into())),
    }
}

#[derive(Clone, Debug)]
pub struct MetadataAssembler {
    info_hash: [u8; 20],
    total_size: usize,
    pieces: Vec<Option<Vec<u8>>>,
}

impl MetadataAssembler {
    pub fn new(info_hash: [u8; 20], total_size: usize) -> Result<Self> {
        if total_size == 0 || total_size > 4 * 1024 * 1024 {
            return Err(ProxyError::Request("invalid metadata size".into()));
        }
        Ok(Self {
            info_hash,
            total_size,
            pieces: vec![None; total_size.div_ceil(16 * 1024)],
        })
    }

    pub fn insert(&mut self, response: MetadataExtensionResponse) -> Result<bool> {
        let MetadataExtensionResponse::Data {
            piece,
            total_size,
            bytes,
        } = response
        else {
            return Err(ProxyError::Request("metadata piece was rejected".into()));
        };
        if total_size != self.total_size {
            return Err(ProxyError::Request("metadata total size changed".into()));
        }
        let index = piece as usize;
        let expected = if index + 1 == self.pieces.len() {
            self.total_size - index * 16 * 1024
        } else {
            16 * 1024
        };
        let slot = self
            .pieces
            .get_mut(index)
            .ok_or_else(|| ProxyError::Request("metadata piece index out of range".into()))?;
        if bytes.len() != expected {
            return Err(ProxyError::Request(
                "metadata piece has wrong length".into(),
            ));
        }
        if let Some(existing) = slot {
            if existing != &bytes {
                return Err(ProxyError::Request("metadata piece changed".into()));
            }
        } else {
            *slot = Some(bytes);
        }
        Ok(self.pieces.iter().all(Option::is_some))
    }

    pub fn finish(self) -> Result<Vec<u8>> {
        if self.pieces.iter().any(Option::is_none) {
            return Err(ProxyError::Request("metadata is incomplete".into()));
        }
        let metadata: Vec<u8> = self.pieces.into_iter().flatten().flatten().collect();
        let digest: [u8; 20] = Sha1::digest(&metadata).into();
        if digest != self.info_hash {
            return Err(ProxyError::Request("metadata InfoHash mismatch".into()));
        }
        Ok(metadata)
    }
}

pub fn parse_torrent_metadata(input: &[u8]) -> Result<TorrentMetadata> {
    if input.is_empty() || input.len() > 4 * 1024 * 1024 {
        return Err(ProxyError::Parse("invalid torrent metadata size".into()));
    }
    let mut parser = BencodeParser {
        input,
        offset: 0,
        depth: 0,
    };
    if parser.input.get(parser.offset) != Some(&b'd') {
        return Err(ProxyError::Parse(
            "torrent metadata must be a dictionary".into(),
        ));
    }
    parser.offset += 1;
    let mut info_value = None;
    let mut info_span = None;
    while parser.input.get(parser.offset) != Some(&b'e') {
        let BValue::Bytes(key) = parser.bytes()? else {
            unreachable!()
        };
        let start = parser.offset;
        let value = parser.value()?;
        if key == b"info" {
            if info_value.is_some() {
                return Err(ProxyError::Parse(
                    "duplicate torrent info dictionary".into(),
                ));
            }
            info_span = Some((start, parser.offset));
            info_value = Some(value);
        }
    }
    parser.offset += 1;
    if parser.offset != input.len() {
        return Err(ProxyError::Parse(
            "torrent metadata has trailing bytes".into(),
        ));
    }
    let BValue::Dict(info) =
        info_value.ok_or_else(|| ProxyError::Parse("torrent metadata is missing info".into()))?
    else {
        return Err(ProxyError::Parse("torrent info is not a dictionary".into()));
    };
    let name = match info.get(b"name" as &[u8]) {
        Some(BValue::Bytes(value)) => String::from_utf8(value.clone())
            .map_err(|_| ProxyError::Parse("torrent name is not UTF-8".into()))?,
        _ => return Err(ProxyError::Parse("torrent name is missing".into())),
    };
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return Err(ProxyError::Parse("torrent name is unsafe".into()));
    }
    let files = parse_torrent_files(&info, &name)?;
    let total_length = files
        .iter()
        .try_fold(0u64, |total, file| total.checked_add(file.length))
        .ok_or_else(|| ProxyError::Parse("torrent total length overflow".into()))?;
    let piece_length = info
        .get(b"piece length" as &[u8])
        .and_then(BValue::integer)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0 && *value <= 8 * 1024 * 1024)
        .ok_or_else(|| ProxyError::Parse("torrent piece length is invalid".into()))?;
    let pieces = match info.get(b"pieces" as &[u8]) {
        Some(BValue::Bytes(value)) if !value.is_empty() && value.len() % 20 == 0 => value,
        _ => return Err(ProxyError::Parse("torrent pieces are invalid".into())),
    };
    if pieces.len() / 20 > 1_000_000 {
        return Err(ProxyError::Parse("torrent has too many pieces".into()));
    }
    let expected_count = total_length.div_ceil(piece_length as u64) as usize;
    if pieces.len() / 20 != expected_count {
        return Err(ProxyError::Parse(
            "torrent piece count does not match length".into(),
        ));
    }
    let piece_sha1 = pieces
        .chunks_exact(20)
        .map(|chunk| <[u8; 20]>::try_from(chunk).unwrap())
        .collect();
    let (start, end) = info_span.unwrap();
    let info_hash: [u8; 20] = Sha1::digest(&input[start..end]).into();
    Ok(TorrentMetadata {
        info_hash,
        name,
        total_length,
        piece_length,
        piece_sha1,
        files,
    })
}

fn parse_torrent_files(
    info: &std::collections::BTreeMap<Vec<u8>, BValue>,
    name: &str,
) -> Result<Vec<TorrentFile>> {
    if let Some(length) = info
        .get(b"length" as &[u8])
        .and_then(BValue::integer)
        .filter(|value| *value > 0)
    {
        if info.contains_key(b"files" as &[u8]) {
            return Err(ProxyError::Parse(
                "torrent mixes single and multi-file layouts".into(),
            ));
        }
        return Ok(vec![TorrentFile {
            path: vec![name.to_string()],
            length,
        }]);
    }
    let Some(BValue::List(entries)) = info.get(b"files" as &[u8]) else {
        return Err(ProxyError::Parse("torrent files are missing".into()));
    };
    if entries.is_empty() || entries.len() > 100_000 {
        return Err(ProxyError::Parse("torrent file count is invalid".into()));
    }
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        let BValue::Dict(entry) = entry else {
            return Err(ProxyError::Parse("torrent file entry is invalid".into()));
        };
        let length = entry
            .get(b"length" as &[u8])
            .and_then(BValue::integer)
            .ok_or_else(|| ProxyError::Parse("torrent file length is invalid".into()))?;
        let Some(BValue::List(components)) = entry.get(b"path" as &[u8]) else {
            return Err(ProxyError::Parse("torrent file path is invalid".into()));
        };
        if components.is_empty() || components.len() > 128 {
            return Err(ProxyError::Parse(
                "torrent file path depth is invalid".into(),
            ));
        }
        let mut path = vec![name.to_string()];
        for component in components {
            let BValue::Bytes(component) = component else {
                return Err(ProxyError::Parse(
                    "torrent path component is invalid".into(),
                ));
            };
            let component = String::from_utf8(component.clone())
                .map_err(|_| ProxyError::Parse("torrent path is not UTF-8".into()))?;
            if component.is_empty()
                || component == "."
                || component == ".."
                || component.contains('/')
                || component.contains('\\')
            {
                return Err(ProxyError::Parse("torrent path is unsafe".into()));
            }
            path.push(component);
        }
        files.push(TorrentFile { path, length });
    }
    Ok(files)
}

pub fn parse_info_metadata(info: &[u8]) -> Result<TorrentMetadata> {
    if info.is_empty() || info.len() > 4 * 1024 * 1024 || info.first() != Some(&b'd') {
        return Err(ProxyError::Parse(
            "invalid raw torrent info dictionary".into(),
        ));
    }
    let mut wrapped = Vec::with_capacity(info.len() + 8);
    wrapped.extend_from_slice(b"d4:info");
    wrapped.extend_from_slice(info);
    wrapped.push(b'e');
    parse_torrent_metadata(&wrapped)
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

pub const UDP_TRACKER_PROTOCOL_ID: u64 = 0x0000_0417_2710_1980;
pub const UDP_ACTION_CONNECT: u32 = 0;
pub const UDP_ACTION_ANNOUNCE: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhtNode {
    pub id: [u8; 20],
    pub address: std::net::SocketAddr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhtGetPeersResponse {
    pub token: Vec<u8>,
    pub nodes: Vec<DhtNode>,
    pub peers: Vec<std::net::SocketAddr>,
}

#[derive(Clone, Debug)]
pub struct DhtRoutingTable {
    local_id: [u8; 20],
    nodes: Vec<DhtNode>,
    capacity: usize,
}

impl DhtRoutingTable {
    pub fn new(local_id: [u8; 20], capacity: usize) -> Result<Self> {
        if capacity == 0 || capacity > 4096 {
            return Err(ProxyError::Request("invalid DHT routing capacity".into()));
        }
        Ok(Self {
            local_id,
            nodes: Vec::new(),
            capacity,
        })
    }

    pub fn insert(&mut self, node: DhtNode) -> bool {
        if node.id == self.local_id || !is_public_socket(node.address) {
            return false;
        }
        if let Some(existing) = self
            .nodes
            .iter_mut()
            .find(|existing| existing.id == node.id)
        {
            existing.address = node.address;
            return true;
        }
        if self.nodes.len() == self.capacity {
            let farthest = self
                .nodes
                .iter()
                .enumerate()
                .max_by_key(|(_, node)| xor_distance(&node.id, &self.local_id))
                .map(|(index, _)| index)
                .unwrap();
            if xor_distance(&node.id, &self.local_id)
                >= xor_distance(&self.nodes[farthest].id, &self.local_id)
            {
                return false;
            }
            self.nodes.swap_remove(farthest);
        }
        self.nodes.push(node);
        true
    }

    pub fn closest(&self, target: &[u8; 20], limit: usize) -> Vec<DhtNode> {
        let mut nodes = self.nodes.clone();
        nodes.sort_by_key(|node| xor_distance(&node.id, target));
        nodes.truncate(limit.min(nodes.len()));
        nodes
    }
}

fn xor_distance(left: &[u8; 20], right: &[u8; 20]) -> [u8; 20] {
    let mut output = [0u8; 20];
    for index in 0..20 {
        output[index] = left[index] ^ right[index];
    }
    output
}

fn is_public_socket(address: std::net::SocketAddr) -> bool {
    if address.port() == 0 {
        return false;
    }
    match address.ip() {
        std::net::IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.octets()[0] == 0)
        }
        std::net::IpAddr::V6(ip) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local())
        }
    }
}

pub async fn send_dht_udp_query(target: std::net::SocketAddr, request: &[u8]) -> Result<Vec<u8>> {
    if !is_public_socket(target) {
        return Err(ProxyError::Request(
            "DHT target must be a public address".into(),
        ));
    }
    if request.is_empty() || request.len() > 4096 {
        return Err(ProxyError::Request("invalid DHT request size".into()));
    }
    let bind = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|error| ProxyError::Network(format!("DHT UDP bind failed: {error}")))?;
    socket
        .connect(target)
        .await
        .map_err(|error| ProxyError::Network(format!("DHT UDP connect failed: {error}")))?;
    tokio::time::timeout(Duration::from_secs(3), socket.send(request))
        .await
        .map_err(|_| ProxyError::Network("DHT UDP send timed out".into()))?
        .map_err(|error| ProxyError::Network(format!("DHT UDP send failed: {error}")))?;
    let mut response = vec![0u8; 65_507];
    let length = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut response))
        .await
        .map_err(|_| ProxyError::Network("DHT UDP response timed out".into()))?
        .map_err(|error| ProxyError::Network(format!("DHT UDP receive failed: {error}")))?;
    response.truncate(length);
    Ok(response)
}

pub fn encode_dht_get_peers(
    transaction: &[u8],
    node_id: &[u8; 20],
    info_hash: &[u8; 20],
) -> Result<Vec<u8>> {
    if transaction.is_empty() || transaction.len() > 32 {
        return Err(ProxyError::Parse("invalid DHT transaction ID".into()));
    }
    let mut output = b"d1:ad2:id20:".to_vec();
    output.extend_from_slice(node_id);
    output.extend_from_slice(b"9:info_hash20:");
    output.extend_from_slice(info_hash);
    output.extend_from_slice(b"1:q9:get_peers1:t");
    output.extend_from_slice(transaction.len().to_string().as_bytes());
    output.push(b':');
    output.extend_from_slice(transaction);
    output.extend_from_slice(b"1:y1:qe");
    Ok(output)
}

pub fn parse_dht_get_peers_response(
    input: &[u8],
    transaction: &[u8],
) -> Result<DhtGetPeersResponse> {
    let mut parser = BencodeParser {
        input,
        offset: 0,
        depth: 0,
    };
    let root = parser.value()?;
    if parser.offset != input.len() {
        return Err(ProxyError::Parse("DHT response has trailing bytes".into()));
    }
    let BValue::Dict(entries) = root else {
        return Err(ProxyError::Parse(
            "DHT response must be a dictionary".into(),
        ));
    };
    if entries.get(b"y" as &[u8]).and_then(|v| match v {
        BValue::Bytes(value) => Some(value.as_slice()),
        _ => None,
    }) != Some(b"r")
    {
        return Err(ProxyError::Parse("DHT response is not a response".into()));
    }
    if entries.get(b"t" as &[u8]).and_then(|v| match v {
        BValue::Bytes(value) => Some(value.as_slice()),
        _ => None,
    }) != Some(transaction)
    {
        return Err(ProxyError::Parse("DHT transaction mismatch".into()));
    }
    let BValue::Dict(response) = entries
        .get(b"r" as &[u8])
        .ok_or_else(|| ProxyError::Parse("DHT response is missing r".into()))?
    else {
        return Err(ProxyError::Parse("DHT response r is invalid".into()));
    };
    let token = match response.get(b"token" as &[u8]) {
        Some(BValue::Bytes(value)) if !value.is_empty() && value.len() <= 256 => value.clone(),
        _ => return Err(ProxyError::Parse("DHT response token is invalid".into())),
    };
    let mut nodes = response
        .get(b"nodes" as &[u8])
        .and_then(|value| match value {
            BValue::Bytes(value) => Some(parse_dht_compact_nodes(value)),
            _ => None,
        })
        .transpose()?
        .unwrap_or_default();
    if let Some(BValue::Bytes(value)) = response.get(b"nodes6" as &[u8]) {
        if value.len() % 38 != 0 {
            return Err(ProxyError::Parse("invalid compact IPv6 DHT nodes".into()));
        }
        for chunk in value.chunks_exact(38) {
            nodes.push(DhtNode {
                id: chunk[..20].try_into().unwrap(),
                address: std::net::SocketAddr::from((
                    std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&chunk[20..36]).unwrap()),
                    u16::from_be_bytes([chunk[36], chunk[37]]),
                )),
            });
        }
    }
    let mut peers = Vec::new();
    if let Some(BValue::List(values)) = response.get(b"values" as &[u8]) {
        for value in values {
            let BValue::Bytes(value) = value else {
                return Err(ProxyError::Parse("DHT peer value is invalid".into()));
            };
            if value.len() != 6 {
                return Err(ProxyError::Parse(
                    "DHT peer value is not compact IPv4".into(),
                ));
            }
            peers.push(std::net::SocketAddr::from((
                std::net::Ipv4Addr::new(value[0], value[1], value[2], value[3]),
                u16::from_be_bytes([value[4], value[5]]),
            )));
        }
    }
    Ok(DhtGetPeersResponse {
        token,
        nodes,
        peers,
    })
}

pub fn encode_dht_ping(transaction: &[u8]) -> Result<Vec<u8>> {
    if transaction.is_empty() || transaction.len() > 32 {
        return Err(ProxyError::Parse("invalid DHT transaction ID".into()));
    }
    let mut output = b"d1:ad2:id20:".to_vec();
    output.extend_from_slice(&[0; 20]);
    output.extend_from_slice(b"1:q4:ping1:t");
    output.extend_from_slice(transaction.len().to_string().as_bytes());
    output.push(b':');
    output.extend_from_slice(transaction);
    output.extend_from_slice(b"1:y1:qe");
    Ok(output)
}

pub fn parse_dht_compact_nodes(input: &[u8]) -> Result<Vec<DhtNode>> {
    if input.len() % 26 != 0 {
        return Err(ProxyError::Parse("invalid compact DHT node list".into()));
    }
    let mut nodes = Vec::with_capacity(input.len() / 26);
    for chunk in input.chunks_exact(26) {
        let id = chunk[..20].try_into().unwrap();
        let address = std::net::SocketAddr::from((
            std::net::Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]),
            u16::from_be_bytes([chunk[24], chunk[25]]),
        ));
        nodes.push(DhtNode { id, address });
    }
    Ok(nodes)
}

pub fn encode_udp_connect_request(transaction_id: u32) -> [u8; 16] {
    let mut output = [0u8; 16];
    output[..8].copy_from_slice(&UDP_TRACKER_PROTOCOL_ID.to_be_bytes());
    output[8..12].copy_from_slice(&UDP_ACTION_CONNECT.to_be_bytes());
    output[12..].copy_from_slice(&transaction_id.to_be_bytes());
    output
}

pub fn parse_udp_connect_response(input: &[u8], transaction_id: u32) -> Result<u64> {
    if input.len() != 16 || u32::from_be_bytes(input[..4].try_into().unwrap()) != UDP_ACTION_CONNECT
    {
        return Err(ProxyError::Parse(
            "invalid UDP tracker connect response".into(),
        ));
    }
    if u32::from_be_bytes(input[4..8].try_into().unwrap()) != transaction_id {
        return Err(ProxyError::Parse("UDP tracker transaction mismatch".into()));
    }
    Ok(u64::from_be_bytes(input[8..].try_into().unwrap()))
}

pub fn encode_udp_announce_request(
    connection_id: u64,
    transaction_id: u32,
    request: &TrackerAnnounce,
) -> [u8; 98] {
    let mut output = [0u8; 98];
    output[..8].copy_from_slice(&connection_id.to_be_bytes());
    output[8..12].copy_from_slice(&UDP_ACTION_ANNOUNCE.to_be_bytes());
    output[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    output[16..36].copy_from_slice(&request.info_hash);
    output[36..56].copy_from_slice(&request.peer_id);
    output[56..64].copy_from_slice(&request.downloaded.to_be_bytes());
    output[64..72].copy_from_slice(&request.left.to_be_bytes());
    output[72..80].copy_from_slice(&request.uploaded.to_be_bytes());
    output[80..84].copy_from_slice(&0u32.to_be_bytes());
    output[84..88].copy_from_slice(&0u32.to_be_bytes());
    output[88..92].copy_from_slice(&0u32.to_be_bytes());
    output[92..96].copy_from_slice(&(request.numwant.unwrap_or(-1i16 as u16) as u32).to_be_bytes());
    output[96..98].copy_from_slice(&request.port.to_be_bytes());
    output
}

pub fn parse_udp_announce_response(input: &[u8], transaction_id: u32) -> Result<TrackerResponse> {
    if input.len() < 20 || u32::from_be_bytes(input[..4].try_into().unwrap()) != UDP_ACTION_ANNOUNCE
    {
        return Err(ProxyError::Parse(
            "invalid UDP tracker announce response".into(),
        ));
    }
    if u32::from_be_bytes(input[4..8].try_into().unwrap()) != transaction_id {
        return Err(ProxyError::Parse("UDP tracker transaction mismatch".into()));
    }
    let interval_secs = u32::from_be_bytes(input[8..12].try_into().unwrap()) as u64;
    let mut peers = Vec::new();
    if (input.len() - 20) % 6 != 0 {
        return Err(ProxyError::Parse("invalid UDP tracker peer list".into()));
    }
    for chunk in input[20..].chunks_exact(6) {
        peers.push(std::net::SocketAddr::from((
            std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]),
            u16::from_be_bytes([chunk[4], chunk[5]]),
        )));
    }
    Ok(TrackerResponse {
        interval_secs,
        min_interval_secs: None,
        peers,
    })
}

pub async fn announce_udp_tracker(
    target: std::net::SocketAddr,
    request: &TrackerAnnounce,
    connect_transaction: u32,
    announce_transaction: u32,
) -> Result<TrackerResponse> {
    if !is_public_socket(target) {
        return Err(ProxyError::Request(
            "UDP tracker must be a public address".into(),
        ));
    }
    let bind = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|error| ProxyError::Network(format!("UDP tracker bind failed: {error}")))?;
    socket
        .connect(target)
        .await
        .map_err(|error| ProxyError::Network(format!("UDP tracker connect failed: {error}")))?;
    let exchange = async {
        socket
            .send(&encode_udp_connect_request(connect_transaction))
            .await
            .map_err(|error| {
                ProxyError::Network(format!("UDP tracker connect send failed: {error}"))
            })?;
        let mut response = [0u8; 65_507];
        let length = socket.recv(&mut response).await.map_err(|error| {
            ProxyError::Network(format!("UDP tracker connect receive failed: {error}"))
        })?;
        let connection_id = parse_udp_connect_response(&response[..length], connect_transaction)?;
        socket
            .send(&encode_udp_announce_request(
                connection_id,
                announce_transaction,
                request,
            ))
            .await
            .map_err(|error| {
                ProxyError::Network(format!("UDP tracker announce send failed: {error}"))
            })?;
        let length = socket.recv(&mut response).await.map_err(|error| {
            ProxyError::Network(format!("UDP tracker announce receive failed: {error}"))
        })?;
        parse_udp_announce_response(&response[..length], announce_transaction)
    };
    tokio::time::timeout(Duration::from_secs(6), exchange)
        .await
        .map_err(|_| ProxyError::Network("UDP tracker exchange timed out".into()))?
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

/// Announces to an HTTP(S) tracker using the Core's public-only shared client.
/// UDP trackers are intentionally left to a separate transport implementation.
pub async fn announce_http_tracker(
    tracker: &Url,
    request: &TrackerAnnounce,
    policy: &NetworkPolicy,
) -> Result<TrackerResponse> {
    let announce_url = build_tracker_announce_url(tracker, request)?;
    policy.validate(announce_url.as_str()).await?;
    let http_request = Request::get(announce_url.as_str())
        .header("User-Agent", "MediaProxyCache/0.3")
        .body(Full::new(bytes::Bytes::new()))
        .map_err(|_| ProxyError::Request("unable to construct tracker request".into()))?;
    let response = tokio::time::timeout(
        Duration::from_secs(15),
        crate::data_source::net_source::shared_client_v1().request(http_request),
    )
    .await
    .map_err(|_| ProxyError::Network("tracker request timed out".into()))?
    .map_err(|error| ProxyError::Network(format!("tracker request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(ProxyError::Network(format!(
            "tracker returned {}",
            response.status()
        )));
    }
    let mut body = Vec::new();
    let mut stream = response.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| ProxyError::Network(format!("tracker body failed: {error}")))?;
        if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err(ProxyError::Request("tracker response is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    parse_tracker_response(&body)
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
    Extended {
        extension_id: u8,
        payload: Vec<u8>,
    },
}

#[derive(Debug)]
pub struct PeerConnection {
    stream: tokio::net::TcpStream,
    pub remote_handshake: BitTorrentHandshake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PieceState {
    Missing,
    InFlight,
    Complete,
}

#[derive(Clone, Debug)]
pub struct PieceScheduler {
    states: Vec<PieceState>,
    availability: Vec<u32>,
    failures: Vec<u8>,
    max_failures: u8,
    priorities: Vec<u8>,
}

impl PieceScheduler {
    pub fn new(piece_count: usize, max_failures: u8) -> Result<Self> {
        if piece_count == 0 || piece_count > 1_000_000 || max_failures == 0 {
            return Err(ProxyError::Request("invalid piece scheduler limits".into()));
        }
        Ok(Self {
            states: vec![PieceState::Missing; piece_count],
            availability: vec![0; piece_count],
            failures: vec![0; piece_count],
            max_failures,
            priorities: vec![0; piece_count],
        })
    }

    pub fn observe_bitfield(&mut self, bitfield: &[u8]) -> Result<()> {
        if bitfield.len() != self.states.len().div_ceil(8) {
            return Err(ProxyError::Parse("peer bitfield length mismatch".into()));
        }
        for index in 0..self.states.len() {
            if bitfield[index / 8] & (0x80 >> (index % 8)) != 0 {
                self.availability[index] = self.availability[index].saturating_add(1);
            }
        }
        let spare_bits = bitfield.len() * 8 - self.states.len();
        if spare_bits > 0
            && bitfield.last().copied().unwrap_or_default() & ((1u8 << spare_bits) - 1) != 0
        {
            return Err(ProxyError::Parse("peer bitfield sets spare bits".into()));
        }
        Ok(())
    }

    pub fn claim_next(&mut self, peer_bitfield: &[u8]) -> Result<Option<u32>> {
        if peer_bitfield.len() != self.states.len().div_ceil(8) {
            return Err(ProxyError::Parse("peer bitfield length mismatch".into()));
        }
        let candidate = (0..self.states.len())
            .filter(|index| {
                peer_bitfield[index / 8] & (0x80 >> (index % 8)) != 0
                    && self.states[*index] == PieceState::Missing
                    && self.failures[*index] < self.max_failures
            })
            .min_by_key(|index| {
                (
                    u8::MAX - self.priorities[*index],
                    self.availability[*index],
                    self.failures[*index],
                    *index,
                )
            });
        if let Some(index) = candidate {
            self.states[index] = PieceState::InFlight;
            return Ok(Some(index as u32));
        }
        Ok(None)
    }

    pub fn complete(&mut self, index: u32) -> Result<()> {
        let state = self
            .states
            .get_mut(index as usize)
            .ok_or_else(|| ProxyError::Request("piece index out of range".into()))?;
        if *state != PieceState::InFlight {
            return Err(ProxyError::Request("piece is not in flight".into()));
        }
        *state = PieceState::Complete;
        Ok(())
    }

    pub fn fail(&mut self, index: u32) -> Result<()> {
        let index = index as usize;
        let state = self
            .states
            .get_mut(index)
            .ok_or_else(|| ProxyError::Request("piece index out of range".into()))?;
        if *state != PieceState::InFlight {
            return Err(ProxyError::Request("piece is not in flight".into()));
        }
        self.failures[index] = self.failures[index].saturating_add(1);
        *state = PieceState::Missing;
        Ok(())
    }

    pub fn is_complete(&self) -> bool {
        self.states
            .iter()
            .all(|state| *state == PieceState::Complete)
    }

    pub fn prioritize_byte_range(
        &mut self,
        metadata: &TorrentMetadata,
        start: u64,
        end: u64,
        priority: u8,
    ) -> Result<()> {
        if start > end
            || end >= metadata.total_length
            || metadata.piece_sha1.len() != self.states.len()
        {
            return Err(ProxyError::Request(
                "invalid playback priority range".into(),
            ));
        }
        let first = usize::try_from(start / metadata.piece_length as u64)
            .map_err(|_| ProxyError::Request("priority piece index overflow".into()))?;
        let last = usize::try_from(end / metadata.piece_length as u64)
            .map_err(|_| ProxyError::Request("priority piece index overflow".into()))?;
        for index in first..=last {
            self.priorities[index] = self.priorities[index].max(priority);
        }
        Ok(())
    }

    pub fn clear_priorities(&mut self) {
        self.priorities.fill(0);
    }
}

pub async fn download_next_piece(
    connection: &mut PeerConnection,
    scheduler: &mut PieceScheduler,
    peer_bitfield: &[u8],
    metadata: &TorrentMetadata,
    store: &TorrentPieceStore,
) -> Result<Option<u32>> {
    let Some(index) = scheduler.claim_next(peer_bitfield)? else {
        return Ok(None);
    };
    let result = async {
        let length = torrent_piece_length(metadata, index)?;
        let expected = metadata
            .piece_sha1
            .get(index as usize)
            .ok_or_else(|| ProxyError::Request("piece hash index out of range".into()))?;
        if store.load(index, expected)?.is_none() {
            let bytes = connection.download_piece(index, length, expected).await?;
            store.store(index, &bytes, expected)?;
        }
        Ok::<(), ProxyError>(())
    }
    .await;
    match result {
        Ok(()) => {
            scheduler.complete(index)?;
            Ok(Some(index))
        }
        Err(error) => {
            scheduler.fail(index)?;
            Err(error)
        }
    }
}

fn torrent_piece_length(metadata: &TorrentMetadata, index: u32) -> Result<u32> {
    let index = index as usize;
    if index >= metadata.piece_sha1.len() {
        return Err(ProxyError::Request("piece index out of range".into()));
    }
    if index + 1 < metadata.piece_sha1.len() {
        return Ok(metadata.piece_length);
    }
    let consumed = (metadata.piece_length as u64)
        .checked_mul(index as u64)
        .ok_or_else(|| ProxyError::Request("piece offset overflow".into()))?;
    u32::try_from(
        metadata
            .total_length
            .checked_sub(consumed)
            .ok_or_else(|| ProxyError::Request("piece length underflow".into()))?,
    )
    .map_err(|_| ProxyError::Request("last piece length overflow".into()))
}

impl PeerConnection {
    pub async fn connect(
        target: std::net::SocketAddr,
        local: &BitTorrentHandshake,
    ) -> Result<Self> {
        if !is_public_socket(target) {
            return Err(ProxyError::Request(
                "peer target must be a public address".into(),
            ));
        }
        let mut stream = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(target),
        )
        .await
        .map_err(|_| ProxyError::Network("peer connection timed out".into()))?
        .map_err(|error| ProxyError::Network(format!("peer connection failed: {error}")))?;
        stream
            .set_nodelay(true)
            .map_err(|error| ProxyError::Network(format!("peer TCP setup failed: {error}")))?;
        let handshake = encode_handshake(local);
        tokio::time::timeout(Duration::from_secs(5), stream.write_all(&handshake))
            .await
            .map_err(|_| ProxyError::Network("peer handshake send timed out".into()))?
            .map_err(|error| ProxyError::Network(format!("peer handshake send failed: {error}")))?;
        let mut response = [0u8; 68];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut response))
            .await
            .map_err(|_| ProxyError::Network("peer handshake receive timed out".into()))?
            .map_err(|error| {
                ProxyError::Network(format!("peer handshake receive failed: {error}"))
            })?;
        let remote_handshake = parse_handshake(&response)?;
        if remote_handshake.info_hash != local.info_hash {
            return Err(ProxyError::Request(
                "peer handshake info hash mismatch".into(),
            ));
        }
        Ok(Self {
            stream,
            remote_handshake,
        })
    }

    pub async fn send(&mut self, message: &PeerMessage) -> Result<()> {
        let encoded = encode_peer_message(message)?;
        tokio::time::timeout(Duration::from_secs(5), self.stream.write_all(&encoded))
            .await
            .map_err(|_| ProxyError::Network("peer message send timed out".into()))?
            .map_err(|error| ProxyError::Network(format!("peer message send failed: {error}")))
    }

    pub async fn receive(&mut self) -> Result<PeerMessage> {
        let mut prefix = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(15), self.stream.read_exact(&mut prefix))
            .await
            .map_err(|_| ProxyError::Network("peer message receive timed out".into()))?
            .map_err(|error| {
                ProxyError::Network(format!("peer message receive failed: {error}"))
            })?;
        let length = u32::from_be_bytes(prefix) as usize;
        if length > MAX_PEER_MESSAGE_BYTES {
            return Err(ProxyError::Request("peer message is too large".into()));
        }
        let mut encoded = Vec::with_capacity(length + 4);
        encoded.extend_from_slice(&prefix);
        encoded.resize(length + 4, 0);
        tokio::time::timeout(
            Duration::from_secs(15),
            self.stream.read_exact(&mut encoded[4..]),
        )
        .await
        .map_err(|_| ProxyError::Network("peer message body timed out".into()))?
        .map_err(|error| ProxyError::Network(format!("peer message body failed: {error}")))?;
        parse_peer_message(&encoded)
    }

    pub async fn download_block(&mut self, index: u32, begin: u32, length: u32) -> Result<Vec<u8>> {
        validate_block_request(begin, length)?;
        let operation = async {
            self.send(&PeerMessage::Interested).await?;
            loop {
                match self.receive().await? {
                    PeerMessage::Unchoke => break,
                    PeerMessage::Choke
                    | PeerMessage::KeepAlive
                    | PeerMessage::Have(_)
                    | PeerMessage::Bitfield(_) => continue,
                    _ => {
                        return Err(ProxyError::Request(
                            "unexpected peer message before unchoke".into(),
                        ))
                    }
                }
            }
            self.send(&PeerMessage::Request {
                index,
                begin,
                length,
            })
            .await?;
            loop {
                match self.receive().await? {
                    PeerMessage::Piece {
                        index: received_index,
                        begin: received_begin,
                        block,
                    } => {
                        if received_index != index
                            || received_begin != begin
                            || block.len() != length as usize
                        {
                            return Err(ProxyError::Request(
                                "peer returned mismatched block".into(),
                            ));
                        }
                        return Ok(block);
                    }
                    PeerMessage::Choke => {
                        return Err(ProxyError::Request("peer choked block request".into()))
                    }
                    PeerMessage::KeepAlive | PeerMessage::Have(_) => continue,
                    _ => {
                        return Err(ProxyError::Request(
                            "unexpected peer message during block request".into(),
                        ))
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), operation)
            .await
            .map_err(|_| ProxyError::Network("peer block request timed out".into()))?
    }

    pub async fn download_piece(
        &mut self,
        index: u32,
        piece_length: u32,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>> {
        if piece_length == 0 || piece_length > 8 * 1024 * 1024 {
            return Err(ProxyError::Request(
                "invalid BitTorrent piece length".into(),
            ));
        }
        let mut piece = Vec::with_capacity(piece_length as usize);
        let mut begin = 0u32;
        while begin < piece_length {
            let length = (piece_length - begin).min(MAX_REQUEST_BLOCK_BYTES);
            piece.extend_from_slice(&self.download_block(index, begin, length).await?);
            begin = begin
                .checked_add(length)
                .ok_or_else(|| ProxyError::Request("piece offset overflow".into()))?;
        }
        verify_piece_sha1(&piece, expected_sha1)?;
        Ok(piece)
    }

    pub async fn download_metadata(&mut self, expected_info_hash: [u8; 20]) -> Result<Vec<u8>> {
        let operation = async {
            self.send(&encode_extended_handshake()).await?;
            let remote = loop {
                match self.receive().await? {
                    PeerMessage::Extended {
                        extension_id: 0,
                        payload,
                    } => break parse_extended_handshake(&payload)?,
                    PeerMessage::KeepAlive | PeerMessage::Have(_) | PeerMessage::Bitfield(_) => {
                        continue
                    }
                    _ => {
                        return Err(ProxyError::Request(
                            "peer did not provide extended handshake".into(),
                        ))
                    }
                }
            };
            let mut assembler = MetadataAssembler::new(expected_info_hash, remote.metadata_size)?;
            for piece in 0..remote.metadata_size.div_ceil(16 * 1024) {
                self.send(&encode_ut_metadata_request(
                    remote.ut_metadata_id,
                    piece as u32,
                )?)
                .await?;
                loop {
                    match self.receive().await? {
                        PeerMessage::Extended {
                            extension_id,
                            payload,
                        } if extension_id == remote.ut_metadata_id => {
                            let response = parse_ut_metadata_response(&payload)?;
                            if matches!(response, MetadataExtensionResponse::Reject { .. }) {
                                return Err(ProxyError::Request(
                                    "peer rejected metadata piece".into(),
                                ));
                            }
                            assembler.insert(response)?;
                            break;
                        }
                        PeerMessage::KeepAlive | PeerMessage::Have(_) => continue,
                        _ => {
                            return Err(ProxyError::Request(
                                "unexpected message during metadata exchange".into(),
                            ))
                        }
                    }
                }
            }
            assembler.finish()
        };
        tokio::time::timeout(Duration::from_secs(60), operation)
            .await
            .map_err(|_| ProxyError::Network("metadata exchange timed out".into()))?
    }

    pub async fn download_torrent_metadata(
        &mut self,
        expected_info_hash: [u8; 20],
    ) -> Result<TorrentMetadata> {
        let raw_info = self.download_metadata(expected_info_hash).await?;
        let metadata = parse_info_metadata(&raw_info)?;
        if metadata.info_hash != expected_info_hash {
            return Err(ProxyError::Request(
                "downloaded torrent metadata InfoHash mismatch".into(),
            ));
        }
        Ok(metadata)
    }
}

const MAX_PEER_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BLOCK_BYTES: u32 = 16 * 1024;

fn validate_block_request(begin: u32, length: u32) -> Result<()> {
    if length == 0 || length > MAX_REQUEST_BLOCK_BYTES || begin.checked_add(length).is_none() {
        return Err(ProxyError::Request("invalid peer block request".into()));
    }
    Ok(())
}

fn verify_piece_sha1(piece: &[u8], expected: &[u8; 20]) -> Result<()> {
    let digest = Sha1::digest(piece);
    if digest.as_slice() != expected {
        return Err(ProxyError::Request(
            "BitTorrent piece SHA-1 mismatch".into(),
        ));
    }
    Ok(())
}

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
        20 if !body.is_empty() => Ok(PeerMessage::Extended {
            extension_id: body[0],
            payload: body[1..].to_vec(),
        }),
        _ => Err(ProxyError::Parse(
            "unsupported or malformed peer message".into(),
        )),
    }
}

pub fn encode_peer_message(message: &PeerMessage) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    match message {
        PeerMessage::KeepAlive => return Ok(vec![0, 0, 0, 0]),
        PeerMessage::Choke => payload.push(0),
        PeerMessage::Unchoke => payload.push(1),
        PeerMessage::Interested => payload.push(2),
        PeerMessage::NotInterested => payload.push(3),
        PeerMessage::Have(index) => {
            payload.push(4);
            payload.extend_from_slice(&index.to_be_bytes());
        }
        PeerMessage::Bitfield(bits) => {
            if bits.len() > MAX_PEER_MESSAGE_BYTES - 1 {
                return Err(ProxyError::Request("peer bitfield is too large".into()));
            }
            payload.push(5);
            payload.extend_from_slice(bits);
        }
        PeerMessage::Request {
            index,
            begin,
            length,
        } => {
            payload.push(6);
            payload.extend_from_slice(&index.to_be_bytes());
            payload.extend_from_slice(&begin.to_be_bytes());
            payload.extend_from_slice(&length.to_be_bytes());
        }
        PeerMessage::Piece {
            index,
            begin,
            block,
        } => {
            if block.len() > MAX_PEER_MESSAGE_BYTES - 9 {
                return Err(ProxyError::Request("peer block is too large".into()));
            }
            payload.push(7);
            payload.extend_from_slice(&index.to_be_bytes());
            payload.extend_from_slice(&begin.to_be_bytes());
            payload.extend_from_slice(block);
        }
        PeerMessage::Cancel {
            index,
            begin,
            length,
        } => {
            payload.push(8);
            payload.extend_from_slice(&index.to_be_bytes());
            payload.extend_from_slice(&begin.to_be_bytes());
            payload.extend_from_slice(&length.to_be_bytes());
        }
        PeerMessage::Port(port) => {
            payload.push(9);
            payload.extend_from_slice(&port.to_be_bytes());
        }
        PeerMessage::Extended {
            extension_id,
            payload: extension_payload,
        } => {
            if extension_payload.len() > MAX_PEER_MESSAGE_BYTES - 2 {
                return Err(ProxyError::Request(
                    "extended peer message is too large".into(),
                ));
            }
            payload.push(20);
            payload.push(*extension_id);
            payload.extend_from_slice(extension_payload);
        }
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| ProxyError::Request("peer message is too large".into()))?;
    let mut output = Vec::with_capacity(payload.len() + 4);
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(&payload);
    Ok(output)
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
    List(Vec<BValue>),
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
        let mut values = Vec::new();
        while self.input.get(self.offset) != Some(&b'e') {
            values.push(self.value()?);
        }
        self.offset += 1;
        Ok(BValue::List(values))
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
    fn parses_single_file_torrent_metadata() {
        let mut torrent = b"d4:infod6:lengthi3e4:name4:test12:piece lengthi3e6:pieces20:".to_vec();
        torrent.extend_from_slice(&[7; 20]);
        torrent.extend_from_slice(b"ee");
        let metadata = parse_torrent_metadata(&torrent).unwrap();
        assert_eq!(metadata.name, "test");
        assert_eq!(metadata.total_length, 3);
        assert_eq!(metadata.piece_sha1, vec![[7; 20]]);
        assert_eq!(metadata.files[0].path, vec!["test"]);
        assert!(parse_torrent_metadata(b"d4:info3:bade").is_err());
        let info_start = b"d4:info".len();
        let raw_info = &torrent[info_start..torrent.len() - 1];
        assert_eq!(parse_info_metadata(raw_info).unwrap(), metadata);
    }

    #[test]
    fn parses_multi_file_torrent_and_rejects_traversal() {
        let mut torrent =
            b"d4:infod5:filesld6:lengthi1e4:pathl1:aeee4:name4:root12:piece lengthi1e6:pieces20:"
                .to_vec();
        torrent.extend_from_slice(&[3; 20]);
        torrent.extend_from_slice(b"ee");
        let metadata = parse_torrent_metadata(&torrent).unwrap();
        assert_eq!(metadata.total_length, 1);
        assert_eq!(metadata.files[0].path, vec!["root", "a"]);
        let unsafe_torrent = b"d4:infod5:filesld6:lengthi1e4:pathl2:..eee4:name4:root12:piece lengthi1e6:pieces20:12345678901234567890ee";
        assert!(parse_torrent_metadata(unsafe_torrent).is_err());
    }

    #[test]
    fn piece_store_recovers_verified_pieces_and_drops_corruption() {
        let root = tempfile::tempdir().unwrap();
        let store = TorrentPieceStore::new(root.path(), &[1; 20]).unwrap();
        let hash: [u8; 20] = Sha1::digest(b"piece").into();
        store.store(0, b"piece", &hash).unwrap();
        assert_eq!(store.completed(&[hash]).unwrap(), vec![0]);
        std::fs::write(store.directory.join("0.piece"), b"broken").unwrap();
        assert!(store.load(0, &hash).unwrap().is_none());
    }

    #[cfg(feature = "p2p-upload")]
    #[test]
    fn upload_session_requires_authorization_and_enforces_quota() {
        let root = tempfile::tempdir().unwrap();
        let hash: [u8; 20] = Sha1::digest(b"piece").into();
        let store = TorrentPieceStore::new(root.path(), &[1; 20]).unwrap();
        store.store(0, b"piece", &hash).unwrap();
        let metadata = TorrentMetadata {
            info_hash: [1; 20],
            name: "x".into(),
            total_length: 5,
            piece_length: 5,
            piece_sha1: vec![hash],
            files: vec![TorrentFile {
                path: vec!["x".into()],
                length: 5,
            }],
        };
        assert!(UploadSession::new(false, 10)
            .unwrap()
            .serve_block(&metadata, &store, 0, 0, 1)
            .is_err());
        let mut upload = UploadSession::new(true, 3).unwrap();
        assert_eq!(
            upload.serve_block(&metadata, &store, 0, 1, 2).unwrap(),
            b"ie"
        );
        assert!(upload.serve_block(&metadata, &store, 0, 0, 2).is_err());
        assert_eq!(upload.uploaded_bytes(), 2);
    }

    #[test]
    fn parses_extended_handshake_and_metadata_request() {
        let handshake =
            parse_extended_handshake(b"d1:md11:ut_metadatai3ee13:metadata_sizei32768ee").unwrap();
        assert_eq!(handshake.ut_metadata_id, 3);
        assert_eq!(handshake.metadata_size, 32768);
        let message = encode_ut_metadata_request(3, 1).unwrap();
        assert_eq!(
            parse_peer_message(&encode_peer_message(&message).unwrap()).unwrap(),
            message
        );
        assert_eq!(
            parse_peer_message(&encode_peer_message(&encode_extended_handshake()).unwrap())
                .unwrap(),
            encode_extended_handshake()
        );
    }

    #[test]
    fn parses_and_assembles_metadata_extension_data() {
        let metadata = b"d4:name4:test6:lengthi1ee".to_vec();
        let hash: [u8; 20] = Sha1::digest(&metadata).into();
        let mut payload =
            format!("d8:msg_typei1e5:piecei0e10:total_sizei{}ee", metadata.len()).into_bytes();
        payload.extend_from_slice(&metadata);
        let response = parse_ut_metadata_response(&payload).unwrap();
        let mut assembler = MetadataAssembler::new(hash, metadata.len()).unwrap();
        assert!(assembler.insert(response).unwrap());
        assert_eq!(assembler.finish().unwrap(), metadata);
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
        let piece = PeerMessage::Piece {
            index: 1,
            begin: 2,
            block: b"abc".to_vec(),
        };
        assert_eq!(
            parse_peer_message(&encode_peer_message(&piece).unwrap()).unwrap(),
            piece
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

    #[test]
    fn round_trips_udp_tracker_packets() {
        let request = TrackerAnnounce {
            info_hash: [1; 20],
            peer_id: [2; 20],
            port: 6881,
            uploaded: 3,
            downloaded: 4,
            left: 5,
            numwant: Some(20),
        };
        let connect = encode_udp_connect_request(7);
        let mut connect_response = [0u8; 16];
        connect_response[..4].copy_from_slice(&0u32.to_be_bytes());
        connect_response[4..8].copy_from_slice(&7u32.to_be_bytes());
        connect_response[8..].copy_from_slice(&9u64.to_be_bytes());
        assert_eq!(parse_udp_connect_response(&connect_response, 7).unwrap(), 9);
        assert_eq!(
            u64::from_be_bytes(connect[..8].try_into().unwrap()),
            UDP_TRACKER_PROTOCOL_ID
        );
        let announce = encode_udp_announce_request(9, 8, &request);
        let mut response = vec![0u8; 26];
        response[..4].copy_from_slice(&1u32.to_be_bytes());
        response[4..8].copy_from_slice(&8u32.to_be_bytes());
        response[8..12].copy_from_slice(&30u32.to_be_bytes());
        response[20..26].copy_from_slice(&[127, 0, 0, 1, 0x1a, 0xe1]);
        assert_eq!(
            parse_udp_announce_response(&response, 8)
                .unwrap()
                .peers
                .len(),
            1
        );
        assert_eq!(&announce[96..98], &6881u16.to_be_bytes());
    }

    #[test]
    fn builds_dht_ping_and_parses_compact_nodes() {
        let ping = encode_dht_ping(b"aa").unwrap();
        assert!(ping.starts_with(b"d1:ad2:id20:"));
        assert!(ping.ends_with(b"1:q4:ping1:t2:aa1:y1:qe"));
        let mut compact = vec![7u8; 20];
        compact.extend_from_slice(&[127, 0, 0, 1, 0x1a, 0xe1]);
        let nodes = parse_dht_compact_nodes(&compact).unwrap();
        assert_eq!(nodes[0].id, [7; 20]);
        assert_eq!(nodes[0].address, "127.0.0.1:6881".parse().unwrap());
        assert!(parse_dht_compact_nodes(&[0; 25]).is_err());
    }

    #[test]
    fn builds_and_parses_dht_get_peers() {
        let query = encode_dht_get_peers(b"aa", &[1; 20], &[2; 20]).unwrap();
        assert!(query
            .windows(b"9:get_peers".len())
            .any(|window| window == b"9:get_peers"));
        let response = b"d1:rd5:nodes26:\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x7f\x00\x00\x01\x1a\xe15:token2:ok6:valuesl6:\x7f\x00\x00\x01\x1a\xe1ee1:t2:aa1:y1:re";
        let parsed = parse_dht_get_peers_response(response, b"aa").unwrap();
        assert_eq!(parsed.token, b"ok");
        assert_eq!(parsed.nodes.len(), 1);
        assert_eq!(parsed.peers.len(), 1);
    }

    #[test]
    fn routing_table_rejects_private_nodes_and_keeps_closest() {
        let mut table = DhtRoutingTable::new([0; 20], 2).unwrap();
        assert!(!table.insert(DhtNode {
            id: [1; 20],
            address: "127.0.0.1:6881".parse().unwrap()
        }));
        assert!(table.insert(DhtNode {
            id: [3; 20],
            address: "1.1.1.1:6881".parse().unwrap()
        }));
        assert!(table.insert(DhtNode {
            id: [2; 20],
            address: "8.8.8.8:6881".parse().unwrap()
        }));
        assert!(table.insert(DhtNode {
            id: [1; 20],
            address: "9.9.9.9:6881".parse().unwrap()
        }));
        let closest = table.closest(&[0; 20], 2);
        assert_eq!(closest[0].id, [1; 20]);
        assert_eq!(closest[1].id, [2; 20]);
    }

    #[tokio::test]
    async fn dht_udp_query_rejects_private_targets_before_network_io() {
        let error = send_dht_udp_query("127.0.0.1:6881".parse().unwrap(), b"query")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("public"));
    }

    #[tokio::test]
    async fn udp_tracker_rejects_private_target_before_network_io() {
        let request = TrackerAnnounce {
            info_hash: [1; 20],
            peer_id: [2; 20],
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1,
            numwant: None,
        };
        let error = announce_udp_tracker("127.0.0.1:80".parse().unwrap(), &request, 1, 2)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("public"));
    }

    #[tokio::test]
    async fn peer_connection_rejects_private_target_before_network_io() {
        let handshake = BitTorrentHandshake {
            reserved: [0; 8],
            info_hash: [1; 20],
            peer_id: [2; 20],
        };
        let error = PeerConnection::connect("127.0.0.1:6881".parse().unwrap(), &handshake)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("public"));
    }

    #[test]
    fn validates_peer_block_request_bounds() {
        assert!(validate_block_request(0, 16 * 1024).is_ok());
        assert!(validate_block_request(0, 0).is_err());
        assert!(validate_block_request(0, 16 * 1024 + 1).is_err());
        assert!(validate_block_request(u32::MAX, 1).is_err());
    }

    #[test]
    fn verifies_bittorrent_piece_sha1() {
        let expected: [u8; 20] = Sha1::digest(b"abc").into();
        assert!(verify_piece_sha1(b"abc", &expected).is_ok());
        assert!(verify_piece_sha1(b"abd", &expected).is_err());
    }

    #[test]
    fn scheduler_selects_rarest_and_bounds_retries() {
        let mut scheduler = PieceScheduler::new(3, 2).unwrap();
        scheduler.observe_bitfield(&[0b1110_0000]).unwrap();
        scheduler.observe_bitfield(&[0b1100_0000]).unwrap();
        assert_eq!(scheduler.claim_next(&[0b1110_0000]).unwrap(), Some(2));
        scheduler.fail(2).unwrap();
        assert_eq!(scheduler.claim_next(&[0b1110_0000]).unwrap(), Some(2));
        scheduler.fail(2).unwrap();
        assert_eq!(scheduler.claim_next(&[0b1110_0000]).unwrap(), Some(0));
        scheduler.complete(0).unwrap();
        assert!(!scheduler.is_complete());
        assert!(scheduler.observe_bitfield(&[0b1110_0001]).is_err());
    }

    #[test]
    fn calculates_last_torrent_piece_length() {
        let metadata = TorrentMetadata {
            info_hash: [0; 20],
            name: "x".into(),
            total_length: 10,
            piece_length: 4,
            piece_sha1: vec![[0; 20]; 3],
            files: vec![TorrentFile {
                path: vec!["x".into()],
                length: 10,
            }],
        };
        assert_eq!(torrent_piece_length(&metadata, 0).unwrap(), 4);
        assert_eq!(torrent_piece_length(&metadata, 2).unwrap(), 2);
        assert!(torrent_piece_length(&metadata, 3).is_err());
    }

    #[test]
    fn playback_range_overrides_rarest_first_priority() {
        let metadata = TorrentMetadata {
            info_hash: [0; 20],
            name: "x".into(),
            total_length: 12,
            piece_length: 4,
            piece_sha1: vec![[0; 20]; 3],
            files: vec![TorrentFile {
                path: vec!["x".into()],
                length: 12,
            }],
        };
        let mut scheduler = PieceScheduler::new(3, 2).unwrap();
        scheduler.observe_bitfield(&[0b1110_0000]).unwrap();
        scheduler
            .prioritize_byte_range(&metadata, 8, 11, 100)
            .unwrap();
        assert_eq!(scheduler.claim_next(&[0b1110_0000]).unwrap(), Some(2));
        scheduler.clear_priorities();
    }
}
