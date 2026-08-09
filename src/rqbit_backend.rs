//! Optional production BitTorrent backend powered by librqbit.

use crate::utils::error::{ProxyError, Result};
use librqbit::{
    api::TorrentIdOrHash, AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent,
    Session, SessionOptions,
};
use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{Mutex, RwLock};

#[derive(Clone, Debug)]
pub struct RqbitBackendConfig {
    pub cache_directory: PathBuf,
    pub max_torrents: usize,
    pub download_bytes_per_second: Option<NonZeroU32>,
}

impl RqbitBackendConfig {
    pub fn new(cache_directory: PathBuf) -> Self {
        Self {
            cache_directory,
            max_torrents: 8,
            download_bytes_per_second: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RqbitFileInfo {
    pub file_id: usize,
    pub relative_path: String,
    pub length: u64,
    pub selected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RqbitTorrentStatus {
    pub state: String,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    pub finished: bool,
    pub error: Option<String>,
}

pub struct RqbitBackend {
    session: Arc<Session>,
    torrents: RwLock<HashMap<usize, Arc<ManagedTorrent>>>,
    info_hashes: RwLock<HashMap<[u8; 20], usize>>,
    add_lock: Mutex<()>,
    max_torrents: usize,
}

impl RqbitBackend {
    pub async fn new(cache_directory: PathBuf) -> Result<Self> {
        Self::with_config(RqbitBackendConfig::new(cache_directory)).await
    }

    pub async fn with_config(config: RqbitBackendConfig) -> Result<Self> {
        let cache_directory = config.cache_directory;
        if !cache_directory.is_absolute() {
            return Err(ProxyError::Storage(
                "librqbit cache directory must be absolute".into(),
            ));
        }
        if config.max_torrents == 0 {
            return Err(ProxyError::Request(
                "librqbit max_torrents must be greater than zero".into(),
            ));
        }
        std::fs::create_dir_all(&cache_directory).map_err(|error| {
            ProxyError::Storage(format!("create librqbit cache failed: {error}"))
        })?;
        let options = SessionOptions {
            disable_upload: true,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            fastresume: true,
            ratelimits: librqbit::limits::LimitsConfig {
                download_bps: config.download_bytes_per_second,
                upload_bps: None,
            },
            ..Default::default()
        };
        let session = Session::new_with_opts(cache_directory, options)
            .await
            .map_err(|error| {
                ProxyError::Network(format!("create librqbit session failed: {error:#}"))
            })?;
        Ok(Self {
            session,
            torrents: RwLock::new(HashMap::new()),
            info_hashes: RwLock::new(HashMap::new()),
            add_lock: Mutex::new(()),
            max_torrents: config.max_torrents,
        })
    }

    pub async fn add_authorized_magnet(
        &self,
        magnet: &str,
        explicitly_authorized: bool,
    ) -> Result<usize> {
        if !explicitly_authorized {
            return Err(ProxyError::Request(
                "magnet download is not authorized".into(),
            ));
        }
        let request = crate::p2p_network::parse_magnet_uri(magnet)?;
        self.add_authorized_source(AddTorrent::from_url(magnet), request.info_hash)
            .await
    }

    pub async fn add_authorized_torrent_bytes(
        &self,
        torrent_bytes: &[u8],
        explicitly_authorized: bool,
    ) -> Result<usize> {
        if !explicitly_authorized {
            return Err(ProxyError::Request(
                "torrent download is not authorized".into(),
            ));
        }
        if torrent_bytes.is_empty() || torrent_bytes.len() > 4 * 1024 * 1024 {
            return Err(ProxyError::Request(
                "torrent metadata length is invalid".into(),
            ));
        }
        let metadata = crate::p2p_network::parse_torrent_metadata(torrent_bytes)?;
        self.add_authorized_source(
            AddTorrent::from_bytes(torrent_bytes.to_vec()),
            metadata.info_hash,
        )
        .await
    }

    async fn add_authorized_source(
        &self,
        source: AddTorrent<'_>,
        info_hash: [u8; 20],
    ) -> Result<usize> {
        let _add_guard = self.add_lock.lock().await;
        if let Some(id) = self.info_hashes.read().await.get(&info_hash) {
            return Ok(*id);
        }
        if self.torrents.read().await.len() >= self.max_torrents {
            return Err(ProxyError::Request(format!(
                "librqbit torrent limit ({}) reached",
                self.max_torrents
            )));
        }
        let response = self
            .session
            .add_torrent(
                source,
                Some(AddTorrentOptions {
                    overwrite: false,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|error| ProxyError::Network(format!("add magnet failed: {error:#}")))?;
        match response {
            AddTorrentResponse::Added(id, handle)
            | AddTorrentResponse::AlreadyManaged(id, handle) => {
                handle.wait_until_initialized().await.map_err(|error| {
                    ProxyError::Network(format!("initialize magnet failed: {error:#}"))
                })?;
                self.torrents.write().await.insert(id, handle);
                self.info_hashes.write().await.insert(info_hash, id);
                Ok(id)
            }
            AddTorrentResponse::ListOnly(_) => Err(ProxyError::Request(
                "magnet was unexpectedly list-only".into(),
            )),
        }
    }

    pub async fn read_file_range(
        &self,
        torrent_id: usize,
        file_id: usize,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        if length == 0 || length > 8 * 1024 * 1024 {
            return Err(ProxyError::Request("invalid librqbit read length".into()));
        }
        let handle = self
            .torrents
            .read()
            .await
            .get(&torrent_id)
            .cloned()
            .ok_or_else(|| ProxyError::Request("unknown librqbit torrent ID".into()))?;
        let mut stream = handle.stream(file_id).map_err(|error| {
            ProxyError::Request(format!("open librqbit stream failed: {error:#}"))
        })?;
        stream
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|error| {
                ProxyError::Storage(format!("seek librqbit stream failed: {error}"))
            })?;
        let mut bytes = vec![0u8; length];
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            stream.read_exact(&mut bytes),
        )
        .await
        .map_err(|_| ProxyError::Network("librqbit range read timed out".into()))?
        .map_err(|error| ProxyError::Storage(format!("read librqbit range failed: {error}")))?;
        Ok(bytes)
    }

    pub async fn files(&self, torrent_id: usize) -> Result<Vec<RqbitFileInfo>> {
        let handle = self.torrent(torrent_id).await?;
        let only_files = handle.only_files();
        handle
            .with_metadata(|metadata| {
                metadata
                    .file_infos
                    .iter()
                    .enumerate()
                    .map(|(file_id, file)| RqbitFileInfo {
                        file_id,
                        relative_path: file.relative_filename.to_string_lossy().into_owned(),
                        length: file.len,
                        selected: only_files
                            .as_ref()
                            .map(|selected| selected.contains(&file_id))
                            .unwrap_or(true),
                    })
                    .collect()
            })
            .map_err(|error| {
                ProxyError::Request(format!("read librqbit metadata failed: {error:#}"))
            })
    }

    pub async fn status(&self, torrent_id: usize) -> Result<RqbitTorrentStatus> {
        let stats = self.torrent(torrent_id).await?.stats();
        Ok(RqbitTorrentStatus {
            state: stats.state.to_string(),
            total_bytes: stats.total_bytes,
            downloaded_bytes: stats.progress_bytes,
            uploaded_bytes: stats.uploaded_bytes,
            finished: stats.finished,
            error: stats.error,
        })
    }

    pub async fn pause(&self, torrent_id: usize) -> Result<()> {
        let handle = self.torrent(torrent_id).await?;
        self.session.pause(&handle).await.map_err(|error| {
            ProxyError::Network(format!("pause librqbit torrent failed: {error:#}"))
        })
    }

    pub async fn resume(&self, torrent_id: usize) -> Result<()> {
        let handle = self.torrent(torrent_id).await?;
        self.session.unpause(&handle).await.map_err(|error| {
            ProxyError::Network(format!("resume librqbit torrent failed: {error:#}"))
        })
    }

    pub fn set_download_limit(&self, bytes_per_second: Option<NonZeroU32>) {
        self.session.ratelimits.set_download_bps(bytes_per_second);
    }

    pub async fn select_files(&self, torrent_id: usize, file_ids: &[usize]) -> Result<()> {
        if file_ids.is_empty() || file_ids.len() > 4096 {
            return Err(ProxyError::Request(
                "librqbit selected file count is invalid".into(),
            ));
        }
        let selected: HashSet<_> = file_ids.iter().copied().collect();
        if selected.len() != file_ids.len() {
            return Err(ProxyError::Request(
                "librqbit selected file IDs contain duplicates".into(),
            ));
        }
        let handle = self.torrent(torrent_id).await?;
        self.session
            .update_only_files(&handle, &selected)
            .await
            .map_err(|error| {
                ProxyError::Request(format!("select librqbit files failed: {error:#}"))
            })
    }

    pub async fn remove(&self, torrent_id: usize, delete_files: bool) -> Result<()> {
        if !self.torrents.read().await.contains_key(&torrent_id) {
            return Err(ProxyError::Request("unknown librqbit torrent ID".into()));
        }
        self.session
            .delete(TorrentIdOrHash::Id(torrent_id), delete_files)
            .await
            .map_err(|error| {
                ProxyError::Storage(format!("remove librqbit torrent failed: {error:#}"))
            })?;
        self.torrents.write().await.remove(&torrent_id);
        self.info_hashes
            .write()
            .await
            .retain(|_, id| *id != torrent_id);
        Ok(())
    }

    async fn torrent(&self, torrent_id: usize) -> Result<Arc<ManagedTorrent>> {
        self.torrents
            .read()
            .await
            .get(&torrent_id)
            .cloned()
            .ok_or_else(|| ProxyError::Request("unknown librqbit torrent ID".into()))
    }

    pub fn shutdown(&self) {
        self.session.cancellation_token().cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_relative_cache_directory() {
        let error = RqbitBackend::new(PathBuf::from("relative-cache"))
            .await
            .err()
            .expect("relative directory must be rejected");
        assert!(error.to_string().contains("must be absolute"));
    }

    #[tokio::test]
    async fn rejects_zero_torrent_limit() {
        let cache = tempfile::tempdir().expect("create temporary cache");
        let error = RqbitBackend::with_config(RqbitBackendConfig {
            cache_directory: cache.path().to_path_buf(),
            max_torrents: 0,
            download_bytes_per_second: None,
        })
        .await
        .err()
        .expect("zero limit must be rejected");
        assert!(error.to_string().contains("max_torrents"));
    }

    #[tokio::test]
    async fn unknown_torrent_operations_return_errors() {
        let cache = tempfile::tempdir().expect("create temporary cache");
        let backend = RqbitBackend::new(cache.path().to_path_buf())
            .await
            .expect("create backend");

        assert!(backend.files(404).await.is_err());
        assert!(backend.status(404).await.is_err());
        assert!(backend.pause(404).await.is_err());
        assert!(backend.resume(404).await.is_err());
        assert!(backend.select_files(404, &[0]).await.is_err());
        assert!(backend.select_files(404, &[]).await.is_err());
        assert!(backend.select_files(404, &[0, 0]).await.is_err());
        assert!(backend.remove(404, false).await.is_err());
        assert!(backend
            .add_authorized_torrent_bytes(b"torrent", false)
            .await
            .is_err());
        assert!(backend
            .add_authorized_torrent_bytes(&[], true)
            .await
            .is_err());

        backend.shutdown();
    }
}
