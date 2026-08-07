import Foundation

/// Swift ownership wrapper for the shared Rust C ABI.
public final class MediaProxyCache {
    private var handle: OpaquePointer?

    public init?(port: UInt16, cacheDirectory: String) {
        guard !cacheDirectory.isEmpty else { return nil }
        handle = cacheDirectory.withCString { proxy_server_create(port, $0) }
        guard handle != nil else { return nil }
    }

    deinit { close() }

    public func start() throws -> UInt16 {
        guard let handle else { throw NSError(domain: "MediaProxyCache", code: 1) }
        let port = proxy_server_start(handle)
        guard port != 0 else { throw NSError(domain: "MediaProxyCache", code: 2) }
        return port
    }

    public func stop() { if let handle { proxy_server_stop(handle) } }

    public func close() {
        if let handle { proxy_server_destroy(handle); self.handle = nil }
    }
}
