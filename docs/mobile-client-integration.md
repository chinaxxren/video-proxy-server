# Mobile Client Integration

English | [简体中文](mobile-client-integration.zh-CN.md)

This document describes the proposed integration of Media Proxy Cache into iOS, Android, and HarmonyOS applications.

> Status: design target, not a released SDK. The repository currently provides a localhost Rust proxy executable and core cache components. The FFI layers, mobile packages, and lifecycle APIs described below still need to be implemented and verified on real devices.

## Goal

Compile one Rust cache core for each supported platform and expose a small platform-native API. Application code supplies media identity and the current signed source URL; the SDK returns a localhost playback URL for the platform media player.

```text
Application
    |
    | asset identity + current source URL
    v
Platform Adapter
    |
    | FFI
    v
Media Proxy Cache Core (Rust)
    |
    | http://127.0.0.1:{dynamic-port}/...
    v
AVPlayer / Media3 / HarmonyOS AVPlayer
```

The same Rust source is compiled separately for each CPU ABI. A single binary is not shared across platforms.

## Target Packages

| Platform | Rust output | Distributed package | Native API |
| --- | --- | --- | --- |
| iOS | Static libraries | XCFramework | Swift |
| Android | Shared libraries | AAR | Kotlin/JNI |
| HarmonyOS | Shared libraries | HAR | ArkTS/N-API |

Expected architecture targets:

- iOS device: `aarch64-apple-ios`
- iOS Simulator: `aarch64-apple-ios-sim` and, if required, `x86_64-apple-ios`
- Android: `arm64-v8a`, with `armeabi-v7a` and `x86_64` only when product support requires them
- HarmonyOS: `aarch64-unknown-linux-ohos` and `armv7-unknown-linux-ohos`

## Proposed Host API

Names are illustrative. Final platform APIs should follow native naming conventions while preserving the same behavior.

```text
create(config) -> client
start() -> localEndpoint
makePlaybackUrl(source, identity) -> localUrl
updateSource(identity, source)
stop()
clearCache(scope)
cacheUsage() -> bytes
```

Configuration:

```text
cacheDirectory       Host-owned writable application directory
maxCacheBytes        Hard cache capacity
maxFileCount         Optional object count limit
allowedHosts         Exact upstream host allowlist
requestTimeout       Upstream timeout
logLevel             Must never enable signed URL logging
```

Media identity:

```text
userId
assetId
assetRevision
```

Source information:

```text
url                  Current signed or unsigned HTTP(S) URL
headers              Optional approved upstream headers
expiresAt            Optional source expiry time
```

The source URL is mutable network information. It must never be used as the cache identity.

## Lifecycle Contract

The Core must implement these guarantees before mobile packaging begins:

1. `start` binds an HTTP/1.1 endpoint to `127.0.0.1` on port `0`, then returns the actual assigned port. The adapter must not require HTTP/2 or h2c for localhost playback.
2. Repeated `start` calls are idempotent or return a documented state error.
3. `stop` stops accepting requests, cancels upstream work, flushes committed metadata, releases the socket, and completes within a bounded timeout.
4. A client instance owns its runtime resources; dropping or destroying it cannot leave detached server tasks behind.
5. The Host supplies the cache directory. The Core must not assume a desktop working directory.
6. Recovery after process termination treats incomplete writes as cache misses and preserves completed ranges.
7. Multiple player requests for the same missing range are coalesced or coordinated without corrupting cache metadata.

Recommended states:

```text
Created -> Starting -> Running -> Stopping -> Stopped
                   \-> Failed
```

The Rust Core now exposes these states through `ProxyServerStatus`, accepts
`ProxyConfig { port: 0, .. }`, reports the assigned port through
`wait_until_ready()`/`bound_port()`, rejects a second `start`, and performs a
bounded stop. Platform adapters must own the running `start` task and await it
during teardown.

## Playback Flow

1. The app authenticates and obtains the current media source URL.
2. The app starts one shared proxy instance for the application process.
3. The app calls `makePlaybackUrl` with the stable identity and source information.
4. The adapter registers the source with the Core and returns a localhost URL that contains an opaque request ID, not the signed URL.
5. The platform player opens the localhost URL and sends Range requests normally.
6. The Core serves completed ranges from disk and fetches missing ranges from the registered source.
7. If authorization expires, the Core asks the Host for a refreshed source and retries only the failed upstream request.
8. The app stops the Core during controlled shutdown; unexpected process termination is handled by recovery on the next start.

Do not place a signed URL in the localhost path or query string. It can be exposed through player diagnostics, analytics, crash logs, or operating-system networking tools.

## Source Refresh

Production playback needs a Host callback because signed URLs can expire while a player is active.

```text
refreshSource(identity, reason) -> new Source
```

The contract should define:

- which upstream responses trigger refresh, normally `401` or `403`;
- one in-flight refresh per media identity;
- a bounded retry count;
- cancellation when playback or the Core stops;
- callback threading and timeout behavior;
- rejection when the refreshed URL violates the allowlist or network policy.

The callback must not expose the signed URL through logs or error messages.

## iOS Adapter

Package the Rust static libraries and C header as an XCFramework, then wrap the C ABI with a Swift API.

Recommended shape:

```swift
let cache = MediaProxyCache(configuration: configuration)
let endpoint = try await cache.start()
let playbackURL = try cache.makePlaybackURL(source: source, identity: identity)
let player = AVPlayer(url: playbackURL)
```

