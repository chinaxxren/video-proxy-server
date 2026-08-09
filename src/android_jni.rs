//! Android JNI ownership bridge for the Kotlin adapter.

use crate::ffi::{
    proxy_server_create_with_hosts, proxy_server_destroy, proxy_server_start, proxy_server_stop,
    ProxyServerHandle,
};
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jlong};
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
