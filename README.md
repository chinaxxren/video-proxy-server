# Media Proxy Cache

English | [简体中文](README.zh-CN.md)

A Rust HTTP media proxy with byte-range caching and HLS support. The server listens on `127.0.0.1`, streams data from approved upstream hosts, and persists completed byte ranges on disk.

> Status: prototype. The core safety and cache-correctness issues have initial fixes and regression tests, but the project is not yet recommended as a production dependency. The C ABI lifecycle, Android JNI bridge, HarmonyOS N-API bridge, and native iOS XCFramework packaging are available; production AAR, Swift, and HAR adapters are still pending. Localhost caller authentication is intentionally outside this project's current scope.

## Features

- HTTP and HTTPS upstream sources
- HTTP `Range` requests and partial-content responses
- `GET` and metadata-only `HEAD` requests
- Disk-backed chunk caching
- Persistent completed-range metadata, independent of sparse file length
- Mixed cache/network responses for a verified contiguous cache prefix
- HLS playlist rewriting and segment proxying
- Size/count-based cache cleanup with physical file deletion
- Stable cache identity independent of signed URLs
- Upstream host allowlist and private-address rejection
- Pure-Rust TLS with bundled WebPKI roots for consistent mobile builds
- Localhost-only HTTP/1.1 listener (HTTP and HTTPS origins are supported)
- C ABI lifecycle entry points for mobile adapters (`include/media_proxy_cache.h`)
- Optional compliance-gated P2P byte-provider boundary (disabled by default)

## Requirements

- Rust 1.85 or later
- Cargo
- Linux, macOS, or Windows

## Build And Test

```bash
cargo build --locked
cargo test --locked
```

### Optional P2P boundary

Build and test the optional module with:

```bash
cargo test --locked --features p2p
```

This feature is not a BitTorrent client. It does not accept magnet links and
does not implement DHT, public trackers, or peer discovery. The Host must make
an explicit authorization decision and provide a stable content ID, total
length, full-content SHA-256, and a per-piece SHA-256 manifest. Core verifies
every supplied piece before returning bytes. Keep the feature disabled when the
application has no authorized P2P source.

See [Optional P2P Client Integration](docs/p2p-client-integration.md) for the
manifest, C callback, lifecycle, playback URL, and acceptance contract.

### Dependency security

CI runs the RustSec audit on every push and pull request. The same checks can be
reproduced locally:

```bash
cargo install cargo-audit --locked
cargo audit
cargo install cargo-license --locked
cargo license --avoid-dev-deps --avoid-build-deps
```

The current lockfile audit covers 116 packages with no RustSec advisories. The
production dependency licenses are permissive Apache-2.0, MIT, ISC,
BSD-3-Clause, Unicode-3.0, Unlicense, CDLA-Permissive-2.0, or multi-license
expressions with a permissive option; no mandatory GPL, AGPL, or SSPL dependency
is present.

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
cargo run --features allow-private-upstream --example local_playground -- ./aa.mp4
```

This starts the local Range origin, proxy, and browser test page together. Open
the printed Web test URL to play and seek `aa.mp4`, issue custom byte ranges,
run repeated/overlapping range checks, and load a real fMP4 HLS stream. When
`ffmpeg` is available, the playground remuxes `aa.mp4` into a local m3u8,
initialization segment, and media segments at startup. The HLS check verifies
playlist MIME, rewritten URIs, every segment response, browser MediaSource
decoding, and a repeated segment cache hit.

The same-origin development gateway adds the identity headers that `<video>`
cannot send itself; all media bytes still pass through the real proxy core. The
terminal logs every upstream range and HLS asset, so a repeated cached request
should not produce another origin log entry.

The command also prints ready-to-paste `curl`, `ffplay`, and `mpv` commands.
`allow-private-upstream` and the development gateway are only for local tests
and are not included in the production server path.

### Mobile FFI preview

The crate also builds `staticlib` and `cdylib` artifacts. Mobile adapters can
include [`include/media_proxy_cache.h`](include/media_proxy_cache.h), create a
server with a host-owned cache directory, start it on a fixed port or port `0`,
and release it with `stop`/`destroy`. This is a preview ABI. Build and release
packaging scripts are provided; platform-specific JNI, Swift, and N-API wrappers
still require integration in the host projects.

Real upstream access must use `proxy_server_create_with_hosts` and pass the
comma-separated host allowlist. The simpler `proxy_server_create` intentionally
uses the deny-all policy.

The native build helper is available at `scripts/build-mobile.sh`:

```bash
PLATFORM=ios ./scripts/build-mobile.sh dist/mobile
PLATFORM=android ./scripts/build-mobile.sh dist/mobile
PLATFORM=harmony ./scripts/build-mobile.sh dist/mobile
PLATFORM=macos ./scripts/build-mobile.sh dist/desktop
PLATFORM=windows ./scripts/build-mobile.sh dist/desktop
```

It requires the corresponding Rust targets and copies the C header beside each
platform's native artifacts. Android Kotlin packaging should consume the
generated `.so` files through an Android library module.

Adapter ownership templates are under `platform/android`, `platform/ios`, and
`platform/harmony`. They are API contracts only until each host project links
the generated native library and supplies its JNI, Swift module map, or N-API
bridge.

The same Core also supports desktop builds. macOS uses Apple Silicon and Intel
targets; Windows uses the GNU x86_64 target by default and requires a MinGW
linker on the build host. Desktop consumers can use the generated `cdylib` or
`staticlib` directly.

Tagged pushes matching `v*` run `.github/workflows/release.yml` and publish
macOS ARM64/Intel, Windows x86_64, and Linux x86_64 archives. The workflow can
also be run manually to produce downloadable Actions artifacts without creating
a GitHub Release. Each archive includes a matching `.sha256` file; verify a
download on macOS/Linux with `shasum -a 256 -c <archive>.sha256` or on Windows
with `Get-FileHash <archive> -Algorithm SHA256`.

The separate `.github/workflows/mobile.yml` workflow builds iOS and Android
native libraries on GitHub-hosted runners and publishes them as Release assets
for tagged pushes. HarmonyOS builds are opt-in: set repository variable
`ENABLE_HARMONY_BUILD=true`, secret `OHOS_NDK_URL` to a downloadable OHOS NDK
archive, and `OHOS_HVIGOR_URL` to a downloadable archive containing executable
`hvigorw`. Set `OHOS_NDK_SHA256` and `OHOS_HVIGOR_SHA256` to the lowercase or
uppercase SHA-256 digest of the corresponding immutable archive. Without that
configuration the HarmonyOS job is skipped or fails before extracting tools.

On macOS, `./scripts/test-ffi-macos.sh` builds a small C program against the
release dylib and exercises the complete create/start/stop/destroy lifecycle.

### HLS playlist refresh policy

Rewritten playlist bodies are kept in a bounded in-memory cache. VOD playlists
(`EXT-X-ENDLIST`) are refreshed every 5 minutes, master playlists every 30
seconds, and live media playlists every half target duration (clamped to
1-10 seconds). At most 128 playlist bodies and 8 MiB of playlist body/key data
are retained; the oldest entries are evicted first. Segment bytes continue to
use the persistent disk cache and are independent of this playlist-body TTL.

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

Optional limits can be set with environment variables. `PROXY_SHUTDOWN_TIMEOUT_MS`
controls the bounded graceful-drain period (default `5000`); after it expires,
the server cancels its outstanding upstream/cache forwarding tasks. The request
header deadline and count limit are configurable with
`PROXY_REQUEST_HEADER_TIMEOUT_MS` (default `10000`) and
`PROXY_MAX_REQUEST_HEADERS` (default `64`). The other limits are
`PROXY_MAX_CACHE_BYTES`, `PROXY_MAX_FILES`, `PROXY_MAX_CONCURRENT`, and
`PROXY_CLEANUP_SECS`.

You can also run the maintained client example:

```bash
cargo run --example proxy_client -- \
  https://media.w3.org/2010/05/sintel/trailer.mp4
