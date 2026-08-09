//! Minimal C ABI for mobile adapters.
//!
//! The ABI owns no caller memory: strings are copied during construction and
//! the returned handle remains valid until `proxy_server_destroy` is called.

use crate::server::{ProxyConfig, ProxyServer};
#[cfg(feature = "p2p")]
use std::ffi::c_void;
use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::ptr;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

#[cfg(feature = "p2p-librqbit")]
enum RqbitCommand {
    Add {
        magnet: String,
        authorized: bool,
        reply: mpsc::SyncSender<Option<usize>>,
    },
    Remove {
        torrent_id: usize,
        delete_files: bool,
        reply: mpsc::SyncSender<bool>,
    },
    Files {
        torrent_id: usize,
        reply: mpsc::SyncSender<Option<String>>,
    },
    Status {
        torrent_id: usize,
        reply: mpsc::SyncSender<Option<String>>,
    },
    SetPaused {
        torrent_id: usize,
        paused: bool,
        reply: mpsc::SyncSender<bool>,
    },
    SetDownloadLimit {
        bytes_per_second: u32,
        reply: mpsc::SyncSender<bool>,
    },
}

pub struct ProxyServerHandle {
    config: Mutex<Option<ProxyConfig>>,
    server: Arc<Mutex<Option<Arc<ProxyServer>>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    #[cfg(feature = "p2p")]
    p2p_sources: crate::p2p::P2pSourceRegistry,
    #[cfg(feature = "p2p-librqbit")]
    rqbit_commands: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<RqbitCommand>>>>,
}

/// # Safety
///
/// `value` 必须为空指针，或指向一个以 NUL 结尾、在本次调用期间保持有效且
/// 不被其他线程改写的 C 字符串。
unsafe fn read_string(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    CStr::from_ptr(value).to_str().ok().map(str::to_owned)
}

/// Creates a server handle. `cache_dir` must be a valid UTF-8, NUL-terminated path.
///
/// 返回的句柄归调用方所有，必须交给 `proxy_server_destroy` 释放。路径为空指针
/// 或非 UTF-8 时返回空指针。
///
/// # Safety
///
/// `cache_dir` 必须为空指针，或指向一个以 NUL 结尾、在本次调用期间保持有效的
/// C 字符串。
#[no_mangle]
pub unsafe extern "C" fn proxy_server_create(
    port: u16,
    cache_dir: *const c_char,
) -> *mut ProxyServerHandle {
    let Some(cache_dir) = read_string(cache_dir) else {
        return ptr::null_mut();
    };
    create_handle(port, cache_dir, Vec::new())
}

/// Creates a server handle with a comma-separated upstream host allowlist.
/// Empty items and surrounding ASCII whitespace are ignored.
///
/// # Safety
///
/// `cache_dir` and `allowed_hosts` must point to valid NUL-terminated strings
/// for the duration of this call. Either pointer may be null, in which case
/// creation fails.
#[no_mangle]
pub unsafe extern "C" fn proxy_server_create_with_hosts(
    port: u16,
    cache_dir: *const c_char,
    allowed_hosts: *const c_char,
) -> *mut ProxyServerHandle {
    let (Some(cache_dir), Some(allowed_hosts)) =
        (read_string(cache_dir), read_string(allowed_hosts))
    else {
        return ptr::null_mut();
    };
    let allowed_hosts = allowed_hosts
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect();
    create_handle(port, cache_dir, allowed_hosts)
}

fn create_handle(
    port: u16,
    cache_dir: String,
    allowed_hosts: Vec<String>,
) -> *mut ProxyServerHandle {
    let config = ProxyConfig {
        port,
        cache_dir: PathBuf::from(cache_dir),
        allowed_hosts,
        ..Default::default()
    };
    #[cfg(feature = "p2p")]
    let p2p_sources = crate::p2p::P2pSourceRegistry::with_cache_limit(
        config.cache_dir.join("p2p"),
        config.max_cache_bytes,
    );
    Box::into_raw(Box::new(ProxyServerHandle {
        config: Mutex::new(Some(config)),
        server: Arc::new(Mutex::new(None)),
        thread: Mutex::new(None),
        #[cfg(feature = "p2p")]
        p2p_sources,
        #[cfg(feature = "p2p-librqbit")]
        rqbit_commands: Arc::new(Mutex::new(None)),
    }))
}

