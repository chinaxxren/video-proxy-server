import Foundation
import MediaProxyCacheCore

/// Swift ownership wrapper for the shared Rust C ABI.
public final class MediaProxyCache: @unchecked Sendable {
    private let lock = NSLock()
    private var handle: OpaquePointer?

    public init?(port: UInt16, cacheDirectory: String, allowedHosts: [String]) {
        guard !cacheDirectory.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              !allowedHosts.isEmpty,
              allowedHosts.allSatisfy({ host in
                  !host.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !host.contains(",")
              }) else { return nil }
        let hosts = allowedHosts.joined(separator: ",")
        handle = cacheDirectory.withCString { cachePath in
            hosts.withCString { hostList in
                proxy_server_create_with_hosts(port, cachePath, hostList)
            }
        }
        guard handle != nil else { return nil }
    }

    deinit { close() }

    public func start() throws -> UInt16 {
        lock.lock()
        defer { lock.unlock() }
        guard let handle else { throw NSError(domain: "MediaProxyCache", code: 1) }
        let port = proxy_server_start(handle)
        guard port != 0 else { throw NSError(domain: "MediaProxyCache", code: 2) }
        return port
    }

    public func stop() {
        lock.lock()
        defer { lock.unlock() }
        if let handle { proxy_server_stop(handle) }
    }

    public func close() {
        lock.lock()
        defer { lock.unlock() }
        if let handle { proxy_server_destroy(handle); self.handle = nil }
    }
}