```

## Embed The Server

Configure approved hosts explicitly:

```rust
use proxy_server::server::{ProxyConfig, ProxyServer};

#[tokio::main]
async fn main() {
    let server = std::sync::Arc::new(ProxyServer::with_config(ProxyConfig {
        port: 0,
        cache_dir: "./cache".into(),
        allowed_hosts: vec!["media.example.com".into(), "cdn.example.com".into()],
        ..Default::default()
    }));
    let running = tokio::spawn({
        let server = server.clone();
        async move { server.start().await }
    });
    let port = server.wait_until_ready().await.unwrap();
    println!("proxy ready at http://127.0.0.1:{port}");
    server.stop();
    running.await.unwrap().unwrap();
}
```

`ProxyServer::new` uses a deny-all network policy. Prefer `with_allowed_hosts` for any server that needs upstream access.

## Proxy Request Contract

Send the current upstream URL in `X-Original-Url` and provide the two cache identity headers currently supported by the core:

```bash
curl 'http://127.0.0.1:8080/proxy/media' \
  -H 'X-Original-Url: https://media.example.com/audio/song.m4a?token=short-lived' \
  -H 'Range: bytes=0-65535' \
  -H 'X-Cache-Asset-Id: song-456' \
  -H 'X-Cache-Asset-Revision: 7'
```

The current cache identity is derived from the upstream scheme/host/port/path plus:

```text
assetId + assetRevision
```

The signed URL query is only the current network source. Changing its token does not create a different cache entry. `userId` is not yet part of the core cache key; multi-user production integration must add a trusted host-provided tenant/user boundary before enabling shared caches.

`HEAD` follows the same request contract. A cold HEAD performs only a
`bytes=0-0` upstream probe to discover total length and content type; it does
not mark media bytes as cached. Once metadata is persisted, later HEAD requests
do not contact the upstream. Single and open-ended byte ranges are supported;
multiple ranges are explicitly rejected with `416`.

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
- a versioned JSON sidecar containing the cache key, upstream metadata, and the inclusive ranges that completed successfully.

File length alone is never treated as proof that a range is cached. Sidecar updates happen only after data has been flushed and are committed through a temporary-file rename.

During startup recovery, the cache removes interrupted sidecar temporary files and validates each committed entry against its hashed path and data-file length. Malformed metadata, unknown future schema versions, overlapping or unsorted ranges, and ranges extending beyond the data file are treated as untrusted; their data and sidecar files are removed instead of being exposed as cache hits. Legacy v0 metadata remains readable and is upgraded to the current schema on its next write.

## Known Limitations

- No production-validated three-ABI Android AAR, production iOS Swift, or HarmonyOS HAR package; ARM64 Android Media3 and iOS Simulator player POCs are validated
- The Core exposes dynamic port assignment, readiness waiting, and lifecycle states; platform-specific ownership across app background/foreground transitions still needs adapter validation
- Concurrent identical ranges are coalesced through the single-flight path; cache-side backpressure is abandoned after a one-second grace period rather than blocking playback
- Range, HLS, process-restart, and corruption-recovery behavior have focused unit/E2E coverage; broader mobile-player coverage is still needed
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
