//! Minimal C ABI for mobile adapters.
//!
//! The ABI owns no caller memory: strings are copied during construction and
//! the returned handle remains valid until `proxy_server_destroy` is called.

use crate::server::{ProxyConfig, ProxyServer};
use crate::source_registry::SourceRegistry;
use std::ffi::{c_char, c_void, CStr};
use std::path::PathBuf;
use std::ptr;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

pub struct ProxyServerHandle {
    config: Mutex<Option<ProxyConfig>>,
    server: Arc<Mutex<Option<Arc<ProxyServer>>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    pub(crate) sources: SourceRegistry,
}

pub type ProxySourceRefreshCallback =
    unsafe extern "C" fn(context: *mut c_void, source_id: u64) -> *const c_char;

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
    Box::into_raw(Box::new(ProxyServerHandle {
        config: Mutex::new(Some(config)),
        server: Arc::new(Mutex::new(None)),
        thread: Mutex::new(None),
        sources: SourceRegistry::default(),
    }))
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
    let sources = handle.sources.clone();
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
            let server = Arc::new(ProxyServer::with_config_and_registry(config, sources));
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

/// Registers a source and returns an opaque process-local ID. The URL is copied
/// into Core memory and is never returned to the caller.
///
/// # Safety
///
/// `handle` must be a live handle returned by a create function. `identity` and
/// `url` must point to immutable NUL-terminated strings for this call.
#[no_mangle]
pub unsafe extern "C" fn proxy_source_register(
    handle: *mut ProxyServerHandle,
    identity: *const c_char,
    url: *const c_char,
) -> u64 {
    let Some(handle) = handle.as_ref() else {
        return 0;
    };
    let (Some(identity), Some(url)) = (read_string(identity), read_string(url)) else {
        return 0;
    };
    handle
        .sources
        .register_or_reuse(&identity, &url)
        .unwrap_or(0)
}

#[no_mangle]
/// Replaces the current URL for an existing opaque source ID.
///
/// # Safety
///
/// `handle` must be live and `url` must point to an immutable NUL-terminated
/// string for this call.
pub unsafe extern "C" fn proxy_source_refresh(
    handle: *mut ProxyServerHandle,
    source_id: u64,
    url: *const c_char,
) -> u8 {
    let Some(handle) = handle.as_ref() else {
        return 0;
    };
    let Some(url) = read_string(url) else {
        return 0;
    };
    handle
        .sources
        .refresh(source_id, &url)
        .map(|_| 1)
        .unwrap_or(0)
}

#[no_mangle]
/// Removes a source ID. Repeated removal is safe and returns zero.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by a create function.
pub unsafe extern "C" fn proxy_source_remove(handle: *mut ProxyServerHandle, source_id: u64) -> u8 {
    handle
        .as_ref()
        .map(|handle| u8::from(handle.sources.remove(source_id)))
        .unwrap_or(0)
}

