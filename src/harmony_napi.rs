//! HarmonyOS N-API ownership bridge for the ArkTS adapter.

#[cfg(feature = "p2p")]
use crate::ffi::{
    proxy_p2p_source_register_directory, proxy_p2p_source_remove, proxy_p2p_source_verify_complete,
};
use crate::ffi::{
    proxy_server_create_with_hosts, proxy_server_destroy, proxy_server_start, proxy_server_stop,
    proxy_source_refresh, proxy_source_register, proxy_source_remove,
    proxy_source_set_refresh_callback, ProxyServerHandle,
};
#[cfg(feature = "p2p-librqbit")]
use crate::ffi::{
    proxy_torrent_add_authorized, proxy_torrent_add_file_authorized, proxy_torrent_files_json,
    proxy_torrent_remove, proxy_torrent_select_files, proxy_torrent_set_download_limit,
    proxy_torrent_set_paused, proxy_torrent_status_json,
};
use crate::harmony_config::HarmonyConfiguration;
#[cfg(feature = "p2p-librqbit")]
use napi::bindgen_prelude::Uint8Array;
use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Error, Result, Status};
use napi_derive::napi;
use std::ffi::{c_char, c_void, CString};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

type HarmonyRefreshFunction = ThreadsafeFunction<(String,), ErrorStrategy::Fatal>;

struct HarmonyRefreshContext {
    provider: HarmonyRefreshFunction,
    buffers: Mutex<Vec<CString>>,
}