#[cfg(feature = "p2p-librqbit")]
const RQBIT_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(feature = "p2p-librqbit")]
const MAX_RQBIT_JSON_BYTES: usize = 4 * 1024 * 1024;

/// Adds an explicitly authorized Magnet URI. Returns the non-negative torrent
/// ID, or -1 when validation, initialization, or command dispatch fails.
///
/// # Safety
///
/// `handle` must be null or live, and `magnet` must be a valid NUL-terminated
/// UTF-8 string for the duration of this call.
#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub unsafe extern "C" fn proxy_torrent_add_authorized(
    handle: *mut ProxyServerHandle,
    magnet: *const c_char,
    explicitly_authorized: u8,
) -> i64 {
    let (Some(handle), Some(magnet)) = (handle.as_ref(), read_string(magnet)) else {
        return -1;
    };
    let Some(commands) = handle
        .rqbit_commands
        .lock()
        .ok()
        .and_then(|commands| commands.clone())
    else {
        return -1;
    };
    let (reply, response) = mpsc::sync_channel(1);
    if commands
        .send(RqbitCommand::Add {
            magnet,
            authorized: explicitly_authorized == 1,
            reply,
        })
        .is_err()
    {
        return -1;
    }
    response
        .recv_timeout(RQBIT_COMMAND_TIMEOUT)
        .ok()
        .flatten()
        .and_then(|id| i64::try_from(id).ok())
        .unwrap_or(-1)
}

/// Forgets a torrent and optionally removes its downloaded files.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by a create function.
#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub unsafe extern "C" fn proxy_torrent_remove(
    handle: *mut ProxyServerHandle,
    torrent_id: i64,
    delete_files: u8,
) -> u8 {
    let Some(handle) = handle.as_ref() else {
        return 0;
    };
    let Ok(torrent_id) = usize::try_from(torrent_id) else {
        return 0;
    };
    let Some(commands) = handle
        .rqbit_commands
        .lock()
        .ok()
        .and_then(|commands| commands.clone())
    else {
        return 0;
    };
    let (reply, response) = mpsc::sync_channel(1);
    if commands
        .send(RqbitCommand::Remove {
            torrent_id,
            delete_files: delete_files == 1,
            reply,
        })
        .is_err()
    {
        return 0;
    }
    u8::from(
        response
            .recv_timeout(RQBIT_COMMAND_TIMEOUT)
            .unwrap_or(false),
    )
}

#[cfg(feature = "p2p-librqbit")]
fn query_rqbit_json(
    handle: &ProxyServerHandle,
    command: impl FnOnce(mpsc::SyncSender<Option<String>>) -> RqbitCommand,
) -> Option<String> {
    let commands = handle
        .rqbit_commands
        .lock()
        .ok()
        .and_then(|commands| commands.clone())?;
    let (reply, response) = mpsc::sync_channel(1);
    commands.send(command(reply)).ok()?;
    response
        .recv_timeout(RQBIT_COMMAND_TIMEOUT)
        .ok()
        .flatten()
        .filter(|json| json.len() <= MAX_RQBIT_JSON_BYTES)
}

/// Writes the torrent file list as UTF-8 JSON. The return value is the required
/// capacity including the trailing NUL. Pass a null buffer to query capacity.
/// Returns zero on failure. No partial output is written.
///
/// # Safety
///
/// `handle` must be null or live. A non-null `buffer` must reference at least
/// `capacity` writable bytes for the duration of this call.
#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub unsafe extern "C" fn proxy_torrent_files_json(
    handle: *mut ProxyServerHandle,
    torrent_id: i64,
    buffer: *mut u8,
    capacity: usize,
) -> usize {
    let (Some(handle), Ok(torrent_id)) = (handle.as_ref(), usize::try_from(torrent_id)) else {
        return 0;
    };
    let Some(json) = query_rqbit_json(handle, |reply| RqbitCommand::Files { torrent_id, reply })
    else {
        return 0;
    };
    write_ffi_json(&json, buffer, capacity)
}

