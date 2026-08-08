//! Optional, compliance-first P2P integration boundary.
//!
//! Core does not perform peer discovery or accept magnet/torrent inputs. A Host
//! may provide bytes only after creating a validated descriptor that proves the
//! application made an explicit authorization decision.

use crate::utils::digest::sha256_hex;
use crate::utils::error::{ProxyError, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const SHA256_HEX_LENGTH: usize = 64;
const MAX_CONTENT_ID_LENGTH: usize = 512;
const MAX_AUTHORIZATION_LENGTH: usize = 2048;
const MAX_P2P_READ_BYTES: u64 = 8 * 1024 * 1024;
const MAX_P2P_SOURCES: usize = 1024;
const MAX_MANIFEST_JSON_BYTES: usize = 1024 * 1024;
const MAX_VERIFIED_PIECE_CACHE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_DISK_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
const STALE_P2P_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

pub type P2pPieceProvider = Arc<dyn Fn(usize) -> Result<Vec<u8>> + Send + Sync>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct P2pManifestDocument {
    content_id: String,
    content_length: u64,
    content_sha256: String,
    piece_length: u64,
    piece_sha256: Vec<String>,
    authorization_reference: String,
    explicitly_authorized: bool,
}

pub fn parse_authorized_manifest_json(
    json: &[u8],
) -> Result<(AuthorizedP2pSource, P2pPieceManifest)> {
    if json.is_empty() || json.len() > MAX_MANIFEST_JSON_BYTES {
        return Err(ProxyError::Request(
            "P2P manifest JSON size is invalid".to_string(),
        ));
    }
    let document: P2pManifestDocument = serde_json::from_slice(json)
        .map_err(|_| ProxyError::Request("P2P manifest JSON is invalid".to_string()))?;
    let source = AuthorizedP2pSource::new(
        &document.content_id,
        document.content_length,
        &document.content_sha256,
        &document.authorization_reference,
        document.explicitly_authorized,
    )?;
    let manifest = P2pPieceManifest::new(
        document.content_length,
        document.piece_length,
        document.piece_sha256,
    )?;
    Ok((source, manifest))
}

struct RegisteredP2pSource {
    source: AuthorizedP2pSource,
    manifest: P2pPieceManifest,
    provider: P2pPieceProvider,
    piece_cache: Arc<Mutex<VerifiedPieceCache>>,
    piece_locks: Arc<Mutex<HashMap<usize, Arc<Mutex<()>>>>>,
    disk_dir: Option<PathBuf>,
    disk_cache: Option<Arc<P2pDiskCache>>,
}

struct P2pDiskCache {
    root: PathBuf,
    max_bytes: u64,
    maintenance: Mutex<()>,
}

#[derive(Default)]
struct VerifiedPieceCache {
    entries: HashMap<usize, Arc<Vec<u8>>>,
    order: VecDeque<usize>,
    bytes: usize,
}

impl VerifiedPieceCache {
    fn get(&self, index: usize) -> Option<Arc<Vec<u8>>> {
        self.entries.get(&index).cloned()
    }

    fn insert(&mut self, index: usize, piece: Vec<u8>) -> Arc<Vec<u8>> {
        if let Some(existing) = self.entries.get(&index) {
            return existing.clone();
        }
        let piece = Arc::new(piece);
        if piece.len() > MAX_VERIFIED_PIECE_CACHE_BYTES {
            return piece;
        }
        while self.bytes.saturating_add(piece.len()) > MAX_VERIFIED_PIECE_CACHE_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.len());
            }
        }
        self.bytes += piece.len();
        self.order.push_back(index);
        self.entries.insert(index, piece.clone());
        piece
    }
}

#[derive(Clone)]
pub struct P2pSourceRegistry {
    next_id: Arc<AtomicU64>,
    entries: Arc<RwLock<HashMap<u64, RegisteredP2pSource>>>,
    disk_cache: Option<Arc<P2pDiskCache>>,
}

impl Default for P2pSourceRegistry {
    fn default() -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            entries: Arc::new(RwLock::new(HashMap::new())),
            disk_cache: None,
        }
    }
}

impl P2pSourceRegistry {
    pub fn with_cache_dir(cache_root: PathBuf) -> Self {
        Self::with_cache_limit(cache_root, DEFAULT_DISK_CACHE_BYTES)
    }

    pub fn with_cache_limit(cache_root: PathBuf, max_bytes: u64) -> Self {
        cleanup_stale_disk_temps(&cache_root);
        Self {
            disk_cache: Some(Arc::new(P2pDiskCache {
                root: cache_root,
                max_bytes,
                maintenance: Mutex::new(()),
            })),
            ..Self::default()
        }
    }

