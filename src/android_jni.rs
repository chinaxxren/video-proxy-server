//! Android JNI ownership bridge for the Kotlin adapter.

#[cfg(feature = "p2p")]
use crate::ffi::{
    proxy_p2p_source_register_directory, proxy_p2p_source_remove, proxy_p2p_source_verify_complete,
};
use crate::ffi::{
    proxy_server_create_with_hosts, proxy_server_destroy, proxy_server_start, proxy_server_stop,
    ProxyServerHandle,
};
#[cfg(feature = "p2p-librqbit")]
use crate::ffi::{
    proxy_torrent_add_authorized, proxy_torrent_add_file_authorized, proxy_torrent_files_json,
    proxy_torrent_remove, proxy_torrent_select_files, proxy_torrent_set_download_limit,
    proxy_torrent_set_paused, proxy_torrent_status_json,
};
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::{errors::ThrowRuntimeExAndDefault, EnvUnowned};
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex};

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static HANDLES: LazyLock<Mutex<HashMap<jlong, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn register_handle(handle: *mut ProxyServerHandle) -> Option<jlong> {
    let token = NEXT_HANDLE
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1).filter(|next| *next > 0)
        })
        .ok()?;
    HANDLES.lock().ok()?.insert(token, handle as usize);
    Some(token)
}

fn with_handle<T>(token: jlong, operation: impl FnOnce(*mut ProxyServerHandle) -> T) -> Option<T> {
    let handles = HANDLES.lock().ok()?;
    let handle = *handles.get(&token)? as *mut ProxyServerHandle;
    Some(operation(handle))
}

fn remove_handle(token: jlong) -> Option<*mut ProxyServerHandle> {
    HANDLES
        .lock()
        .ok()?
        .remove(&token)
        .map(|handle| handle as *mut ProxyServerHandle)
}