/// Writes torrent progress/status as UTF-8 JSON. Uses the same capacity-query
/// contract as `proxy_torrent_files_json`.
///
/// # Safety
///
/// `handle` must be null or live. A non-null `buffer` must reference at least
/// `capacity` writable bytes for the duration of this call.
#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub unsafe extern "C" fn proxy_torrent_status_json(
    handle: *mut ProxyServerHandle,
    torrent_id: i64,
    buffer: *mut u8,
    capacity: usize,
) -> usize {
    let (Some(handle), Ok(torrent_id)) = (handle.as_ref(), usize::try_from(torrent_id)) else {
        return 0;
    };
    let Some(json) = query_rqbit_json(handle, |reply| RqbitCommand::Status { torrent_id, reply })
    else {
        return 0;
    };
    write_ffi_json(&json, buffer, capacity)
}

/// Pauses or resumes an initialized torrent. Returns 1 on success.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by a create function.
#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub unsafe extern "C" fn proxy_torrent_set_paused(
    handle: *mut ProxyServerHandle,
    torrent_id: i64,
    paused: u8,
) -> u8 {
    let (Some(handle), Ok(torrent_id)) = (handle.as_ref(), usize::try_from(torrent_id)) else {
        return 0;
    };
    let Some(commands) = handle
        .rqbit_commands
        .lock()
        .ok()
        .and_then(|commands| commands.clone())
    else {
        return 0;
    };
    let (reply, response) = mpsc::sync_channel(1);
    if commands
        .send(RqbitCommand::SetPaused {
            torrent_id,
            paused: paused == 1,
            reply,
        })
        .is_err()
    {
        return 0;
    }
    u8::from(
        response
            .recv_timeout(RQBIT_COMMAND_TIMEOUT)
            .unwrap_or(false),
    )
}

/// Sets the session-wide BitTorrent download limit in bytes per second.
/// Passing zero removes the limit. Returns 1 when the backend is active.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by a create function.
#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub unsafe extern "C" fn proxy_torrent_set_download_limit(
    handle: *mut ProxyServerHandle,
    bytes_per_second: u32,
) -> u8 {
    let Some(handle) = handle.as_ref() else {
        return 0;
    };
    let Some(commands) = handle
        .rqbit_commands
        .lock()
        .ok()
        .and_then(|commands| commands.clone())
    else {
        return 0;
    };
    let (reply, response) = mpsc::sync_channel(1);
    if commands
        .send(RqbitCommand::SetDownloadLimit {
            bytes_per_second,
            reply,
        })
        .is_err()
    {
        return 0;
    }
    u8::from(
        response
            .recv_timeout(RQBIT_COMMAND_TIMEOUT)
            .unwrap_or(false),
    )
}

#[cfg(feature = "p2p-librqbit")]
unsafe fn write_ffi_json(json: &str, buffer: *mut u8, capacity: usize) -> usize {
    let Some(required) = json.len().checked_add(1) else {
        return 0;
    };
    if buffer.is_null() || capacity < required {
        return required;
    }
    ptr::copy_nonoverlapping(json.as_ptr(), buffer, json.len());
    buffer.add(json.len()).write(0);
    required
}

#[cfg(feature = "p2p")]
pub type ProxyP2pPieceCallback = unsafe extern "C" fn(
    context: *mut c_void,
    piece_index: usize,
    buffer: *mut u8,
    capacity: usize,
) -> usize;

#[cfg(feature = "p2p")]
const MAX_P2P_CALLBACK_PIECE_BYTES: usize = 8 * 1024 * 1024;

/// Registers an authorized P2P manifest and Host piece callback.
///
/// # Safety
///
/// `handle`, `manifest_json`, callback, and context must remain valid as
/// documented by the C header. The callback may run on arbitrary Core threads.
#[cfg(feature = "p2p")]
#[no_mangle]
pub unsafe extern "C" fn proxy_p2p_source_register(
    handle: *mut ProxyServerHandle,
    manifest_json: *const u8,
    manifest_length: usize,
    callback: Option<ProxyP2pPieceCallback>,
    context: *mut c_void,
) -> u64 {
    let (Some(handle), Some(callback)) = (handle.as_ref(), callback) else {
        return 0;
    };
    if manifest_json.is_null() || manifest_length == 0 {
        return 0;
    }
    let json = std::slice::from_raw_parts(manifest_json, manifest_length);
    let Ok((source, manifest)) = crate::p2p::parse_authorized_manifest_json(json) else {
        return 0;
    };
    let context = context as usize;
    let provider: crate::p2p::P2pPieceProvider = Arc::new(move |piece_index| {
        let required = unsafe { callback(context as *mut c_void, piece_index, ptr::null_mut(), 0) };
        if required == 0 || required > MAX_P2P_CALLBACK_PIECE_BYTES {
            return Err(crate::utils::error::ProxyError::Request(
                "P2P Host piece length is invalid".to_string(),
            ));
        }
        let mut bytes = vec![0u8; required];
        let written = unsafe {
            callback(
                context as *mut c_void,
                piece_index,
                bytes.as_mut_ptr(),
                bytes.len(),
            )
        };
        if written != required {
            return Err(crate::utils::error::ProxyError::Request(
                "P2P Host piece write length mismatch".to_string(),
            ));
        }
        Ok(bytes)
    });
    handle
        .p2p_sources
        .register(source, manifest, provider)
        .unwrap_or(0)
}