iOS work items:

- build device and Simulator slices;
- expose an exception-free C ABI with explicit error codes and owned buffers;
- make Swift concurrency and callback queues explicit;
- use an App Support or Caches directory supplied by the app;
- verify local HTTP playback under App Transport Security policy;
- verify `AVPlayer` Range and HLS behavior;
- validate background audio, interruption, route-change, and process-restoration scenarios;
- ensure the XCFramework contains no simulator slice in App Store device output.

## Android Adapter

Compile Rust shared libraries, expose JNI bindings, and package Kotlin APIs and native libraries in an AAR.

Recommended shape:

```kotlin
val cache = MediaProxyCache.create(context, configuration)
val endpoint = cache.start()
val playbackUri = cache.makePlaybackUri(source, identity)
val player = ExoPlayer.Builder(context).build()
player.setMediaItem(MediaItem.fromUri(playbackUri))
```

Android work items:

- package one `.so` per supported ABI;
- keep JNI handles opaque and validate every native handle;
- use an app-provided cache directory;
- define the effect of process death and service recreation;
- verify localhost cleartext policy in the application network-security configuration;
- test Media3/ExoPlayer Range, seeking, HLS, foreground-service, and background playback;
- avoid blocking Binder, main, or player threads with FFI calls;
- add R8/ProGuard keep rules for JNI entry points when required.

## HarmonyOS Adapter

Compile the Rust shared library for the HarmonyOS toolchain, expose a stable C ABI through N-API, and package the ArkTS API and native library in a HAR.

Recommended shape:

```typescript
const cache = await MediaProxyCache.create(context, configuration)
const endpoint = await cache.start()
const playbackUrl = await cache.makePlaybackUrl(source, identity)
await avPlayer.setUrl(playbackUrl)
```

HarmonyOS work items:

- validate the Rust target and native build chain against the supported HarmonyOS SDK version;
- package ARM64 first and expand only from the product device matrix;
- keep N-API calls asynchronous and document callback threads;
- use a sandbox path supplied by the application context;
- verify localhost networking and cleartext policy;
- test AVPlayer Range, seek, HLS, background playback, and application recovery;
- verify HAR loading and symbol visibility in both debug and release builds.

## Security Requirements

- Bind only to `127.0.0.1`; do not bind to all interfaces.
- Use an unguessable per-process token or opaque request ID so unrelated local callers cannot freely use the proxy.
- Require an exact upstream host allowlist.
- Reject non-HTTP(S) schemes, URL credentials, private/reserved addresses, and unsafe redirects.
- Production HTTPS uses Rustls with bundled WebPKI roots. Private enterprise CAs are not trusted unless the Core adds an explicit host-supplied trust-store API.
- Pin validated DNS results to the connection to close the DNS-rebinding time-of-check/time-of-use gap.
- Never log source URLs, authorization headers, cookies, opaque request IDs, or stable cache identities.
- Restrict forwarded upstream headers to an explicit allowlist.
- Keep cache files inside the application sandbox and follow platform data-protection requirements.
- Define whether cached media must be encrypted at rest based on product and content-license requirements.

## Threading And FFI Rules

- No Rust panic may unwind across an FFI boundary.
- All returned strings and byte buffers need explicit ownership and release functions.
- Long-running operations must be asynchronous and cancellable.
- Callbacks into Swift, Kotlin, or ArkTS must use documented threads/queues.
- Destroying a Host object must invalidate future callbacks safely.
- Error values should contain stable codes and sanitized messages, never source credentials.

## POC Acceptance Criteria

Each platform POC should demonstrate:

- start on a dynamic localhost port and deterministic stop;
- first playback downloads and caches media;
- second playback with a different signed URL hits the same cache identity;
- seek to cached and uncached ranges;
- two concurrent players requesting overlapping ranges;
- signed URL expiry and refresh without restarting playback;
- offline replay of completed ranges;
- recovery after forced process termination during a write;
- cache capacity enforcement and physical deletion;
- rejection of disallowed hosts and private addresses;
- no signed URL in application logs, crash reports, or localhost request URLs;
- foreground/background transitions appropriate to the platform;
- real-device testing in addition to Simulator/emulator testing.

## Recommended Delivery Order

1. Complete the remaining Core host contracts: opaque request registration and source-refresh callback. Start/stop, dynamic port, and Host cache-directory injection are already available through the C ABI.
2. Keep the existing unit and desktop integration suites for Range, concurrent requests, corruption recovery, cleanup, HLS, and network policy as release gates.
3. Build the Android JNI/AAR POC and validate Media3 on real devices.
4. Freeze the shared lifecycle and error contracts after the Android POC.
5. Build the iOS XCFramework/Swift adapter and validate AVPlayer.
6. Build the HarmonyOS HAR/N-API adapter and validate AVPlayer.
7. Run the cross-platform acceptance matrix before declaring the SDK production-ready.

## Current Repository Gap

The repository now exposes a C ABI with create/start/stop/destroy, dynamic-port discovery, and a Host-provided cache directory. It also includes build/release scripts and ownership-wrapper templates. It does not yet provide production JNI/AAR, XCFramework, or N-API/HAR packages, and the opaque request registry/source-refresh callback contract is still missing. Treat this document as the implementation and acceptance contract for the remaining mobile SDK work, not as a claim that those platform packages have been validated on real devices.
