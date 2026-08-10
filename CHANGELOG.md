# Changelog

[简体中文](CHANGELOG.zh-CN.md)

## 0.4.0 - 2026-08-10

### Added

- Opaque media-source registration with stable cache identities and Host-provided signed URL refresh callbacks.
- Android JNI, iOS Swift, and HarmonyOS N-API adapters and release packaging.
- Optional BitTorrent support through `librqbit`, plus protocol-level magnet, tracker, DHT, peer-wire, piece-store, upload, and seeding components.
- Real TCP tests for Range playback, cache recovery, HLS, concurrent requests, and authorization refresh.
- Automated Cargo, platform-feature, FFI, packaging, and RustSec quality gates.

### Changed

- Migrated to Hyper 1 and Rustls, removed the duplicate Reqwest client from the Core, and narrowed Tokio features.
- Source refresh callbacks now write into a Core-owned bounded buffer instead of retaining every returned URL until shutdown.
- Added redacted aggregate runtime metrics across the C, Swift, Kotlin/JNI, and HarmonyOS N-API adapters.
- Added property tests for Range, percent-decoding, Magnet, tracker, and peer-wire parsers.
- Mobile and desktop package workflows use reproducible locked builds and checksum release assets.

### Security

- Enforced exact upstream allowlists, HTTP(S)-only sources, credential rejection, redirect revalidation, and public-address filtering.
- Removed signed URLs from player-facing paths and logs.
- Persisted completed cache ranges so sparse file holes cannot become false cache hits.
- Kept the shared upstream HTTP client crate-private so callers cannot bypass network policy.

### Compatibility

- The source refresh callback ABI changed in 0.4.0. Rebuild all platform adapters together with the Core.
- Real-device playback and background lifecycle acceptance remain the responsibility of each Host application.
