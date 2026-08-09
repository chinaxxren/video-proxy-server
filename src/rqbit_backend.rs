//! Optional production BitTorrent backend powered by librqbit.

use crate::utils::error::{ProxyError, Result};
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::RwLock;

pub struct RqbitBackend {
    session: Arc<Session>,
    torrents: RwLock<HashMap<usize, Arc<ManagedTorrent>>>,
}

impl RqbitBackend {
    pub async fn new(cache_directory: PathBuf) -> Result<Self> {
        if !cache_directory.is_absolute() {
            return Err(ProxyError::Storage(
                "librqbit cache directory must be absolute".into(),
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
        crate::p2p_network::parse_magnet_uri(magnet)?;
        let response = self
            .session
            .add_torrent(
                AddTorrent::from_url(magnet),
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

    pub fn shutdown(&self) {
        self.session.cancellation_token().cancel();
    }
}
