# BitTorrent Client Integration

[简体中文](torrent-client-integration.zh-CN.md)

## Scope

The optional `p2p-librqbit` feature provides Magnet parsing, BitTorrent wire,
HTTP/UDP trackers, DHT, peer discovery, resume data, piece verification, and
Range-oriented streaming. It is disabled by default. The backend is configured
with upload disabled, no listening port, and no UPnP, so it does not seed.

Only add content that the application is legally authorized to retrieve. Core
requires an explicit authorization flag and validates the Magnet URI before it
creates the P2P session. Starting the ordinary localhost proxy alone performs no
BitTorrent network activity.

Core manages at most eight torrents by default. Rust hosts may override this
with `RqbitBackendConfig::max_torrents`. Concurrent additions are serialized,
and Magnet URIs with the same info-hash reuse one torrent ID even when their
tracker parameters differ.

## Build

```bash
cargo build --locked --features p2p-librqbit
LIBRQBIT_ENABLED=1 PLATFORM=android ./scripts/build-mobile.sh dist/mobile
LIBRQBIT_ENABLED=1 PLATFORM=ios ./scripts/build-mobile.sh dist/mobile
LIBRQBIT_ENABLED=1 PLATFORM=harmony ./scripts/build-mobile.sh dist/mobile
```

In `mobile-native`, select `include_librqbit`. Artifacts contain `-librqbit`
and `build-features.txt` records `librqbit_enabled=1`. iOS adapters define
`MEDIA_PROXY_CACHE_ENABLE_LIBRQBIT` automatically during packaging.

## Lifecycle

1. Create and start `MediaProxyCache`.
2. Call `addAuthorizedTorrent(magnetUri)`. This is a blocking operation and
   should run away from the UI thread while metadata is resolved.
3. Call `torrentFiles` to select a file ID. `torrentStatus` reports progress.
4. Play `http://127.0.0.1:<port>/torrent/<torrentId>/<fileId>`.
5. Use `pauseTorrent` and `resumeTorrent` to control network downloading.
6. Call `removeTorrent(id, false)` to forget the session while preserving data,
   or pass `true` to delete downloaded files.
7. Stop and close the proxy. Closing cancels the librqbit session.

The HTTP endpoint supports `GET`, `HEAD`, open-ended ranges, bounded ranges,
and suffix ranges. Reads are streamed in chunks of at most 8 MiB and each chunk
has a 30-second timeout.

## Platform APIs

- Android/Kotlin: `addAuthorizedTorrent`, `torrentFiles`, `torrentStatus`, `removeTorrent`, `torrentPlaybackUrl`
- iOS/Swift: `addAuthorizedTorrent`, `torrentFiles`, `torrentStatus`, `removeTorrent`, `torrentPlaybackURL`
- HarmonyOS/ArkTS: `addAuthorizedTorrent`, `torrentFiles`, `torrentStatus`, `removeTorrent`, `torrentPlaybackUrl`

Torrent IDs may be zero. HarmonyOS represents IDs as decimal strings to avoid
JavaScript integer precision loss. Calling these methods with a default native
artifact that lacks `p2p-librqbit` fails explicitly.
