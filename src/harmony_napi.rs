//! HarmonyOS N-API ownership bridge for the ArkTS adapter.

use crate::ffi::{
    proxy_server_create_with_hosts, proxy_server_destroy, proxy_server_start, proxy_server_stop,
    ProxyServerHandle,
};
use napi::{Error, Result, Status};
use napi_derive::napi;
use std::ffi::CString;
use std::sync::Mutex;

#[napi]
pub struct MediaProxyCache {
    handle: Mutex<Option<usize>>,
}

#[napi]
impl MediaProxyCache {
    #[napi(factory)]
    pub fn create(port: u32, cache_directory: String, allowed_hosts: Vec<String>) -> Result<Self> {
        if port > u16::MAX as u32
            || cache_directory.trim().is_empty()
            || allowed_hosts.is_empty()
            || allowed_hosts
                .iter()
                .any(|host| host.trim().is_empty() || host.contains(','))
        {
            return Err(Error::new(Status::InvalidArg, "invalid proxy configuration"));
        }
        let cache_directory = CString::new(cache_directory)
            .map_err(|_| Error::new(Status::InvalidArg, "invalid cache directory"))?;
        let allowed_hosts = CString::new(allowed_hosts.join(","))
            .map_err(|_| Error::new(Status::InvalidArg, "invalid allowed host"))?;
        let handle = unsafe {
            proxy_server_create_with_hosts(
                port as u16,
                cache_directory.as_ptr(),
                allowed_hosts.as_ptr(),
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
        let handle = handle
            .ok_or_else(|| Error::new(Status::GenericFailure, "proxy is closed"))?;
        let port = unsafe { proxy_server_start(handle as *mut ProxyServerHandle) };
        if port == 0 {
            Err(Error::new(Status::GenericFailure, "proxy start failed"))
        } else {
            Ok(port as u32)
        }
    }

    #[napi]
    pub fn stop(&self) {
        if let Ok(handle) = self.handle.lock() {
            if let Some(handle) = *handle {
                unsafe { proxy_server_stop(handle as *mut ProxyServerHandle) }
            }
        }
    }

    #[napi]
    pub fn close(&self) {
        let handle = self
            .handle
            .lock()
            .ok()
            .and_then(|mut handle| handle.take());
        if let Some(handle) = handle {
            unsafe { proxy_server_destroy(handle as *mut ProxyServerHandle) }
        }
    }
}

impl Drop for MediaProxyCache {
    fn drop(&mut self) {
        self.close();
    }
}
