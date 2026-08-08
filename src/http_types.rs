use crate::utils::error::{ProxyError, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};

pub type AppBody = UnsyncBoxBody<Bytes, ProxyError>;

pub fn stream_body<S>(stream: S) -> AppBody
where
    S: Stream<Item = Result<Bytes>> + Send + 'static,
{
    StreamBody::new(stream.map(|chunk| chunk.map(hyper::body::Frame::data))).boxed_unsync()
}

pub fn full_body(value: impl Into<Bytes>) -> AppBody {
    Full::new(value.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

pub fn empty_body() -> AppBody {
    full_body(Bytes::new())
}

pub async fn collect_body(body: AppBody) -> Result<Bytes> {
    body.collect().await.map(|collected| collected.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn app_body_supports_stream_full_and_empty_content() {
        let stream = futures_util::stream::iter([
            Ok(Bytes::from_static(b"ab")),
            Ok(Bytes::from_static(b"cd")),
        ]);
        assert_eq!(collect_body(stream_body(stream)).await.unwrap(), "abcd");
        assert_eq!(collect_body(full_body("data")).await.unwrap(), "data");
        assert!(collect_body(empty_body()).await.unwrap().is_empty());
    }
}
