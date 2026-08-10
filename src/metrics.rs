use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MetricsSnapshot {
    pub requests: u64,
    pub active_requests: u64,
    pub request_errors: u64,
    pub response_bytes: u64,
    pub authorization_refreshes: u64,
}

#[derive(Debug, Default)]
pub(crate) struct RuntimeMetrics {
    requests: AtomicU64,
    active_requests: AtomicU64,
    request_errors: AtomicU64,
    response_bytes: AtomicU64,
    authorization_refreshes: AtomicU64,
}

impl RuntimeMetrics {
    pub(crate) fn begin_request(self: &Arc<Self>) -> ActiveRequestGuard {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.active_requests.fetch_add(1, Ordering::Relaxed);
        ActiveRequestGuard {
            metrics: Arc::clone(self),
        }
    }

    pub(crate) fn record_request_error(&self) {
        self.request_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_response_bytes(&self, bytes: usize) {
        self.response_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_authorization_refresh(&self) {
        self.authorization_refreshes.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            active_requests: self.active_requests.load(Ordering::Relaxed),
            request_errors: self.request_errors.load(Ordering::Relaxed),
            response_bytes: self.response_bytes.load(Ordering::Relaxed),
            authorization_refreshes: self.authorization_refreshes.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct ActiveRequestGuard {
    metrics: Arc<RuntimeMetrics>,
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.metrics.active_requests.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_request_guard_balances_the_gauge() {
        let metrics = Arc::new(RuntimeMetrics::default());
        let first = metrics.begin_request();
        let second = metrics.begin_request();
        assert_eq!(metrics.snapshot().active_requests, 2);
        drop(first);
        assert_eq!(metrics.snapshot().active_requests, 1);
        drop(second);
        assert_eq!(metrics.snapshot().active_requests, 0);
        assert_eq!(metrics.snapshot().requests, 2);
    }
}
