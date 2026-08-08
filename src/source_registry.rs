//! Opaque media-source registration for native adapters.
//!
//! Signed URLs are kept only in this process-local table. Callers receive an
//! opaque decimal ID and must never put the source URL in a localhost URL.

use crate::utils::error::{ProxyError, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use url::Url;

#[derive(Clone, Debug)]
pub struct RegisteredSource {
    pub identity: String,
    pub url: String,
}

#[derive(Clone)]
pub struct SourceRegistry {
    next_id: Arc<AtomicU64>,
    entries: Arc<RwLock<HashMap<u64, RegisteredSource>>>,
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl SourceRegistry {
    pub fn register(&self, identity: &str, url: &str) -> Result<u64> {
        let identity = validate_component(identity, "媒体身份")?;
        let url = validate_source_url(url)?;
        let id = self
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                (value != 0).then_some(value.wrapping_add(1).max(1))
            })
            .map_err(|_| ProxyError::Request("来源 ID 已耗尽".to_string()))?;
        self.entries
            .write()
            .map_err(|_| ProxyError::Request("来源注册表不可用".to_string()))?
            .insert(id, RegisteredSource { identity, url });
        Ok(id)
    }

    pub fn resolve(&self, id: u64) -> Option<RegisteredSource> {
        self.entries.read().ok()?.get(&id).cloned()
    }

    /// Reuse an existing registration for the same logical source URL.
    pub fn register_or_reuse(&self, identity: &str, url: &str) -> Result<u64> {
        let validated_identity = validate_component(identity, "媒体身份")?;
        let validated_url = validate_source_url(url)?;
        if let Ok(entries) = self.entries.read() {
            if let Some((id, _)) = entries.iter().find(|(_, source)| {
                source.identity == validated_identity && source.url == validated_url
            }) {
                return Ok(*id);
            }
        }
        self.register(&validated_identity, &validated_url)
    }

    pub fn refresh(&self, id: u64, url: &str) -> Result<()> {
        let url = validate_source_url(url)?;
        let mut entries = self
            .entries
            .write()
            .map_err(|_| ProxyError::Request("来源注册表不可用".to_string()))?;
        let source = entries
            .get_mut(&id)
            .ok_or_else(|| ProxyError::Request("来源 ID 不存在".to_string()))?;
        source.url = url;
        Ok(())
    }

    pub fn remove(&self, id: u64) -> bool {
        self.entries
            .write()
            .ok()
            .and_then(|mut entries| entries.remove(&id))
            .is_some()
    }

    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }
}

fn validate_component(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 512 || value.bytes().any(|byte| byte == 0) {
        return Err(ProxyError::Request(format!("{}无效", label)));
    }
    Ok(value.to_string())
}

fn validate_source_url(value: &str) -> Result<String> {
    let parsed = Url::parse(value).map_err(|_| ProxyError::Request("来源 URL 无效".to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.host_str().is_none()
    {
        return Err(ProxyError::Request(
            "来源 URL 必须是无凭据的 HTTP(S) 地址".to_string(),
        ));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_ids_resolve_and_refresh_without_exposing_url_in_id() {
        let registry = SourceRegistry::default();
        let id = registry
            .register("asset-1", "https://media.example/a.mp4?token=secret")
            .unwrap();
        assert!(!id.to_string().contains("secret"));
        assert_eq!(registry.resolve(id).unwrap().identity, "asset-1");
        registry
            .refresh(id, "https://media.example/a.mp4?token=rotated")
            .unwrap();
        assert!(registry.resolve(id).unwrap().url.contains("rotated"));
    }

    #[test]
    fn rejects_credentials_invalid_scheme_and_unknown_refresh() {
        let registry = SourceRegistry::default();
        assert!(registry.register("a", "file:///tmp/a").is_err());
        assert!(registry
            .register("a", "https://user:pass@media.example/a")
            .is_err());
        assert!(registry.refresh(99, "https://media.example/a").is_err());
    }

    #[test]
    fn removal_invalidates_future_resolution() {
        let registry = SourceRegistry::default();
        let id = registry.register("a", "https://media.example/a").unwrap();
        assert!(registry.remove(id));
        assert!(registry.resolve(id).is_none());
        assert!(!registry.remove(id));
    }

    #[test]
    fn identical_source_registration_reuses_id() {
        let registry = SourceRegistry::default();
        let first = registry
            .register_or_reuse("hls", "https://media.example/seg.ts?token=x")
            .unwrap();
        let second = registry
            .register_or_reuse("hls", "https://media.example/seg.ts?token=x")
            .unwrap();
        assert_eq!(first, second);
    }
}