/// Registers an authorized P2P manifest backed by files named
/// `<piece_index>.piece` in a Host-owned directory.
///
/// This entry point is intended for managed runtimes that cannot safely expose
/// a synchronous callback on arbitrary Core threads. Piece bytes are still
/// subject to the manifest digest and size checks before they can be served.
///
/// # Safety
///
/// `handle` must be a live handle. `manifest_json` must reference
/// `manifest_length` readable bytes, and `piece_directory` must be a valid
/// NUL-terminated UTF-8 string for the duration of this call.
#[cfg(feature = "p2p")]
#[no_mangle]
pub unsafe extern "C" fn proxy_p2p_source_register_directory(
    handle: *mut ProxyServerHandle,
    manifest_json: *const u8,
    manifest_length: usize,
    piece_directory: *const c_char,
) -> u64 {
    let (Some(handle), Some(piece_directory)) = (handle.as_ref(), read_string(piece_directory))
    else {
        return 0;
    };
    if manifest_json.is_null() || manifest_length == 0 || piece_directory.trim().is_empty() {
        return 0;
    }
    let json = std::slice::from_raw_parts(manifest_json, manifest_length);
    let Ok((source, manifest)) = crate::p2p::parse_authorized_manifest_json(json) else {
        return 0;
    };
    let piece_directory = PathBuf::from(piece_directory);
    if !piece_directory.is_absolute() {
        return 0;
    }
    let provider: crate::p2p::P2pPieceProvider = Arc::new(move |piece_index| {
        use std::io::Read;

        let path = piece_directory.join(format!("{piece_index}.piece"));
        let mut file = std::fs::File::open(path).map_err(|_| {
            crate::utils::error::ProxyError::Request("P2P Host piece is unavailable".to_string())
        })?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_P2P_CALLBACK_PIECE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| {
                crate::utils::error::ProxyError::Request(
                    "P2P Host piece could not be read".to_string(),
                )
            })?;
        if bytes.is_empty() || bytes.len() > MAX_P2P_CALLBACK_PIECE_BYTES {
            return Err(crate::utils::error::ProxyError::Request(
                "P2P Host piece length is invalid".to_string(),
            ));
        }
        Ok(bytes)
    });
    handle
        .p2p_sources
        .register(source, manifest, provider)
        .unwrap_or(0)
}

#[cfg(feature = "p2p")]
#[no_mangle]
/// Removes an opaque P2P source ID.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by a create function.
pub unsafe extern "C" fn proxy_p2p_source_remove(
    handle: *mut ProxyServerHandle,
    source_id: u64,
) -> u8 {
    handle
        .as_ref()
        .map(|handle| u8::from(handle.p2p_sources.remove(source_id)))
        .unwrap_or(0)
}

/// Verifies every authorized piece and the complete content digest.
///
/// Returns 1 only when the Host callback supplies the complete authorized
/// content; returns 0 for an invalid source ID or any integrity/provider error.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by a create function. The
/// registered callback and context must remain valid until this call returns.
#[cfg(feature = "p2p")]
#[no_mangle]
pub unsafe extern "C" fn proxy_p2p_source_verify_complete(
    handle: *mut ProxyServerHandle,
    source_id: u64,
) -> u8 {
    handle
        .as_ref()
        .map(|handle| u8::from(handle.p2p_sources.verify_complete(source_id).is_ok()))
        .unwrap_or(0)
}