    pub fn register(
        &self,
        source: AuthorizedP2pSource,
        manifest: P2pPieceManifest,
        provider: P2pPieceProvider,
    ) -> Result<u64> {
        if source.content_length != manifest.content_length {
            return Err(ProxyError::Request(
                "P2P source and manifest lengths differ".to_string(),
            ));
        }
        let mut entries = self
            .entries
            .write()
            .map_err(|_| ProxyError::Storage("P2P registry unavailable".to_string()))?;
        if entries.len() >= MAX_P2P_SOURCES {
            return Err(ProxyError::Request(
                "P2P source registry is full".to_string(),
            ));
        }
        let id = self
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                (value != 0).then_some(value.wrapping_add(1).max(1))
            })
            .map_err(|_| ProxyError::Request("P2P source ID exhausted".to_string()))?;
        let disk_dir = self
            .disk_cache
            .as_ref()
            .map(|cache| cache.root.join(&source.sha256));
        entries.insert(
            id,
            RegisteredP2pSource {
                source,
                manifest,
                provider,
                piece_cache: Arc::new(Mutex::new(VerifiedPieceCache::default())),
                piece_locks: Arc::new(Mutex::new(HashMap::new())),
                disk_dir,
                disk_cache: self.disk_cache.clone(),
            },
        );
        Ok(id)
    }

    pub fn read_range(&self, id: u64, start: u64, end: u64) -> Result<Vec<u8>> {
        let entries = self
            .entries
            .read()
            .map_err(|_| ProxyError::Storage("P2P registry unavailable".to_string()))?;
        let entry = entries
            .get(&id)
            .ok_or_else(|| ProxyError::Request("P2P source ID not found".to_string()))?;
        let manifest = entry.manifest.clone();
        let provider = entry.provider.clone();
        let cache = entry.piece_cache.clone();
        let piece_locks = entry.piece_locks.clone();
        let disk_dir = entry.disk_dir.clone();
        let disk_cache = entry.disk_cache.clone();
        drop(entries);
        let provider = cached_verified_provider(
            manifest.clone(),
            provider,
            cache,
            piece_locks,
            disk_dir,
            disk_cache,
        );
        manifest.read_verified_range(start, end, &provider)
    }

    pub fn verify_complete(&self, id: u64) -> Result<()> {
        let entries = self
            .entries
            .read()
            .map_err(|_| ProxyError::Storage("P2P registry unavailable".to_string()))?;
        let entry = entries
            .get(&id)
            .ok_or_else(|| ProxyError::Request("P2P source ID not found".to_string()))?;
        let source_digest = entry.source.sha256.clone();
        let manifest = entry.manifest.clone();
        let provider = entry.provider.clone();
        let cache = entry.piece_cache.clone();
        let piece_locks = entry.piece_locks.clone();
        let disk_dir = entry.disk_dir.clone();
        let disk_cache = entry.disk_cache.clone();
        drop(entries);
        let provider = cached_verified_provider(
            manifest.clone(),
            provider,
            cache,
            piece_locks,
            disk_dir,
            disk_cache,
        );

        let mut hasher = Sha256::new();
        for index in 0..manifest.piece_sha256.len() {
            let piece = provider(index)?;
            manifest.verify_piece(index, &piece)?;
            hasher.update(&piece);
        }
        let actual = bytes_to_hex(&hasher.finalize());
        if actual != source_digest {
            return Err(ProxyError::Request(
                "P2P complete content integrity check failed".to_string(),
            ));
        }
        Ok(())
    }

    pub fn content_length(&self, id: u64) -> Option<u64> {
        self.entries
            .read()
            .ok()?
            .get(&id)
            .map(|entry| entry.source.content_length)
    }

    pub fn source_info(&self, id: u64) -> Option<AuthorizedP2pSource> {
        self.entries
            .read()
            .ok()?
            .get(&id)
            .map(|entry| entry.source.clone())
    }

    pub fn remove(&self, id: u64) -> bool {
        self.entries
            .write()
            .ok()
            .and_then(|mut entries| entries.remove(&id))
            .is_some()
    }
}

