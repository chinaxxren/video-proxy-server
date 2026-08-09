//! HarmonyOS N-API ownership bridge for the ArkTS adapter.

use crate::ffi::{
    proxy_server_create_with_hosts, proxy_server_destroy, proxy_server_start, proxy_server_stop,
    ProxyServerHandle,
};
use crate::harmony_config::HarmonyConfiguration;
use napi::{Error, Result, Status};
use napi_derive::napi;
use std::sync::Mutex;

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

impl Drop for MediaProxyCache {
    fn drop(&mut self) {
        self.close_for_drop();
    }
}