/// Starts the server and waits until its socket is bound. Returns the bound port,
/// or 0 on failure/already-started. A port of 0 requests OS allocation.
///
/// # Safety
///
/// `handle` 必须为空指针，或为 `proxy_server_create` 返回且尚未被
/// `proxy_server_destroy` 释放的指针。
#[no_mangle]
pub unsafe extern "C" fn proxy_server_start(handle: *mut ProxyServerHandle) -> u16 {
    let Some(handle) = handle.as_ref() else {
        return 0;
    };
    let Ok(mut slot) = handle.thread.lock() else {
        return 0;
    };
    if slot.is_some() {
        return 0;
    }
    let Some(config) = handle.config.lock().ok().and_then(|mut value| value.take()) else {
        return 0;
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let published = handle.server.clone();
    #[cfg(feature = "p2p")]
    let p2p_registry = handle.p2p_sources.clone();
    #[cfg(feature = "p2p-librqbit")]
    let rqbit_commands = handle.rqbit_commands.clone();
    let thread = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => {
                let _ = tx.send(0);
                return;
            }
        };
        let result = runtime.block_on(async {
            #[cfg(feature = "p2p-librqbit")]
            let rqbit_cache_directory = config.cache_dir.join("torrent");
            #[cfg(not(feature = "p2p"))]
            let server = Arc::new(ProxyServer::with_config(config));
            #[cfg(feature = "p2p")]
            let server = Arc::new(ProxyServer::with_config_and_p2p_registry(
                config,
                p2p_registry,
            ));
            if let Ok(mut value) = published.lock() {
                *value = Some(server.clone());
            }
            #[cfg(feature = "p2p-librqbit")]
            {
                let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
                if let Ok(mut slot) = rqbit_commands.lock() {
                    *slot = Some(commands);
                }
                tokio::spawn(run_rqbit_commands(
                    receiver,
                    server.clone(),
                    rqbit_cache_directory,
                ));
            }
            let start = tokio::spawn({
                let server = server.clone();
                async move { server.start().await }
            });
            let port = server.wait_until_ready().await.unwrap_or(0);
            let _ = tx.send(port);
            let _ = start.await;
            port
        });
        let _ = result;
    });
    *slot = Some(thread);
    drop(slot);
    rx.recv().unwrap_or(0)
}

#[cfg(feature = "p2p-librqbit")]
async fn run_rqbit_commands(
    mut commands: tokio::sync::mpsc::UnboundedReceiver<RqbitCommand>,
    server: Arc<ProxyServer>,
    cache_directory: PathBuf,
) {
    let mut backend: Option<Arc<crate::rqbit_backend::RqbitBackend>> = None;
    while let Some(command) = commands.recv().await {
        match command {
            RqbitCommand::Add {
                magnet,
                authorized,
                reply,
            } => {
                let valid_request =
                    authorized && crate::p2p_network::parse_magnet_uri(&magnet).is_ok();
                if backend.is_none() && valid_request {
                    if let Ok(created) =
                        crate::rqbit_backend::RqbitBackend::new(cache_directory.clone()).await
                    {
                        let created = Arc::new(created);
                        server.set_rqbit_backend(created.clone()).await;
                        backend = Some(created);
                    }
                }
                let result = match &backend {
                    Some(backend) if valid_request => backend
                        .add_authorized_magnet(&magnet, authorized)
                        .await
                        .ok(),
                    _ => None,
                };
                let _ = reply.send(result);
            }
            RqbitCommand::Remove {
                torrent_id,
                delete_files,
                reply,
            } => {
                let removed = match &backend {
                    Some(backend) => backend.remove(torrent_id, delete_files).await.is_ok(),
                    None => false,
                };
                let _ = reply.send(removed);
            }
            RqbitCommand::Files { torrent_id, reply } => {
                let json = match &backend {
                    Some(backend) => backend
                        .files(torrent_id)
                        .await
                        .ok()
                        .and_then(|files| serde_json::to_string(&files).ok()),
                    None => None,
                };
                let _ = reply.send(json);
            }
            RqbitCommand::Status { torrent_id, reply } => {
                let json = match &backend {
                    Some(backend) => backend
                        .status(torrent_id)
                        .await
                        .ok()
                        .and_then(|status| serde_json::to_string(&status).ok()),
                    None => None,
                };
                let _ = reply.send(json);
            }
            RqbitCommand::SetPaused {
                torrent_id,
                paused,
                reply,
            } => {
                let changed = match &backend {
                    Some(backend) if paused => backend.pause(torrent_id).await.is_ok(),
                    Some(backend) => backend.resume(torrent_id).await.is_ok(),
                    None => false,
                };
                let _ = reply.send(changed);
            }
            RqbitCommand::SetDownloadLimit {
                bytes_per_second,
                reply,
            } => {
                let changed = match &backend {
                    Some(backend) => {
                        backend.set_download_limit(std::num::NonZeroU32::new(bytes_per_second));
                        true
                    }
                    None => false,
                };
                let _ = reply.send(changed);
            }
        }
    }
    if let Some(backend) = backend {
        backend.shutdown();
    }
}