/// Registers or clears the Host callback used after upstream 401/403 responses.
/// The returned string is copied during the callback and remains Host-owned.
///
/// # Safety
///
/// `handle` must be live. The callback and context must remain valid until the
/// callback is cleared or the handle is destroyed. Returned strings must be
/// immutable, NUL-terminated UTF-8 for the duration of the callback.
#[no_mangle]
pub unsafe extern "C" fn proxy_source_set_refresh_callback(
    handle: *mut ProxyServerHandle,
    callback: Option<ProxySourceRefreshCallback>,
    context: *mut c_void,
) -> u8 {
    let Some(handle) = handle.as_ref() else {
        return 0;
    };
    let provider = callback.map(|callback| {
        let context = context as usize;
        Arc::new(move |source_id| {
            let value = unsafe { callback(context as *mut c_void, source_id) };
            unsafe { read_string(value) }
                .ok_or_else(|| crate::utils::error::ProxyError::Request("来源刷新失败".to_string()))
        }) as crate::source_registry::RefreshProvider
    });
    u8::from(handle.sources.set_refresh_provider(provider).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    unsafe extern "C" fn test_refresh_callback(
        _context: *mut c_void,
        _source_id: u64,
    ) -> *const c_char {
        static URL: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
        URL.get_or_init(|| CString::new("https://media.example/refreshed.mp4?token=new").unwrap())
            .as_ptr()
    }
    use std::io::{Read, Write};

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

    #[test]
    fn ffi_source_registration_refresh_and_remove_are_opaque_and_validated() {
        let cache = tempfile::tempdir().unwrap();
        let path = CString::new(cache.path().to_str().unwrap()).unwrap();
        let identity = CString::new("asset-1").unwrap();
        let source = CString::new("https://media.example/a.mp4?token=secret").unwrap();
        let rotated = CString::new("https://media.example/a.mp4?token=rotated").unwrap();
        let invalid = CString::new("file:///tmp/a").unwrap();
        unsafe {
            let handle = proxy_server_create_with_hosts(
                0,
                path.as_ptr(),
                CString::new("media.example").unwrap().as_ptr(),
            );
            assert!(!handle.is_null());
            let id = proxy_source_register(handle, identity.as_ptr(), source.as_ptr());
            assert_ne!(id, 0);
            assert_eq!(
                proxy_source_register(handle, identity.as_ptr(), source.as_ptr()),
                id
            );
            assert!(!id.to_string().contains("secret"));
            assert_eq!(proxy_source_refresh(handle, id, rotated.as_ptr()), 1);
            assert_eq!(proxy_source_refresh(handle, 999, rotated.as_ptr()), 0);
            assert_eq!(proxy_source_refresh(handle, id, invalid.as_ptr()), 0);
            assert_eq!(proxy_source_remove(handle, id), 1);
            assert_eq!(proxy_source_remove(handle, id), 0);
            assert_eq!(
                proxy_source_register(handle, ptr::null(), source.as_ptr()),
                0
            );
            proxy_server_destroy(handle);
        }
    }

    #[test]
    fn ffi_refresh_callback_is_copied_and_validated() {
        let cache = tempfile::tempdir().unwrap();
        let path = CString::new(cache.path().to_str().unwrap()).unwrap();
        let identity = CString::new("asset").unwrap();
        let source = CString::new("https://media.example/original.mp4").unwrap();
        unsafe {
            let handle = proxy_server_create(0, path.as_ptr());
            let id = proxy_source_register(handle, identity.as_ptr(), source.as_ptr());
            assert_eq!(
                proxy_source_set_refresh_callback(
                    handle,
                    Some(test_refresh_callback),
                    ptr::null_mut(),
                ),
                1
            );
            (*handle).sources.refresh_from_provider(id).unwrap();
            assert!((*handle)
                .sources
                .resolve(id)
                .unwrap()
                .url
                .contains("token=new"));
            assert_eq!(
                proxy_source_set_refresh_callback(handle, None, ptr::null_mut()),
                1
            );
            assert!((*handle).sources.refresh_from_provider(id).is_err());
            proxy_server_destroy(handle);
        }
    }

    #[test]
    fn ffi_registered_id_is_visible_to_running_http_server() {
        let cache = tempfile::tempdir().unwrap();
        let path = CString::new(cache.path().to_str().unwrap()).unwrap();
        let identity = CString::new("asset-ffi-route").unwrap();
        let source = CString::new("https://media.example/video.mp4?token=secret").unwrap();
        unsafe {
            let handle = proxy_server_create(0, path.as_ptr());
            assert!(!handle.is_null());
            let id = proxy_source_register(handle, identity.as_ptr(), source.as_ptr());
            assert_ne!(id, 0);
            let port = proxy_server_start(handle);
            assert_ne!(port, 0);

            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                stream,
                "GET /media/{id} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
            proxy_server_destroy(handle);
        }
    }
}
