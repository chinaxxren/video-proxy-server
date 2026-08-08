# HarmonyOS HAR Adapter

This directory contains the ArkTS-facing HAR packaging skeleton for the Rust
Core. A HarmonyOS NDK is required because `node_api.h` and the OHOS linker are
provided by the SDK, not by this repository.

Build from the repository root:

```bash
OHOS_NDK_HOME=/path/to/ohos-sdk/native ./scripts/build-harmony-har.sh dist/mobile dist/harmony-sdk
```

The adapter owns an opaque numeric handle. `create` copies UTF-8 inputs,
`start` returns the dynamically bound localhost port, and `close` is idempotent.
N-API calls must be dispatched away from the ArkTS main thread by the host.

This package has not been validated on a physical HarmonyOS device.
