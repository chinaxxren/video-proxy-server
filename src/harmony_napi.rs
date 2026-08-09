//! HarmonyOS N-API ownership bridge for the ArkTS adapter.

#[cfg(feature = "p2p")]
use crate::ffi::{
    proxy_p2p_source_register_directory, proxy_p2p_source_remove, proxy_p2p_source_verify_complete,
};
use crate::ffi::{
    proxy_server_create_with_hosts, proxy_server_destroy, proxy_server_start, proxy_server_stop,
    ProxyServerHandle,
};
use crate::harmony_config::HarmonyConfiguration;
use napi::{Error, Result, Status};
use napi_derive::napi;
use std::ffi::CString;
use std::sync::Mutex;

#[cfg(feature = "p2p")]
fn register_p2p_directory(
    handle: *mut ProxyServerHandle,
    manifest_json: &str,
    piece_directory: &CString,
) -> u64 {
    unsafe {
        proxy_p2p_source_register_directory(
            handle,
            manifest_json.as_ptr(),
            manifest_json.len(),
            piece_directory.as_ptr(),
        )
    }
}

#[cfg(not(feature = "p2p"))]
fn register_p2p_directory(
    _handle: *mut ProxyServerHandle,
    _manifest_json: &str,
    _piece_directory: &CString,
) -> u64 {
    0
}

#[cfg(feature = "p2p")]
fn verify_p2p_source(handle: *mut ProxyServerHandle, source_id: u64) -> bool {
    unsafe { proxy_p2p_source_verify_complete(handle, source_id) != 0 }
}

#[cfg(not(feature = "p2p"))]
fn verify_p2p_source(_handle: *mut ProxyServerHandle, _source_id: u64) -> bool {
    false
}

#[cfg(feature = "p2p")]
fn remove_p2p_source(handle: *mut ProxyServerHandle, source_id: u64) -> bool {
    unsafe { proxy_p2p_source_remove(handle, source_id) != 0 }
}

#[cfg(not(feature = "p2p"))]
fn remove_p2p_source(_handle: *mut ProxyServerHandle, _source_id: u64) -> bool {
    false
}

#[napi]
pub struct MediaProxyCache {
    handle: Mutex<Option<usize>>,
}

#[napi]
impl MediaProxyCache {
    #[napi(factory)]
    pub fn create(port: u32, cache_directory: String, allowed_hosts: Vec<String>) -> Result<Self> {
        let configuration = HarmonyConfiguration::parse(port, cache_directory, allowed_hosts)
            .map_err(|message| Error::new(Status::InvalidArg, message))?;
        let handle = unsafe {
            proxy_server_create_with_hosts(
                configuration.port,
                configuration.cache_directory.as_ptr(),
                configuration.allowed_hosts.as_ptr(),
            )
        };
        if handle.is_null() {
            return Err(Error::new(Status::GenericFailure, "proxy creation failed"));
        }
        Ok(Self {
            handle: Mutex::new(Some(handle as usize)),
        })
    }

    #[napi]
    pub fn start(&self) -> Result<u32> {
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?;
        let handle = handle.ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        let port = unsafe { proxy_server_start(handle as *mut ProxyServerHandle) };
        if port == 0 {
            Err(Error::new(Status::GenericFailure, "proxy start failed"))
        } else {
            Ok(port as u32)
        }
    }

    #[napi]
    pub fn stop(&self) -> Result<()> {
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?;
        if let Some(handle) = *handle {
            unsafe { proxy_server_stop(handle as *mut ProxyServerHandle) }
        }
        Ok(())
    }

    #[napi]
    pub fn close(&self) -> Result<()> {
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .take();
        if let Some(handle) = handle {
            unsafe { proxy_server_destroy(handle as *mut ProxyServerHandle) }
        }
        Ok(())
    }

    #[napi]
    pub fn register_p2p_directory(
        &self,
        manifest_json: String,
        piece_directory: String,
    ) -> Result<String> {
        if manifest_json.trim().is_empty() || piece_directory.trim().is_empty() {
            return Err(Error::new(Status::InvalidArg, "invalid P2P source"));
        }
        let piece_directory = CString::new(piece_directory)
            .map_err(|_| Error::new(Status::InvalidArg, "invalid P2P piece directory"))?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        let source_id = register_p2p_directory(
            handle as *mut ProxyServerHandle,
            &manifest_json,
            &piece_directory,
        );
        if source_id == 0 {
            return Err(Error::new(
                Status::GenericFailure,
                if cfg!(feature = "p2p") {
                    "P2P source registration failed"
                } else {
                    "P2P is not enabled in this native library"
                },
            ));
        }
        Ok(source_id.to_string())
    }

    #[napi]
    pub fn verify_p2p_source(&self, source_id: String) -> Result<bool> {
        let source_id = parse_source_id(&source_id)?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        Ok(verify_p2p_source(
            handle as *mut ProxyServerHandle,
            source_id,
        ))
    }

    #[napi]
    pub fn remove_p2p_source(&self, source_id: String) -> Result<bool> {
        let source_id = parse_source_id(&source_id)?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        Ok(remove_p2p_source(
            handle as *mut ProxyServerHandle,
            source_id,
        ))
    }

    fn close_for_drop(&self) {
        let handle = match self.handle.lock() {
            Ok(mut handle) => handle.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(handle) = handle {
            unsafe { proxy_server_destroy(handle as *mut ProxyServerHandle) }
        }
    }
}

fn parse_source_id(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|source_id| *source_id != 0)
        .ok_or_else(|| Error::new(Status::InvalidArg, "invalid P2P source ID"))
}

impl Drop for MediaProxyCache {
    fn drop(&mut self) {
        self.close_for_drop();
    }
}
