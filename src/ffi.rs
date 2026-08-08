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

pub struct ProxyServerHandle {
    config: Mutex<Option<ProxyConfig>>,
    server: Arc<Mutex<Option<Arc<ProxyServer>>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    #[cfg(feature = "p2p")]
    p2p_sources: crate::p2p::P2pSourceRegistry,
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
    let p2p_sources = crate::p2p::P2pSourceRegistry::with_cache_dir(config.cache_dir.join("p2p"));
    Box::into_raw(Box::new(ProxyServerHandle {
        config: Mutex::new(Some(config)),
        server: Arc::new(Mutex::new(None)),
        thread: Mutex::new(None),
        #[cfg(feature = "p2p")]
        p2p_sources,
    }))
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
            let removed = raw_http(
                port,
                &format!("GET /p2p/{id} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
            );
            assert!(removed.starts_with("HTTP/1.1 400"));
            proxy_server_destroy(handle);
        }
    }
}
