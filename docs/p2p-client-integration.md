# Optional P2P Client Integration

[简体中文](p2p-client-integration.zh-CN.md)

## Scope

The `p2p` feature is a Host-provided byte-source boundary, not a BitTorrent
client. It does not accept magnet links and does not implement DHT, public
trackers, peer discovery, upload, or seeding. Keep it disabled unless the Host
has an authorized source for the exact media content.

## Build

```bash
cargo build --locked --features p2p
cargo test --locked --features p2p
```

Define `MEDIA_PROXY_CACHE_ENABLE_P2P` when compiling native Adapter code that
includes `include/media_proxy_cache.h`.

Mobile packaging remains P2P-free by default. Build a clearly labeled optional
package with:

```bash
P2P_ENABLED=1 PLATFORM=android ./scripts/build-mobile.sh
P2P_ENABLED=1 ./scripts/package-mobile.sh
```

P2P-enabled native builds fail if the resulting library does not export all
three P2P ABI functions: register, complete verification, and remove.
Packaging also requires `build-features.txt` to match `P2P_ENABLED`, preventing
default libraries from being mislabeled as an optional P2P release (or vice versa).
The package step also rejects inputs missing the public C header or all native
proxy library artifacts.

In GitHub Actions, run the `mobile-native` workflow manually and set
`include_p2p` to true. Tag-triggered releases always build the default core
variant. Optional artifacts include `-p2p` in their names.

## Authorization Manifest

Pass UTF-8 JSON to `proxy_p2p_source_register`:

```json
{
  "content_id": "movie-42-revision-7",
  "content_length": 10485760,
  "content_sha256": "64 lowercase or uppercase hex characters",
  "piece_length": 1048576,
  "piece_sha256": ["one 64-character SHA-256 per piece"],
  "authorization_reference": "license-or-entitlement-reference",
  "explicitly_authorized": true
}
```

Core rejects unknown fields, malformed lengths and digests, missing explicit
authorization, inconsistent piece counts, and manifests larger than 1 MiB.
Authorization references stay inside Core and are never returned by HTTP.
Each piece is limited to 8 MiB and each manifest to 16,000 pieces, including
manifests constructed through the Rust API.

## Piece Callback

`ProxyP2pPieceCallback` is invoked twice for a piece:

1. `buffer == NULL`, `capacity == 0`: return the required byte length.
2. Core supplies a buffer: write exactly that number of bytes and return it.

The callback and context may run on Core worker threads and must remain valid
until `proxy_p2p_source_remove` or `proxy_server_destroy`. A piece is accepted
only after its SHA-256 matches the manifest. Calls are single-flight per piece;
verified pieces use a bounded 16 MiB per-source memory cache. Verified pieces
are also persisted below `<cache-directory>/p2p` and can be reused after a Core
restart. Core revalidates every disk-cached piece before serving it; corrupt
entries are deleted and requested from the Host again.
The P2P disk cache uses the Host's `max_cache_bytes` limit and evicts the least
recently used verified pieces when that limit is exceeded.
Core also performs this P2P eviction once during initialization, so pieces left
by a prior process cannot remain over budget while the new process is idle.
The ordinary HTTP cache also counts `<cache-directory>/p2p` toward the same
overall budget during its cleanup cycle, so the two cache classes do not each
receive an independent full-sized allowance.
Its versioned cache identity includes both the whole-content digest and the
piece manifest, so different piece layouts for identical content cannot collide.

`proxy_p2p_source_remove` first prevents new reads and then waits for callbacks
that already started. After it returns, the Host may release the callback
context. A callback must not re-enter P2P Core functions or
`proxy_server_destroy`, because those operations may wait for that callback.
Removing the last registration that references a manifest also purges that
manifest's persistent piece directory. Dropping and recreating Core without an
explicit remove preserves verified pieces for restart recovery.

Adapters may call `proxy_p2p_source_verify_complete` before playback to verify
all pieces and the complete content digest. It returns `1` on success and `0`
on any provider or integrity failure; the operation may fetch the entire asset.
If the complete digest mismatches, Core permanently invalidates every registered
source sharing that manifest cache identity; subsequent Range requests fail and
the Host must register a corrected authorized source. The invalidation survives
Core restart for that identity; explicit removal of its final registration
purges the directory and its marker.

## Playback

Registration returns an opaque ID. Use:

```text
http://127.0.0.1:<bound-port>/p2p/<id>
```

The route supports GET, HEAD, normal/open-ended/suffix Range, and streams large
responses in verified chunks. It never accepts a source URL, magnet, tracker,
or peer address. Remove the ID immediately when authorization is revoked.

## Acceptance

- unauthorized and malformed manifests are rejected;
- corrupt, short, long, missing, and timed-out pieces fail closed;
- concurrent overlapping ranges fetch each piece once;
- GET, HEAD, seek, suffix Range, and 416 behavior work with the target player;
- removing an ID prevents future reads;
- logs and HTTP responses contain no authorization reference or peer details.