#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeCreate<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    port: jint,
    cache_directory: JString<'local>,
    allowed_hosts: JString<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let cache_directory = cache_directory.try_to_string(env)?;
        let allowed_hosts = allowed_hosts.try_to_string(env)?;
        let (Ok(cache_directory), Ok(allowed_hosts), Ok(port)) = (
            CString::new(cache_directory),
            CString::new(allowed_hosts),
            u16::try_from(port),
        ) else {
            return Ok(0);
        };
        let handle = unsafe {
            proxy_server_create_with_hosts(port, cache_directory.as_ptr(), allowed_hosts.as_ptr())
        };
        if handle.is_null() {
            return Ok(0);
        }
        match register_handle(handle) {
            Some(token) => Ok(token),
            None => {
                unsafe { proxy_server_destroy(handle) }
                Ok(0)
            }
        }
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeStart<'local>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
) -> jint {
    env.with_env(|_| -> jni::errors::Result<jint> {
        Ok(with_handle(handle, |handle| unsafe {
            proxy_server_start(handle) as jint
        })
        .unwrap_or(0))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeStop<'local>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
) {
    env.with_env(|_| -> jni::errors::Result<()> {
        with_handle(handle, |handle| unsafe { proxy_server_stop(handle) });
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeDestroy<'local>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
) {
    env.with_env(|_| -> jni::errors::Result<()> {
        if let Some(handle) = remove_handle(handle) {
            unsafe { proxy_server_destroy(handle) }
        }
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
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

#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeRegisterP2PDirectory<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    manifest_json: JString<'local>,
    piece_directory: JString<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let manifest_json = manifest_json.try_to_string(env)?;
        let piece_directory = piece_directory.try_to_string(env)?;
        let Ok(piece_directory) = CString::new(piece_directory) else {
            return Ok(0);
        };
        Ok(with_handle(handle, |handle| {
            register_p2p_directory(handle, &manifest_json, &piece_directory)
        })
        .and_then(|source_id| jlong::try_from(source_id).ok())
        .unwrap_or(0))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeVerifyP2PSource<'local>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    source_id: jlong,
) -> jboolean {
    env.with_env(|_| -> jni::errors::Result<jboolean> {
        let verified = u64::try_from(source_id)
            .ok()
            .and_then(|source_id| {
                with_handle(handle, |handle| verify_p2p_source(handle, source_id))
            })
            .unwrap_or(false);
        Ok(verified)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeRemoveP2PSource<'local>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    source_id: jlong,
) -> jboolean {
    env.with_env(|_| -> jni::errors::Result<jboolean> {
        let removed = u64::try_from(source_id)
            .ok()
            .and_then(|source_id| {
                with_handle(handle, |handle| remove_p2p_source(handle, source_id))
            })
            .unwrap_or(false);
        Ok(removed)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeAddAuthorizedTorrent<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    magnet_uri: JString<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let magnet_uri = magnet_uri.try_to_string(env)?;
        let Ok(magnet_uri) = CString::new(magnet_uri) else {
            return Ok(-1);
        };
        Ok(with_handle(handle, |handle| unsafe {
            proxy_torrent_add_authorized(handle, magnet_uri.as_ptr(), 1)
        })
        .unwrap_or(-1))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeAddAuthorizedTorrentFile<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    torrent_bytes: JByteArray<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let bytes = env.convert_byte_array(&torrent_bytes)?;
        if bytes.is_empty() || bytes.len() > 4 * 1024 * 1024 {
            return Ok(-1);
        }
        Ok(with_handle(handle, |handle| unsafe {
            proxy_torrent_add_file_authorized(handle, bytes.as_ptr(), bytes.len(), 1)
        })
        .unwrap_or(-1))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeRemoveTorrent<'local>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    torrent_id: jlong,
    delete_files: jboolean,
) -> jboolean {
    env.with_env(|_| -> jni::errors::Result<jboolean> {
        Ok(with_handle(handle, |handle| unsafe {
            proxy_torrent_remove(handle, torrent_id, u8::from(delete_files)) != 0
        })
        .unwrap_or(false))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(feature = "p2p-librqbit")]
unsafe fn read_torrent_json(
    handle: *mut ProxyServerHandle,
    torrent_id: jlong,
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

#[cfg(feature = "p2p-librqbit")]
fn torrent_json_to_jstring<'local>(
    env: &mut jni::Env<'local>,
    handle: jlong,
    torrent_id: jlong,
    query: unsafe extern "C" fn(*mut ProxyServerHandle, i64, *mut u8, usize) -> usize,
) -> jni::errors::Result<jstring> {
    let json = with_handle(handle, |handle| unsafe {
        read_torrent_json(handle, torrent_id, query)
    })
    .flatten();
    match json {
        Some(json) => Ok(env.new_string(json)?.into_raw().cast()),
        None => Ok(std::ptr::null_mut()),
    }
}

#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeTorrentFilesJson<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    torrent_id: jlong,
) -> jstring {
    env.with_env(|env| torrent_json_to_jstring(env, handle, torrent_id, proxy_torrent_files_json))
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeTorrentStatusJson<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    torrent_id: jlong,
) -> jstring {
    env.with_env(|env| torrent_json_to_jstring(env, handle, torrent_id, proxy_torrent_status_json))
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeSetTorrentPaused<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    torrent_id: jlong,
    paused: jboolean,
) -> jboolean {
    env.with_env(|_| -> jni::errors::Result<jboolean> {
        Ok(with_handle(handle, |handle| unsafe {
            proxy_torrent_set_paused(handle, torrent_id, u8::from(paused)) != 0
        })
        .unwrap_or(false))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
#[allow(deprecated)]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeSelectTorrentFiles(
    mut env: EnvUnowned<'_>,
    _object: JObject<'_>,
    handle: jlong,
    torrent_id: jlong,
    file_ids: jni::objects::JIntArray<'_>,
) -> jboolean {
    env.with_env(|env| -> jni::errors::Result<jboolean> {
        let mut ids = vec![0i32; env.get_array_length(&file_ids)? as usize];
        env.get_int_array_region(&file_ids, 0, &mut ids)?;
        let ids: Vec<u32> = ids
            .into_iter()
            .filter_map(|id| u32::try_from(id).ok())
            .collect();
        Ok(with_handle(handle, |handle| unsafe {
            proxy_torrent_select_files(handle, torrent_id, ids.as_ptr(), ids.len()) != 0
        })
        .unwrap_or(false))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(feature = "p2p-librqbit")]
#[no_mangle]
pub extern "system" fn Java_com_example_mediaproxy_MediaProxyCache_nativeSetTorrentDownloadLimit<
    'local,
>(
    mut env: EnvUnowned<'local>,
    _object: JObject<'local>,
    handle: jlong,
    bytes_per_second: jlong,
) -> jboolean {
    env.with_env(|_| -> jni::errors::Result<jboolean> {
        let changed = u32::try_from(bytes_per_second)
            .ok()
            .and_then(|limit| {
                with_handle(handle, |handle| unsafe {
                    proxy_torrent_set_download_limit(handle, limit) != 0
                })
            })
            .unwrap_or(false);
        Ok(changed)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_handles_reject_unknown_and_removed_tokens() {
        let pointer = std::ptr::dangling_mut::<ProxyServerHandle>();
        let token = register_handle(pointer).unwrap();
        assert_ne!(token, pointer as jlong);
        assert_eq!(
            with_handle(token, |handle| handle as usize),
            Some(pointer as usize)
        );
        assert!(with_handle(jlong::MAX, |_| ()).is_none());
        assert_eq!(remove_handle(token), Some(pointer));
        assert!(with_handle(token, |_| ()).is_none());
        assert!(remove_handle(token).is_none());
    }
}
