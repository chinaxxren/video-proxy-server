# Media Proxy Cache

English | [简体中文](README.zh-CN.md)

A Rust HTTP media proxy with byte-range caching and HLS support. The server listens on `127.0.0.1`, streams data from approved upstream hosts, and persists completed byte ranges on disk.

> Status: prototype. The core safety and cache-correctness issues have initial fixes and regression tests, but the project is not yet recommended as a production dependency. Mobile FFI, request coalescing, and broader end-to-end coverage are still pending.

## Features

- HTTP and HTTPS upstream sources
- HTTP `Range` requests and partial-content responses
- Disk-backed chunk caching
- Persistent completed-range metadata, independent of sparse file length
- Mixed cache/network responses for a verified contiguous cache prefix
- HLS playlist rewriting and segment proxying
- Size/count-based cache cleanup with physical file deletion
- Stable cache identity independent of signed URLs
- Upstream host allowlist and private-address rejection
- Localhost-only listener

## Requirements

- Rust 1.70 or later
- Cargo
- Linux, macOS, or Windows

## Build And Test

```bash
cargo build --locked
cargo test --locked
```

The current suite covers stable cache keys, sparse-range correctness, range metadata persistence, physical deletion, cleanup behavior, block-lock regression, and core network-policy rejection cases.

## Run

The executable accepts:

```text
proxy-server [port] [cache-directory] [comma-separated-allowed-hosts]
```

Example:

```bash
cargo run -- 8080 ./cache media.example.com,cdn.example.com
```

If the allowlist is omitted, the server starts but rejects every upstream request. It always binds to `127.0.0.1`.

You can also run the maintained client example:

```bash
cargo run --example proxy_client -- \
  https://media.w3.org/2010/05/sintel/trailer.mp4
```

## Embed The Server

Configure approved hosts explicitly:

```rust
use proxy_server::server::ProxyServer;

#[tokio::main]
async fn main() {
    let server = ProxyServer::with_allowed_hosts(
        8080,
        "./cache",
        ["media.example.com", "cdn.example.com"],
    );

    server.start().await.unwrap();
}
```

`ProxyServer::new` uses a deny-all network policy. Prefer `with_allowed_hosts` for any server that needs upstream access.

## Proxy Request Contract

Send the current upstream URL in `X-Original-Url` and provide all three stable cache identity headers:

```bash
curl 'http://127.0.0.1:8080/proxy/media' \
  -H 'X-Original-Url: https://media.example.com/audio/song.m4a?token=short-lived' \
  -H 'Range: bytes=0-65535' \
  -H 'X-Cache-User-Id: user-123' \
  -H 'X-Cache-Asset-Id: song-456' \
  -H 'X-Cache-Asset-Revision: 7'
```

The cache identity is derived only from:

```text
userId + assetId + assetRevision
```

The signed URL is only the current network source. Changing its token does not create a different cache entry. Requests missing any stable identity header are rejected when they enter the cache path.

## Network Security

Before an upstream request is sent, the proxy:

- accepts only `http` and `https` URLs;
- rejects URLs containing credentials;
- requires an exact host allowlist match;
- resolves the host and rejects loopback, private, link-local, documentation, multicast, and other reserved addresses;
- rejects redirects because the current Hyper client does not follow them automatically.

Do not log or persist `X-Original-Url` outside this core. It may contain short-lived credentials.

## Cache Layout

Cache keys are hashed before being used as paths. Each cached object has:

- a data file containing bytes at their source offsets;
- a JSON sidecar containing the inclusive ranges that completed successfully.

File length alone is never treated as proof that a range is cached. Sidecar updates happen only after data has been flushed and are committed through a temporary-file rename.

## Known Limitations

- No Android JNI, iOS XCFramework, or HarmonyOS N-API adapter
- No public start/stop/lifecycle API suitable for mobile hosts
- No concurrent request coalescing for the same missing range
- No full Range, HLS, corruption-recovery, or process-restart integration suite
- DNS policy validation and the connector's later DNS lookup are not yet pinned to the same resolved address, leaving a DNS-rebinding time-of-check/time-of-use gap
- The dependency graph still contains overlapping HTTP clients and broad Tokio features

## Client Integration

See [Mobile Client Integration](docs/mobile-client-integration.md) for the proposed iOS, Android, and HarmonyOS SDK architecture, lifecycle contract, packaging targets, and POC acceptance criteria.

## Project Layout

```text
src/
├── data_source/          # Network and file sources
├── handlers/             # Cache, network, mixed-source, and response handlers
├── hls/                  # HLS parsing, rewriting, and segment handling
├── storage/              # Disk engine, completed ranges, cleanup, and block state
├── utils/                # Range parsing, errors, logging, and network policy
├── data_request.rs       # Proxy request and stable cache identity
├── data_source_manager.rs
├── request_handler.rs
└── server.rs
```

## Contributing

Keep changes focused and include regression tests for behavior that affects ranges, cache integrity, network policy, cleanup, or concurrency. Run before submitting:

```bash
cargo test --locked
git diff --check
```

## License

[MIT](LICENSE)
