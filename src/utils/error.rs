use std::error::Error;
use std::fmt;
use std::io;
use std::str::Utf8Error;
use tokio::sync::AcquireError;

/// 全局结果集类型
pub type Result<T> = std::result::Result<T, ProxyError>;

#[derive(Debug, Clone)]
pub enum ProxyError {
    Cache(String),
    Network(String),
    InvalidRange(String),
    Range(String),
    Request(String),
    Storage(String),
    Parse(String),
    IO(String),
    MethodNotAllowed,
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProxyError::Cache(msg) => write!(f, "Cache error: {}", msg),
            ProxyError::Network(msg) => write!(f, "Network error: {}", msg),
            ProxyError::InvalidRange(msg) => write!(f, "Invalid range error: {}", msg),
            ProxyError::Range(msg) => write!(f, "Range error: {}", msg),
            ProxyError::Request(msg) => write!(f, "Request error: {}", msg),
            ProxyError::Storage(msg) => write!(f, "Storage error: {}", msg),
            ProxyError::Parse(msg) => write!(f, "Parse error: {}", msg),
            ProxyError::IO(msg) => write!(f, "IO error: {}", msg),
            ProxyError::MethodNotAllowed => write!(f, "Method not allowed"),
        }
    }
}

impl Error for ProxyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

impl ProxyError {
    /// 映射为对外的 HTTP 状态码。
    ///
    /// 此前所有错误一律返回 500，客户端无法区分「自己的 Range 不合法」和
    /// 「服务端故障」，播放器也就无法据此重试或调整请求。
    pub fn status_code(&self) -> hyper::StatusCode {
        match self {
            ProxyError::InvalidRange(_) | ProxyError::Range(_) => {
                hyper::StatusCode::RANGE_NOT_SATISFIABLE
            }
            ProxyError::Request(_) => hyper::StatusCode::BAD_REQUEST,
            ProxyError::Network(_) => hyper::StatusCode::BAD_GATEWAY,
            ProxyError::Cache(_) | ProxyError::Storage(_) | ProxyError::IO(_) => {
                hyper::StatusCode::INTERNAL_SERVER_ERROR
            }
            ProxyError::Parse(_) => hyper::StatusCode::BAD_GATEWAY,
            ProxyError::MethodNotAllowed => hyper::StatusCode::METHOD_NOT_ALLOWED,
        }
    }

    /// 对外可见的错误描述。
    ///
    /// 只暴露错误类别，不回显内部消息（内部消息含缓存路径、上游 URL 等）。
    pub fn public_message(&self) -> &'static str {
        match self {
            ProxyError::InvalidRange(_) | ProxyError::Range(_) => "Requested range not satisfiable",
            ProxyError::Request(_) => "Bad request",
            ProxyError::Network(_) | ProxyError::Parse(_) => "Upstream error",
            ProxyError::Cache(_) | ProxyError::Storage(_) | ProxyError::IO(_) => "Internal error",
            ProxyError::MethodNotAllowed => "Method not allowed",
        }
    }
}

impl From<hyper::Error> for ProxyError {
    fn from(err: hyper::Error) -> Self {
        ProxyError::Network(err.to_string())
    }
}

impl From<io::Error> for ProxyError {
    fn from(err: io::Error) -> Self {
        ProxyError::IO(err.to_string())
    }
}

impl From<std::num::ParseIntError> for ProxyError {
    fn from(err: std::num::ParseIntError) -> Self {
        ProxyError::Parse(err.to_string())
    }
}

impl From<serde_json::Error> for ProxyError {
    fn from(err: serde_json::Error) -> Self {
        ProxyError::Parse(err.to_string())
    }
}

impl From<hyper::http::Error> for ProxyError {
    fn from(err: hyper::http::Error) -> Self {
        ProxyError::Request(err.to_string())
    }
}

impl From<hyper::header::ToStrError> for ProxyError {
    fn from(err: hyper::header::ToStrError) -> Self {
        ProxyError::Request(err.to_string())
    }
}

impl From<Utf8Error> for ProxyError {
    fn from(err: Utf8Error) -> Self {
        ProxyError::Parse(err.to_string())
    }
}

impl From<AcquireError> for ProxyError {
    fn from(_: AcquireError) -> Self {
        ProxyError::Storage("无法获取信号量".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_stable_http_status_and_sanitized_messages() {
        let cases = [
            (
                ProxyError::InvalidRange("secret".into()),
                416,
                "Requested range not satisfiable",
            ),
            (
                ProxyError::Range("secret".into()),
                416,
                "Requested range not satisfiable",
            ),
            (ProxyError::Request("signed-url".into()), 400, "Bad request"),
            (
                ProxyError::Network("signed-url".into()),
                502,
                "Upstream error",
            ),
            (
                ProxyError::Parse("signed-url".into()),
                502,
                "Upstream error",
            ),
            (
                ProxyError::Cache("/private/path".into()),
                500,
                "Internal error",
            ),
            (
                ProxyError::Storage("/private/path".into()),
                500,
                "Internal error",
            ),
            (
                ProxyError::IO("/private/path".into()),
                500,
                "Internal error",
            ),
            (ProxyError::MethodNotAllowed, 405, "Method not allowed"),
        ];

        for (error, status, message) in cases {
            assert_eq!(error.status_code().as_u16(), status);
            assert_eq!(error.public_message(), message);
            assert!(!error.public_message().contains("secret"));
            assert!(!error.public_message().contains("private"));
        }
    }
}
