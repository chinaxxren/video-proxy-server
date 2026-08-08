#![allow(dead_code)]

use std::error::Error;
use std::fmt;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;

#[derive(Debug)]
pub struct ClientError(String);

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ClientError {}

#[derive(Clone)]
pub struct Client {
    inner: HyperClient<HttpConnector, Full<Bytes>>,
}

impl Client {
    pub fn new() -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(true);
        Self {
            inner: HyperClient::builder(TokioExecutor::new()).build(connector),
        }
    }

    pub fn get(&self, uri: impl Into<String>) -> RequestBuilder {
        self.request(Method::GET, uri)
    }

    pub fn request(&self, method: Method, uri: impl Into<String>) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            uri: uri.into(),
            headers: HeaderMap::new(),
            error: None,
        }
    }
}

pub struct RequestBuilder {
    client: Client,
    method: Method,
    uri: String,
    headers: HeaderMap,
    error: Option<ClientError>,
}

impl RequestBuilder {
    pub fn header<K, V>(mut self, name: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: fmt::Display,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: fmt::Display,
    {
        let name = HeaderName::try_from(name)
            .map_err(|error| ClientError(format!("invalid header name: {error}")));
        let value = HeaderValue::try_from(value)
            .map_err(|error| ClientError(format!("invalid header value: {error}")));
        match (name, value) {
            (Ok(name), Ok(value)) => {
                self.headers.append(name, value);
            }
            (Err(error), _) | (_, Err(error)) => self.error = Some(error),
        }
        self
    }

    pub async fn send(self) -> Result<Response, ClientError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut request = Request::builder()
            .method(self.method)
            .uri(self.uri)
            .body(Full::new(Bytes::new()))
            .map_err(|error| ClientError(format!("failed to build request: {error}")))?;
        *request.headers_mut() = self.headers;
        let response = self
            .client
            .inner
            .request(request)
            .await
            .map_err(|error| ClientError(format!("HTTP request failed: {error}")))?;
        Ok(Response(response))
    }
}

pub struct Response(hyper::Response<Incoming>);

impl Response {
    pub fn status(&self) -> StatusCode {
        self.0.status()
    }

    pub fn headers(&self) -> &HeaderMap {
        self.0.headers()
    }

    pub fn content_length(&self) -> Option<u64> {
        self.0
            .headers()
            .get(hyper::header::CONTENT_LENGTH)?
            .to_str()
            .ok()?
            .parse()
            .ok()
    }

    pub async fn bytes(self) -> Result<Bytes, ClientError> {
        self.0
            .into_body()
            .collect()
            .await
            .map(|body| body.to_bytes())
            .map_err(|error| ClientError(format!("failed to read response body: {error}")))
    }

    pub async fn text(self) -> Result<String, ClientError> {
        let bytes = self.bytes().await?;
        String::from_utf8(bytes.to_vec())
            .map_err(|error| ClientError(format!("response body is not UTF-8: {error}")))
    }

    pub fn bytes_stream(self) -> impl Stream<Item = Result<Bytes, ClientError>> + Send + 'static {
        self.0.into_body().into_data_stream().map(|chunk| {
            chunk.map_err(|error| ClientError(format!("failed to read response body: {error}")))
        })
    }
}
