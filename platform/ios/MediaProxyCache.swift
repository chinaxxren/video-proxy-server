import Foundation
import MediaProxyCacheCore

/// Swift ownership wrapper for the shared Rust C ABI.
public final class MediaProxyCache: @unchecked Sendable {
    private let lock = NSLock()
    private var handle: OpaquePointer?
    private var boundPort: UInt16 = 0

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
        boundPort = port
        return port
    }

    public func stop() {
        lock.lock()
        defer { lock.unlock() }
        if let handle { proxy_server_stop(handle) }
        boundPort = 0
    }

#if MEDIA_PROXY_CACHE_ENABLE_P2P
    /// Registers `<pieceIndex>.piece` files from a Host-owned sandbox directory.
    public func registerP2PDirectory(manifestJSON: Data, pieceDirectory: URL) throws -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        guard let handle else { throw NSError(domain: "MediaProxyCache", code: 1) }
        guard !manifestJSON.isEmpty, pieceDirectory.isFileURL else {
            throw NSError(domain: "MediaProxyCache", code: 3)
        }
        let sourceID = manifestJSON.withUnsafeBytes { manifest in
            pieceDirectory.path.withCString { directory in
                proxy_p2p_source_register_directory(
                    handle,
                    manifest.bindMemory(to: UInt8.self).baseAddress,
                    manifest.count,
                    directory
                )
            }
        }
        guard sourceID != 0 else { throw NSError(domain: "MediaProxyCache", code: 4) }
        return sourceID
    }

    /// Verifies every piece and the complete authorized content digest.
    public func verifyP2PSource(_ sourceID: UInt64) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, sourceID != 0 else { return false }
        return proxy_p2p_source_verify_complete(handle, sourceID) == 1
    }

    /// Revokes the source and waits for active Provider reads to finish.
    public func removeP2PSource(_ sourceID: UInt64) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, sourceID != 0 else { return false }
        return proxy_p2p_source_remove(handle, sourceID) == 1
    }

    public func p2pPlaybackURL(sourceID: UInt64) throws -> URL {
        lock.lock()
        defer { lock.unlock() }
        guard sourceID != 0, boundPort != 0,
              let url = URL(string: "http://127.0.0.1:\(boundPort)/p2p/\(sourceID)") else {
            throw NSError(domain: "MediaProxyCache", code: 5)
        }
        return url
    }
#endif

#if MEDIA_PROXY_CACHE_ENABLE_LIBRQBIT
    public func addAuthorizedTorrent(magnetURI: String) throws -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, boundPort != 0, magnetURI.hasPrefix("magnet:?") else {
            throw NSError(domain: "MediaProxyCache", code: 6)
        }
        let torrentID = magnetURI.withCString {
            proxy_torrent_add_authorized(handle, $0, 1)
        }
        guard torrentID >= 0 else { throw NSError(domain: "MediaProxyCache", code: 7) }
        return torrentID
    }

    public func removeTorrent(_ torrentID: Int64, deleteFiles: Bool = false) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, torrentID >= 0 else { return false }
        return proxy_torrent_remove(handle, torrentID, deleteFiles ? 1 : 0) == 1
    }

    public func torrentPlaybackURL(torrentID: Int64, fileID: Int) throws -> URL {
        lock.lock()
        defer { lock.unlock() }
        guard torrentID >= 0, fileID >= 0, boundPort != 0,
              let url = URL(string: "http://127.0.0.1:\(boundPort)/torrent/\(torrentID)/\(fileID)") else {
            throw NSError(domain: "MediaProxyCache", code: 8)
        }
        return url
    }
#endif

    public func close() {
        lock.lock()
        defer { lock.unlock() }
        if let handle { proxy_server_destroy(handle); self.handle = nil }
        boundPort = 0
    }
}
