//! Optional production BitTorrent backend powered by librqbit.

use crate::utils::error::{ProxyError, Result};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions};
use std::path::PathBuf;
use std::sync::Arc;

pub struct RqbitBackend {
    session: Arc<Session>,
}

impl RqbitBackend {
    pub async fn new(cache_directory: PathBuf) -> Result<Self> {
        if !cache_directory.is_absolute() {
            return Err(ProxyError::Storage("librqbit cache directory must be absolute".into()));
        }
        std::fs::create_dir_all(&cache_directory).map_err(|error| ProxyError::Storage(format!("create librqbit cache failed: {error}")))?;
        let options = SessionOptions {
            disable_upload: true,
            listen_port_range: None,
            enable_upnp_port_forwarding: false,
            fastresume: true,
            ..Default::default()
        };
        let session = Session::new_with_opts(cache_directory, options).await.map_err(|error| ProxyError::Network(format!("create librqbit session failed: {error:#}")))?;
        Ok(Self { session })
    }

    pub async fn add_authorized_magnet(&self, magnet: &str, explicitly_authorized: bool) -> Result<usize> {
        if !explicitly_authorized {
            return Err(ProxyError::Request("magnet download is not authorized".into()));
        }
        crate::p2p_network::parse_magnet_uri(magnet)?;
        let response = self.session.add_torrent(AddTorrent::from_url(magnet), Some(AddTorrentOptions { overwrite: false, ..Default::default() })).await.map_err(|error| ProxyError::Network(format!("add magnet failed: {error:#}")))?;
        match response {
            AddTorrentResponse::Added(id, _) | AddTorrentResponse::AlreadyManaged(id, _) => Ok(id),
            AddTorrentResponse::ListOnly(_) => Err(ProxyError::Request("magnet was unexpectedly list-only".into())),
        }
    }

    pub fn shutdown(&self) {
        self.session.cancellation_token().cancel();
    }
}