/// Stops the running server. Idempotent, and a no-op on a null handle.
///
/// # Safety
///
/// `handle` 必须为空指针，或为 `proxy_server_create` 返回且尚未被
/// `proxy_server_destroy` 释放的指针。
#[no_mangle]
pub unsafe extern "C" fn proxy_server_stop(handle: *mut ProxyServerHandle) {
    let Some(handle) = handle.as_ref() else {
        return;
    };
    if let Ok(server) = handle.server.lock() {
        if let Some(server) = server.as_ref() {
            server.stop();
        }
    }
}

/// Stops the server, joins its thread, and frees the handle.
///
/// # Safety
///
/// `handle` 必须为空指针，或为 `proxy_server_create` 返回的指针。本函数取得
/// 其所有权，因此每个句柄只能调用一次，调用后该指针不得再被使用。
#[no_mangle]
pub unsafe extern "C" fn proxy_server_destroy(handle: *mut ProxyServerHandle) {
    if handle.is_null() {
        return;
    }
    let handle = Box::from_raw(handle);
    if let Ok(server) = handle.server.lock() {
        if let Some(server) = server.as_ref() {
            server.stop();
        }
    }
    if let Ok(mut slot) = handle.thread.lock() {
        if let Some(thread) = slot.take() {
            let _ = thread.join();
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    #[cfg(feature = "p2p")]
    use std::io::{Read, Write};

    #[cfg(feature = "p2p")]
    unsafe extern "C" fn p2p_piece_callback(
        context: *mut c_void,
        piece_index: usize,
        buffer: *mut u8,
        capacity: usize,
    ) -> usize {
        let pieces = &*(context as *const Vec<Vec<u8>>);
        let Some(piece) = pieces.get(piece_index) else {
            return 0;
        };
        if buffer.is_null() || capacity == 0 {
            return piece.len();
        }
        if capacity < piece.len() {
            return 0;
        }
        ptr::copy_nonoverlapping(piece.as_ptr(), buffer, piece.len());
        piece.len()
    }

    #[cfg(feature = "p2p")]
    fn raw_http(port: u16, request: &str) -> String {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        String::from_utf8(response).unwrap()
    }

    #[test]
    fn ffi_lifecycle_uses_dynamic_port_and_rejects_repeated_start() {
        let cache = tempfile::tempdir().unwrap();
        let path = CString::new(cache.path().to_str().unwrap()).unwrap();
        unsafe {
            let handle = proxy_server_create(0, path.as_ptr());
            assert!(!handle.is_null());
            assert_ne!(proxy_server_start(handle), 0);
            assert_eq!(proxy_server_start(handle), 0);
            proxy_server_stop(handle);
            proxy_server_destroy(handle);
        }
    }

    #[test]
    fn ffi_rejects_null_and_non_utf8_paths() {
        unsafe {
            assert!(proxy_server_create(0, ptr::null()).is_null());
            let invalid = [0xff_u8, 0];
            assert!(proxy_server_create(0, invalid.as_ptr().cast()).is_null());
            assert_eq!(proxy_server_start(ptr::null_mut()), 0);
            #[cfg(feature = "p2p")]
            assert_eq!(proxy_p2p_source_verify_complete(ptr::null_mut(), 1), 0);
            proxy_server_stop(ptr::null_mut());
            proxy_server_destroy(ptr::null_mut());
        }
    }

    #[test]
    fn ffi_accepts_and_normalizes_an_upstream_allowlist() {
        let cache = tempfile::tempdir().unwrap();
        let path = CString::new(cache.path().to_str().unwrap()).unwrap();
        let hosts = CString::new(" media.example.com,cdn.example.com, ").unwrap();
        unsafe {
            let handle = proxy_server_create_with_hosts(0, path.as_ptr(), hosts.as_ptr());
            assert!(!handle.is_null());
            let config = (*handle).config.lock().unwrap();
            assert_eq!(
                config.as_ref().unwrap().allowed_hosts,
                ["media.example.com", "cdn.example.com"]
            );
            drop(config);
            proxy_server_destroy(handle);
        }
    }

    #[cfg(feature = "p2p-librqbit")]
    #[test]
    fn ffi_torrent_commands_require_running_server_and_explicit_authorization() {
        let cache = tempfile::tempdir().unwrap();
        let path = CString::new(cache.path().to_str().unwrap()).unwrap();
        let magnet =
            CString::new("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567").unwrap();
        unsafe {
            assert_eq!(
                proxy_torrent_add_authorized(ptr::null_mut(), magnet.as_ptr(), 1),
                -1
            );
            assert_eq!(proxy_torrent_remove(ptr::null_mut(), 0, 0), 0);

            let handle = proxy_server_create(0, path.as_ptr());
            assert!(!handle.is_null());
            assert_eq!(proxy_torrent_add_authorized(handle, magnet.as_ptr(), 1), -1);
            assert_ne!(proxy_server_start(handle), 0);
            assert_eq!(proxy_torrent_add_authorized(handle, magnet.as_ptr(), 0), -1);
            assert_eq!(proxy_torrent_remove(handle, -1, 0), 0);
            proxy_server_stop(handle);
            proxy_server_destroy(handle);
        }
    }

    #[cfg(feature = "p2p-librqbit")]
    #[test]
    fn ffi_json_writer_reports_capacity_and_never_writes_partial_output() {
        let json = r#"[{"file_id":0}]"#;
        let required = unsafe { write_ffi_json(json, ptr::null_mut(), 0) };
        assert_eq!(required, json.len() + 1);

        let mut short = vec![0xaa; required - 1];
        assert_eq!(
            unsafe { write_ffi_json(json, short.as_mut_ptr(), short.len()) },
            required
        );
        assert!(short.iter().all(|byte| *byte == 0xaa));

        let mut output = vec![0xaa; required];
        assert_eq!(
            unsafe { write_ffi_json(json, output.as_mut_ptr(), output.len()) },
            required
        );
        assert_eq!(&output[..json.len()], json.as_bytes());
        assert_eq!(output[json.len()], 0);
    }

    #[cfg(feature = "p2p")]
    #[test]
    fn ffi_registers_and_reads_verified_p2p_source() {
        let cache = tempfile::tempdir().unwrap();
        let path = CString::new(cache.path().to_str().unwrap()).unwrap();
        let pieces = vec![b"data".to_vec()];
        let digest = crate::utils::digest::sha256_hex(b"data");
        let manifest = serde_json::to_vec(&serde_json::json!({
            "content_id": "asset",
            "content_length": 4,
            "content_sha256": digest,
            "piece_length": 4,
            "piece_sha256": [crate::utils::digest::sha256_hex(b"data")],
            "authorization_reference": "license",
            "explicitly_authorized": true
        }))
        .unwrap();
        unsafe {
            let handle = proxy_server_create(0, path.as_ptr());
            let id = proxy_p2p_source_register(
                handle,
                manifest.as_ptr(),
                manifest.len(),
                Some(p2p_piece_callback),
                (&pieces as *const Vec<Vec<u8>>).cast_mut().cast(),
            );
            assert_ne!(id, 0);
            assert_eq!(proxy_p2p_source_verify_complete(handle, id), 1);
            assert_eq!(proxy_p2p_source_verify_complete(handle, u64::MAX), 0);
            assert_eq!((*handle).p2p_sources.read_range(id, 0, 3).unwrap(), b"data");
            let port = proxy_server_start(handle);
            assert_ne!(port, 0);
            let server = (*handle).server.lock().unwrap().clone().unwrap();
            assert_eq!(server.p2p_registry().read_range(id, 0, 3).unwrap(), b"data");
            let response = raw_http(
                port,
                &format!("GET /p2p/{id} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=1-2\r\nConnection: close\r\n\r\n"),
            );
            assert!(response.starts_with("HTTP/1.1 206"), "{response}");
            assert!(response
                .to_ascii_lowercase()
                .contains("content-range: bytes 1-2/4"));
            assert!(response.ends_with("at"));

            let full = raw_http(
                port,
                &format!("GET /p2p/{id} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
            );
            assert!(full.starts_with("HTTP/1.1 200"));
            assert!(full.ends_with("data"));

            let suffix = raw_http(
                port,
                &format!("GET /p2p/{id} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=-2\r\nConnection: close\r\n\r\n"),
            );
            assert!(suffix.starts_with("HTTP/1.1 206"));
            assert!(suffix.ends_with("ta"));

            let head = raw_http(
                port,
                &format!("HEAD /p2p/{id} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
            );
            assert!(head.starts_with("HTTP/1.1 200"));
            assert!(!head.ends_with("data"));
            let invalid_range = raw_http(
                port,
                &format!("GET /p2p/{id} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=8-9\r\nConnection: close\r\n\r\n"),
            );
            assert!(invalid_range.starts_with("HTTP/1.1 416"));
            assert_eq!(proxy_p2p_source_remove(handle, id), 1);
            assert_eq!(proxy_p2p_source_remove(handle, id), 0);
            assert_eq!(proxy_p2p_source_verify_complete(handle, id), 0);
            let removed = raw_http(
                port,
                &format!("GET /p2p/{id} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
            );
            assert!(removed.starts_with("HTTP/1.1 400"));
            proxy_server_destroy(handle);
        }
    }

    #[cfg(feature = "p2p")]
    #[test]
    fn ffi_complete_verification_rejects_corrupt_host_bytes() {
        let cache = tempfile::tempdir().unwrap();
        let path = CString::new(cache.path().to_str().unwrap()).unwrap();
        let pieces = vec![b"evil".to_vec()];
        let manifest = serde_json::to_vec(&serde_json::json!({
            "content_id": "asset",
            "content_length": 4,
            "content_sha256": crate::utils::digest::sha256_hex(b"data"),
            "piece_length": 4,
            "piece_sha256": [crate::utils::digest::sha256_hex(b"data")],
            "authorization_reference": "license",
            "explicitly_authorized": true
        }))
        .unwrap();
        unsafe {
            let handle = proxy_server_create(0, path.as_ptr());
            let id = proxy_p2p_source_register(
                handle,
                manifest.as_ptr(),
                manifest.len(),
                Some(p2p_piece_callback),
                (&pieces as *const Vec<Vec<u8>>).cast_mut().cast(),
            );
            assert_ne!(id, 0);
            assert_eq!(proxy_p2p_source_verify_complete(handle, id), 0);
            proxy_server_destroy(handle);
        }
    }

    #[cfg(feature = "p2p")]
    #[test]
    fn ffi_directory_provider_registers_verified_piece_files() {
        let cache = tempfile::tempdir().unwrap();
        let pieces = tempfile::tempdir().unwrap();
        std::fs::write(pieces.path().join("0.piece"), b"data").unwrap();
        let cache_path = CString::new(cache.path().to_str().unwrap()).unwrap();
        let piece_path = CString::new(pieces.path().to_str().unwrap()).unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "content_id": "directory-asset",
            "content_length": 4,
            "content_sha256": crate::utils::digest::sha256_hex(b"data"),
            "piece_length": 4,
            "piece_sha256": [crate::utils::digest::sha256_hex(b"data")],
            "authorization_reference": "license",
            "explicitly_authorized": true
        }))
        .unwrap();
        unsafe {
            let handle = proxy_server_create(0, cache_path.as_ptr());
            let id = proxy_p2p_source_register_directory(
                handle,
                manifest.as_ptr(),
                manifest.len(),
                piece_path.as_ptr(),
            );
            assert_ne!(id, 0);
            assert_eq!(proxy_p2p_source_verify_complete(handle, id), 1);
            assert_eq!((*handle).p2p_sources.read_range(id, 1, 2).unwrap(), b"at");
            assert_eq!(proxy_p2p_source_remove(handle, id), 1);
            proxy_server_destroy(handle);
        }
    }
}