fn cached_verified_provider(
    manifest: P2pPieceManifest,
    provider: P2pPieceProvider,
    cache: Arc<Mutex<VerifiedPieceCache>>,
    piece_locks: Arc<Mutex<HashMap<usize, Arc<Mutex<()>>>>>,
    disk_dir: Option<PathBuf>,
    disk_cache: Option<Arc<P2pDiskCache>>,
) -> P2pPieceProvider {
    Arc::new(move |index| {
        if let Some(piece) = cache
            .lock()
            .map_err(|_| ProxyError::Storage("P2P piece cache unavailable".to_string()))?
            .get(index)
        {
            return Ok(piece.as_ref().clone());
        }
        let piece_lock = piece_locks
            .lock()
            .map_err(|_| ProxyError::Storage("P2P piece locks unavailable".to_string()))?
            .entry(index)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = piece_lock
            .lock()
            .map_err(|_| ProxyError::Storage("P2P piece lock unavailable".to_string()))?;
        if let Some(piece) = cache
            .lock()
            .map_err(|_| ProxyError::Storage("P2P piece cache unavailable".to_string()))?
            .get(index)
        {
            return Ok(piece.as_ref().clone());
        }
        if let Some(directory) = &disk_dir {
            if let Some(piece) = read_verified_disk_piece(directory, index, &manifest)? {
                let piece = cache
                    .lock()
                    .map_err(|_| ProxyError::Storage("P2P piece cache unavailable".to_string()))?
                    .insert(index, piece);
                return Ok(piece.as_ref().clone());
            }
        }
        let piece = provider(index)?;
        manifest.verify_piece(index, &piece)?;
        if let Some(directory) = &disk_dir {
            if disk_cache.as_ref().is_none_or(|cache| cache.max_bytes > 0) {
                write_verified_disk_piece(directory, index, &piece)?;
            }
        }
        if let Some(cache) = &disk_cache {
            enforce_disk_cache_limit(cache)?;
        }
        let piece = cache
            .lock()
            .map_err(|_| ProxyError::Storage("P2P piece cache unavailable".to_string()))?
            .insert(index, piece);
        Ok(piece.as_ref().clone())
    })
}

