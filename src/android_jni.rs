//! Android JNI ownership bridge for the Kotlin adapter.

use crate::ffi::{
    proxy_server_create_with_hosts, proxy_server_destroy, proxy_server_start, proxy_server_stop,
    ProxyServerHandle,
};
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jlong};
use jni::{errors::ThrowRuntimeExAndDefault, EnvUnowned};
use std::ffi::CString;

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
        Ok(handle as jlong)
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
        Ok(unsafe { proxy_server_start(handle as *mut ProxyServerHandle) as jint })
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
        unsafe { proxy_server_stop(handle as *mut ProxyServerHandle) }
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
        unsafe { proxy_server_destroy(handle as *mut ProxyServerHandle) }
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}