unsafe extern "C" fn harmony_refresh_callback(
    context: *mut c_void,
    source_id: u64,
) -> *const c_char {
    let Some(context) = (context as *const HarmonyRefreshContext).as_ref() else {
        return std::ptr::null();
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let status = context.provider.call_with_return_value_raw::<String, _>(
        (source_id.to_string(),),
        ThreadsafeFunctionCallMode::Blocking,
        move |result| {
            let _ = sender.send(result.ok());
            Ok(())
        },
    );
    if status != Status::Ok {
        return std::ptr::null();
    }
    let Ok(Some(value)) = receiver.recv_timeout(Duration::from_secs(10)) else {
        return std::ptr::null();
    };
    let Ok(value) = CString::new(value) else {
        return std::ptr::null();
    };
    let pointer = value.as_ptr();
    let Ok(mut buffers) = context.buffers.lock() else {
        return std::ptr::null();
    };
    buffers.push(value);
    pointer
}

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

#[cfg(feature = "p2p-librqbit")]
fn add_authorized_torrent(handle: *mut ProxyServerHandle, magnet_uri: &CString) -> i64 {
    unsafe { proxy_torrent_add_authorized(handle, magnet_uri.as_ptr(), 1) }
}

#[cfg(not(feature = "p2p-librqbit"))]
fn add_authorized_torrent(_handle: *mut ProxyServerHandle, _magnet_uri: &CString) -> i64 {
    -1
}

#[cfg(feature = "p2p-librqbit")]
fn remove_torrent(handle: *mut ProxyServerHandle, torrent_id: i64, delete_files: bool) -> bool {
    unsafe { proxy_torrent_remove(handle, torrent_id, u8::from(delete_files)) != 0 }
}

#[cfg(not(feature = "p2p-librqbit"))]
fn remove_torrent(_handle: *mut ProxyServerHandle, _torrent_id: i64, _delete_files: bool) -> bool {
    false
}

#[cfg(feature = "p2p-librqbit")]
fn set_torrent_paused(handle: *mut ProxyServerHandle, torrent_id: i64, paused: bool) -> bool {
    unsafe { proxy_torrent_set_paused(handle, torrent_id, u8::from(paused)) != 0 }
}

#[cfg(not(feature = "p2p-librqbit"))]
fn set_torrent_paused(_handle: *mut ProxyServerHandle, _torrent_id: i64, _paused: bool) -> bool {
    false
}

#[cfg(feature = "p2p-librqbit")]
unsafe fn query_torrent_json(
    handle: *mut ProxyServerHandle,
    torrent_id: i64,
    query: unsafe extern "C" fn(*mut ProxyServerHandle, i64, *mut u8, usize) -> usize,
) -> Option<String> {
    let required = query(handle, torrent_id, std::ptr::null_mut(), 0);
    if required == 0 || required > 4 * 1024 * 1024 + 1 {
        return None;
    }
    let mut bytes = vec![0u8; required];
    if query(handle, torrent_id, bytes.as_mut_ptr(), bytes.len()) != required {
        return None;
    }
    bytes.pop();
    String::from_utf8(bytes).ok()
}

#[napi]
pub struct MediaProxyCache {
    handle: Mutex<Option<usize>>,
    // Boxes keep C callback context addresses stable as providers are replaced.
    #[allow(clippy::vec_box)]
    refresh_contexts: Mutex<Vec<Box<HarmonyRefreshContext>>>,
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
            refresh_contexts: Mutex::new(Vec::new()),
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
    pub fn register_source(&self, identity: String, url: String) -> Result<String> {
        let identity = CString::new(identity)
            .map_err(|_| Error::new(Status::InvalidArg, "invalid source identity"))?;
        let url =
            CString::new(url).map_err(|_| Error::new(Status::InvalidArg, "invalid source URL"))?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        let id = unsafe {
            proxy_source_register(
                handle as *mut ProxyServerHandle,
                identity.as_ptr(),
                url.as_ptr(),
            )
        };
        if id == 0 {
            return Err(Error::new(Status::InvalidArg, "source registration failed"));
        }
        Ok(id.to_string())
    }

    #[napi]
    pub fn refresh_source(&self, source_id: String, url: String) -> Result<bool> {
        let source_id = parse_source_id(&source_id)?;
        let url =
            CString::new(url).map_err(|_| Error::new(Status::InvalidArg, "invalid source URL"))?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        Ok(unsafe {
            proxy_source_refresh(handle as *mut ProxyServerHandle, source_id, url.as_ptr()) != 0
        })
    }

    #[napi]
    pub fn remove_source(&self, source_id: String) -> Result<bool> {
        let source_id = parse_source_id(&source_id)?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        Ok(unsafe { proxy_source_remove(handle as *mut ProxyServerHandle, source_id) != 0 })
    }

    #[napi]
    pub fn set_source_refresh_provider(&self, provider: HarmonyRefreshFunction) -> Result<bool> {
        let handle_guard = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?;
        let handle =
            handle_guard.ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        let mut contexts = self
            .refresh_contexts
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "refresh contexts unavailable"))?;
        let context = Box::new(HarmonyRefreshContext {
            provider,
            buffers: Mutex::new(Vec::new()),
        });
        let context_pointer = (&*context as *const HarmonyRefreshContext)
            .cast_mut()
            .cast();
        let installed = unsafe {
            proxy_source_set_refresh_callback(
                handle as *mut ProxyServerHandle,
                Some(harmony_refresh_callback),
                context_pointer,
            ) != 0
        };
        if installed {
            contexts.push(context);
        }
        Ok(installed)
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
        if let Ok(mut contexts) = self.refresh_contexts.lock() {
            contexts.clear();
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

    #[napi]
    pub fn add_authorized_torrent(&self, magnet_uri: String) -> Result<String> {
        if !magnet_uri.starts_with("magnet:?") {
            return Err(Error::new(Status::InvalidArg, "invalid Magnet URI"));
        }
        let magnet_uri = CString::new(magnet_uri)
            .map_err(|_| Error::new(Status::InvalidArg, "invalid Magnet URI"))?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        let torrent_id = add_authorized_torrent(handle as *mut ProxyServerHandle, &magnet_uri);
        if torrent_id < 0 {
            return Err(Error::new(
                Status::GenericFailure,
                if cfg!(feature = "p2p-librqbit") {
                    "torrent registration failed"
                } else {
                    "librqbit is not enabled in this native library"
                },
            ));
        }
        Ok(torrent_id.to_string())
    }

    #[cfg(feature = "p2p-librqbit")]
    #[napi]
    pub fn add_authorized_torrent_file(&self, torrent_bytes: Uint8Array) -> Result<String> {
        let bytes = torrent_bytes.as_ref();
        if bytes.is_empty() || bytes.len() > 4 * 1024 * 1024 {
            return Err(Error::new(
                Status::InvalidArg,
                "invalid torrent metadata length",
            ));
        }
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        let torrent_id = unsafe {
            proxy_torrent_add_file_authorized(
                handle as *mut ProxyServerHandle,
                bytes.as_ptr(),
                bytes.len(),
                1,
            )
        };
        if torrent_id < 0 {
            return Err(Error::new(
                Status::GenericFailure,
                "torrent file registration failed",
            ));
        }
        Ok(torrent_id.to_string())
    }

    #[napi]
    pub fn remove_torrent(&self, torrent_id: String, delete_files: bool) -> Result<bool> {
        let torrent_id = torrent_id
            .parse::<i64>()
            .ok()
            .filter(|torrent_id| *torrent_id >= 0)
            .ok_or_else(|| Error::new(Status::InvalidArg, "invalid torrent ID"))?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        Ok(remove_torrent(
            handle as *mut ProxyServerHandle,
            torrent_id,
            delete_files,
        ))
    }

    #[cfg(feature = "p2p-librqbit")]
    #[napi]
    pub fn torrent_files_json(&self, torrent_id: String) -> Result<String> {
        self.torrent_json(&torrent_id, proxy_torrent_files_json)
    }

    #[cfg(feature = "p2p-librqbit")]
    #[napi]
    pub fn torrent_status_json(&self, torrent_id: String) -> Result<String> {
        self.torrent_json(&torrent_id, proxy_torrent_status_json)
    }

    #[cfg(feature = "p2p-librqbit")]
    #[napi]
    pub fn select_torrent_files(&self, torrent_id: String, file_ids: Vec<u32>) -> Result<bool> {
        let torrent_id = torrent_id
            .parse::<i64>()
            .ok()
            .filter(|id| *id >= 0)
            .ok_or_else(|| Error::new(Status::InvalidArg, "invalid torrent ID"))?;
        if file_ids.is_empty() || file_ids.len() > 4096 {
            return Err(Error::new(Status::InvalidArg, "invalid file selection"));
        }
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        Ok(unsafe {
            proxy_torrent_select_files(
                handle as *mut ProxyServerHandle,
                torrent_id,
                file_ids.as_ptr(),
                file_ids.len(),
            ) != 0
        })
    }

    #[napi]
    pub fn set_torrent_paused(&self, torrent_id: String, paused: bool) -> Result<bool> {
        let torrent_id = torrent_id
            .parse::<i64>()
            .ok()
            .filter(|torrent_id| *torrent_id >= 0)
            .ok_or_else(|| Error::new(Status::InvalidArg, "invalid torrent ID"))?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        Ok(set_torrent_paused(
            handle as *mut ProxyServerHandle,
            torrent_id,
            paused,
        ))
    }

    #[cfg(feature = "p2p-librqbit")]
    #[napi]
    pub fn set_torrent_download_limit(&self, bytes_per_second: u32) -> Result<bool> {
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        Ok(unsafe {
            proxy_torrent_set_download_limit(handle as *mut ProxyServerHandle, bytes_per_second)
                != 0
        })
    }

    #[cfg(feature = "p2p-librqbit")]
    fn torrent_json(
        &self,
        torrent_id: &str,
        query: unsafe extern "C" fn(*mut ProxyServerHandle, i64, *mut u8, usize) -> usize,
    ) -> Result<String> {
        let torrent_id = torrent_id
            .parse::<i64>()
            .ok()
            .filter(|torrent_id| *torrent_id >= 0)
            .ok_or_else(|| Error::new(Status::InvalidArg, "invalid torrent ID"))?;
        let handle = self
            .handle
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "proxy handle unavailable"))?
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        unsafe { query_torrent_json(handle as *mut ProxyServerHandle, torrent_id, query) }
            .ok_or_else(|| Error::new(Status::GenericFailure, "torrent metadata unavailable"))
    }

    fn close_for_drop(&self) {
        let handle = match self.handle.lock() {
            Ok(mut handle) => handle.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(handle) = handle {
            unsafe { proxy_server_destroy(handle as *mut ProxyServerHandle) }
        }
        match self.refresh_contexts.lock() {
            Ok(mut contexts) => contexts.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
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