fn enforce_disk_cache_limit(cache: &P2pDiskCache) -> Result<()> {
    let _guard = cache
        .maintenance
        .lock()
        .map_err(|_| ProxyError::Storage("P2P disk cache unavailable".to_string()))?;
    let roots = match std::fs::read_dir(&cache.root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ProxyError::Storage(error.to_string())),
    };
    let mut files = Vec::new();
    let mut total = 0u64;
    for root in roots.flatten() {
        if !root.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(root.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("piece") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            total = total.saturating_add(metadata.len());
            files.push((
                metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                metadata.len(),
                path,
            ));
        }
    }
    files.sort_unstable_by_key(|(modified, _, _)| *modified);
    for (_, size, path) in files {
        if total <= cache.max_bytes {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
    Ok(())
}

fn cleanup_stale_disk_temps(root: &Path) {
    let Ok(source_dirs) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for source_dir in source_dirs.flatten() {
        if !source_dir.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(source_dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("tmp") {
                continue;
            }
            let Ok(modified) = file.metadata().and_then(|metadata| metadata.modified()) else {
                continue;
            };
            if now.duration_since(modified).unwrap_or_default() >= STALE_P2P_TEMP_AGE {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn read_verified_disk_piece(
    directory: &Path,
    index: usize,
    manifest: &P2pPieceManifest,
) -> Result<Option<Vec<u8>>> {
    let path = directory.join(format!("{index}.piece"));
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ProxyError::Storage(error.to_string())),
    };
    if manifest.verify_piece(index, &bytes).is_err() {
        let _ = std::fs::remove_file(path);
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn write_verified_disk_piece(directory: &Path, index: usize, bytes: &[u8]) -> Result<()> {
    static TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(directory).map_err(|error| ProxyError::Storage(error.to_string()))?;
    let final_path = directory.join(format!("{index}.piece"));
    let temporary = directory.join(format!(
        ".{index}.{}.{}.piece.tmp",
        std::process::id(),
        TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temporary, bytes).map_err(|error| ProxyError::Storage(error.to_string()))?;
    match std::fs::rename(&temporary, &final_path) {
        Ok(()) => Ok(()),
        Err(_error) if matches!(std::fs::read(&final_path), Ok(existing) if existing == bytes) => {
            let _ = std::fs::remove_file(temporary);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            Err(ProxyError::Storage(error.to_string()))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedP2pSource {
    content_id: String,
    content_length: u64,
    sha256: String,
    authorization_reference: String,
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
            authorization_reference,
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

    pub fn authorization_reference(&self) -> &str {
        &self.authorization_reference
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

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
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
        assert_eq!(source.authorization_reference(), "license-42");
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

    #[test]
    fn registry_returns_verified_ranges_by_opaque_id() {
        let source = AuthorizedP2pSource::new("asset", 6, DIGEST, "license", true).unwrap();
        let pieces = [b"abcd".to_vec(), b"ef".to_vec()];
        let manifest =
            P2pPieceManifest::new(6, 4, pieces.iter().map(|piece| sha256_hex(piece)).collect())
                .unwrap();
        let provider_pieces = pieces.clone();
        let provider: P2pPieceProvider = Arc::new(move |index| {
            provider_pieces
                .get(index)
                .cloned()
                .ok_or_else(|| ProxyError::Request("missing piece".to_string()))
        });
        let registry = P2pSourceRegistry::default();
        let id = registry.register(source, manifest, provider).unwrap();
        assert_ne!(id, 0);
        assert_eq!(registry.content_length(id), Some(6));
        assert_eq!(
            registry.source_info(id).unwrap().authorization_reference(),
            "license"
        );
        assert_eq!(registry.read_range(id, 1, 5).unwrap(), b"bcdef");
        assert!(registry.remove(id));
        assert!(registry.source_info(id).is_none());
        assert!(registry.read_range(id, 0, 0).is_err());
    }

    #[test]
    fn registry_rejects_mismatched_source_and_manifest() {
        let source = AuthorizedP2pSource::new("asset", 8, DIGEST, "license", true).unwrap();
        let manifest = P2pPieceManifest::new(4, 4, vec![sha256_hex(b"data")]).unwrap();
        let provider: P2pPieceProvider = Arc::new(|_| Ok(b"data".to_vec()));
        assert!(P2pSourceRegistry::default()
            .register(source, manifest, provider)
            .is_err());
    }

    #[test]
    fn verifies_complete_content_digest_after_piece_checks() {
        let content = b"abcdefghij";
        let source = AuthorizedP2pSource::new(
            "asset",
            content.len() as u64,
            &sha256_hex(content),
            "license",
            true,
        )
        .unwrap();
        let pieces = [b"abcd".to_vec(), b"efgh".to_vec(), b"ij".to_vec()];
        let manifest = P2pPieceManifest::new(
            content.len() as u64,
            4,
            pieces.iter().map(|piece| sha256_hex(piece)).collect(),
        )
        .unwrap();
        let provider_pieces = pieces.clone();
        let provider: P2pPieceProvider = Arc::new(move |index| {
            provider_pieces
                .get(index)
                .cloned()
                .ok_or_else(|| ProxyError::Request("missing piece".to_string()))
        });
        let registry = P2pSourceRegistry::default();
        let id = registry.register(source, manifest, provider).unwrap();
        assert!(registry.verify_complete(id).is_ok());
    }

    #[test]
    fn rejects_manifest_whose_pieces_do_not_match_whole_digest() {
        let source = AuthorizedP2pSource::new("asset", 4, DIGEST, "license", true).unwrap();
        let manifest = P2pPieceManifest::new(4, 4, vec![sha256_hex(b"data")]).unwrap();
        let provider: P2pPieceProvider = Arc::new(|_| Ok(b"data".to_vec()));
        let registry = P2pSourceRegistry::default();
        let id = registry.register(source, manifest, provider).unwrap();
        assert!(registry.verify_complete(id).is_err());
    }

    #[test]
    fn parses_strict_authorized_manifest_json() {
        let content = b"data";
        let json = serde_json::json!({
            "content_id": "asset-json",
            "content_length": 4,
            "content_sha256": sha256_hex(content),
            "piece_length": 4,
            "piece_sha256": [sha256_hex(content)],
            "authorization_reference": "license-42",
            "explicitly_authorized": true
        });
        let (source, manifest) =
            parse_authorized_manifest_json(&serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(source.content_id(), "asset-json");
        assert!(manifest.verify_piece(0, content).is_ok());
    }

    #[test]
    fn rejects_unknown_fields_unauthorized_and_oversized_json() {
        let base = serde_json::json!({
            "content_id": "asset-json",
            "content_length": 4,
            "content_sha256": sha256_hex(b"data"),
            "piece_length": 4,
            "piece_sha256": [sha256_hex(b"data")],
            "authorization_reference": "license-42",
            "explicitly_authorized": false,
            "unexpected": true
        });
        assert!(parse_authorized_manifest_json(&serde_json::to_vec(&base).unwrap()).is_err());
        assert!(parse_authorized_manifest_json(&vec![b'x'; MAX_MANIFEST_JSON_BYTES + 1]).is_err());
        assert!(parse_authorized_manifest_json(b"").is_err());
    }

    #[test]
    fn repeated_ranges_reuse_verified_piece_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let source =
            AuthorizedP2pSource::new("asset", 4, &sha256_hex(b"data"), "license", true).unwrap();
        let manifest = P2pPieceManifest::new(4, 4, vec![sha256_hex(b"data")]).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = calls.clone();
        let provider: P2pPieceProvider = Arc::new(move |_| {
            provider_calls.fetch_add(1, Ordering::SeqCst);
            Ok(b"data".to_vec())
        });
        let registry = P2pSourceRegistry::default();
        let id = registry.register(source, manifest, provider).unwrap();
        assert_eq!(registry.read_range(id, 0, 1).unwrap(), b"da");
        assert_eq!(registry.read_range(id, 2, 3).unwrap(), b"ta");
        assert!(registry.verify_complete(id).is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_ranges_fetch_each_piece_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let source =
            AuthorizedP2pSource::new("asset", 4, &sha256_hex(b"data"), "license", true).unwrap();
        let manifest = P2pPieceManifest::new(4, 4, vec![sha256_hex(b"data")]).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = calls.clone();
        let provider: P2pPieceProvider = Arc::new(move |_| {
            provider_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(10));
            Ok(b"data".to_vec())
        });
        let registry = P2pSourceRegistry::default();
        let id = registry.register(source, manifest, provider).unwrap();
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let registry = registry.clone();
                std::thread::spawn(move || registry.read_range(id, 0, 3).unwrap())
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), b"data");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn verified_piece_cache_survives_registry_restart() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let digest = sha256_hex(b"data");
        let source = AuthorizedP2pSource::new("asset", 4, &digest, "license", true).unwrap();
        let manifest = P2pPieceManifest::new(4, 4, vec![digest.clone()]).unwrap();
        let first_calls = Arc::new(AtomicUsize::new(0));
        let counter = first_calls.clone();
        let first_provider: P2pPieceProvider = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(b"data".to_vec())
        });
        let registry = P2pSourceRegistry::with_cache_dir(directory.path().to_path_buf());
        let id = registry
            .register(source.clone(), manifest.clone(), first_provider)
            .unwrap();
        assert_eq!(registry.read_range(id, 0, 3).unwrap(), b"data");
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        drop(registry);

        let second_calls = Arc::new(AtomicUsize::new(0));
        let counter = second_calls.clone();
        let second_provider: P2pPieceProvider = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(b"data".to_vec())
        });
        let registry = P2pSourceRegistry::with_cache_dir(directory.path().to_path_buf());
        let id = registry
            .register(source, manifest, second_provider)
            .unwrap();
        assert_eq!(registry.read_range(id, 0, 3).unwrap(), b"data");
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn corrupt_disk_piece_is_deleted_and_refetched() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let digest = sha256_hex(b"data");
        let content_dir = directory.path().join(&digest);
        std::fs::create_dir_all(&content_dir).unwrap();
        std::fs::write(content_dir.join("0.piece"), b"evil").unwrap();
        let source = AuthorizedP2pSource::new("asset", 4, &digest, "license", true).unwrap();
        let manifest = P2pPieceManifest::new(4, 4, vec![digest]).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let provider: P2pPieceProvider = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(b"data".to_vec())
        });
        let registry = P2pSourceRegistry::with_cache_dir(directory.path().to_path_buf());
        let id = registry.register(source, manifest, provider).unwrap();
        assert_eq!(registry.read_range(id, 0, 3).unwrap(), b"data");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(content_dir.join("0.piece")).unwrap(), b"data");
    }

    #[test]
    fn concurrent_disk_writes_do_not_share_temporary_paths() {
        let directory = tempfile::tempdir().unwrap();
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let path = directory.path().to_path_buf();
                std::thread::spawn(move || write_verified_disk_piece(&path, 0, b"data"))
            })
            .collect();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert_eq!(
            std::fs::read(directory.path().join("0.piece")).unwrap(),
            b"data"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn disk_cache_limit_evicts_oldest_verified_pieces() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("0.piece"), b"old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(second.join("0.piece"), b"new").unwrap();
        let cache = P2pDiskCache {
            root: directory.path().to_path_buf(),
            max_bytes: 3,
            maintenance: Mutex::new(()),
        };
        enforce_disk_cache_limit(&cache).unwrap();
        assert!(!first.join("0.piece").exists());
        assert_eq!(std::fs::read(second.join("0.piece")).unwrap(), b"new");
    }

    #[test]
    fn fresh_disk_temporary_files_are_preserved_on_startup() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("content");
        std::fs::create_dir_all(&source).unwrap();
        let temporary = source.join(".0.1.2.piece.tmp");
        std::fs::write(&temporary, b"in progress").unwrap();
        cleanup_stale_disk_temps(directory.path());
        assert!(temporary.exists());
    }
}
