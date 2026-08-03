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

The unit suite covers stable cache keys, sparse-range correctness, range metadata
persistence, physical deletion, cleanup behavior, block-lock regression, tee
fan-out, startup index warm-up, and core network-policy rejection cases.

### End-to-end HTTP tests

`tests/end_to_end.rs` drives the real server over real HTTP against a local
origin that speaks `Range`. It needs the `allow-private-upstream` feature,
because the origin binds `127.0.0.1` and the production policy rejects
loopback upstreams:

```bash
cargo test --locked --features allow-private-upstream
```

It asserts served-then-cached behavior, cache hits for subranges, mixed
cache-plus-network stitching (including the write-back of the network part),
open-ended ranges, concurrent identical ranges, disjoint seeks, rejection of an
origin that ignores `Range`, cache survival across a restart, and that error
bodies leak neither the upstream URL nor the cache path.

It also covers the acceptance items from
[docs/mobile-client-integration.md](docs/mobile-client-integration.md): a
rotated signed URL hits the same cache entry (query is not part of the cache
identity), a cached range still replays after the origin goes offline, two
players asking for overlapping ranges each get correct bytes, an ExoPlayer-style
sequence of open-ended seeks returns correct bytes and `Content-Range` at every
step, and a client that disconnects mid-transfer still leaves a complete cache
entry behind.

Two of those tests need a specific origin shape to mean anything, both found by
mutation testing rather than by reasoning:

- The disconnect test uses a raw socket, not `hyper::Client`. The client pools
  connections, so dropping a `Response` makes it *drain* the rest of the body to
  reuse the socket — the proxy sees a well-behaved client and there is no
  disconnect to observe. It also needs an origin that emits many small chunks;
  with a single-chunk body the forwarding loop finishes before the drop happens.
- The offline test asserts an *uncached* range fails before trusting that the
  origin is down. `JoinHandle::abort()` is not enough to take a hyper server
  down: it stops the accept loop, but each live connection is its own task, and
  the proxy's pooled keep-alive connection kept being served normally.

The feature only widens `is_allowed_target` to accept loopback and private
addresses, and any binary built with it prints a warning to stderr on first
upstream admission. **Never enable it in a shipped build.** Link-local
addresses, including cloud metadata endpoints, stay rejected either way.

### Manual testing with a real player

```bash
# synthetic bytes, protocol correctness only
cargo run --features allow-private-upstream --example local_playground

# a real media file you can actually play and seek
cargo run --features allow-private-upstream --example local_playground -- /path/to/video.mp4
```

This starts the fake origin and the proxy together, logs every upstream range
the origin receives, and prints ready-to-paste `curl`, `ffplay`, and `mpv`
commands. Watching the origin log while seeking shows which ranges come from
cache.

A browser page cannot drive the proxy: `<video>` cannot send the custom
identity headers the request contract requires, so use a client that can set
headers.

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
- `start`/`stop` and a config struct exist, but `start` binds a fixed port; binding port 0 and reporting the assigned port is still missing, and the lifecycle has no explicit state machine
- No concurrent request coalescing for the same missing range. Concurrent identical ranges are correct but redundant: each fetches upstream separately, and a cache writer blocked on the per-key write lock is abandoned after a one-second grace period rather than merged
- Range and process-restart behavior now have an end-to-end suite; HLS and corruption-recovery still do not
- A request without a `Range` header answers `206` rather than `200`. Most players tolerate it, but it is not what RFC 7233 specifies
- DNS policy validation and the connector's DNS lookup are two separate lookups, so they are not pinned to the same address. Both filter to public addresses, so rebinding cannot reach `connect`. For IP-literal upstreams the connector skips the resolver entirely, which makes `NetworkPolicy::validate` the only line of defense; every new upstream path must therefore call it
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

Licensed under the [Apache License 2.0](LICENSE). You may use, modify, and
distribute this project, including for commercial purposes, subject to the
terms of the license.
