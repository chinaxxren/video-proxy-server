import Foundation
import MediaProxyCacheCore

public struct MediaProxyCacheConfiguration: Sendable {
    public let cacheDirectory: URL
    public let port: UInt16
    public let allowedHosts: [String]

    public init(cacheDirectory: URL, port: UInt16 = 0, allowedHosts: [String]) {
        self.cacheDirectory = cacheDirectory
        self.port = port
        self.allowedHosts = allowedHosts
    }
}

public enum MediaProxyCacheError: Error, Equatable {
    case invalidCacheDirectory
    case creationFailed
    case alreadyStarted
    case stopped
    case startFailed
}

public final class MediaProxyCache: @unchecked Sendable {
    private let lock = NSLock()
    private var handle: OpaquePointer?
    private var boundPort: UInt16?
    private var isStopped = false

    public init(configuration: MediaProxyCacheConfiguration) throws {
        guard configuration.cacheDirectory.isFileURL else {
            throw MediaProxyCacheError.invalidCacheDirectory
        }

        let path = configuration.cacheDirectory.path
        let hosts = configuration.allowedHosts.joined(separator: ",")
        let created = path.withCString { pathPointer in
            hosts.withCString { hostsPointer in
                proxy_server_create_with_hosts(configuration.port, pathPointer, hostsPointer)
            }
        }
        guard let created else {
            throw MediaProxyCacheError.creationFailed
        }
        handle = created
    }

    deinit {
        lock.lock()
        let ownedHandle = handle
        handle = nil
        boundPort = nil
        lock.unlock()
        if let ownedHandle {
            proxy_server_destroy(ownedHandle)
        }
    }

    public func start() throws -> URL {
        lock.lock()
        defer { lock.unlock() }
        guard boundPort == nil else {
            throw MediaProxyCacheError.alreadyStarted
        }
        guard !isStopped else {
            throw MediaProxyCacheError.stopped
        }
        guard let handle else {
            throw MediaProxyCacheError.creationFailed
        }
        let port = proxy_server_start(handle)
        guard port != 0, let endpoint = URL(string: "http://127.0.0.1:\(port)") else {
            throw MediaProxyCacheError.startFailed
        }
        boundPort = port
        return endpoint
    }

    public func stop() {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, boundPort != nil else { return }
        proxy_server_stop(handle)
        boundPort = nil
        isStopped = true
    }

#if MEDIA_PROXY_CACHE_ENABLE_LIBRQBIT
    public func addAuthorizedTorrent(magnetURI: String) throws -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, let boundPort, boundPort != 0, magnetURI.hasPrefix("magnet:?") else {
            throw MediaProxyCacheError.startFailed
        }
        let torrentID = magnetURI.withCString {
            proxy_torrent_add_authorized(handle, $0, 1)
        }
        guard torrentID >= 0 else { throw MediaProxyCacheError.creationFailed }
        return torrentID
    }

    public func removeTorrent(_ torrentID: Int64, deleteFiles: Bool = false) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, torrentID >= 0 else { return false }
        return proxy_torrent_remove(handle, torrentID, deleteFiles ? 1 : 0) == 1
    }

    public func torrentPlaybackURL(torrentID: Int64, fileID: Int) -> URL? {
        lock.lock()
        defer { lock.unlock() }
        guard torrentID >= 0, fileID >= 0, let boundPort else { return nil }
        return URL(string: "http://127.0.0.1:\(boundPort)/torrent/\(torrentID)/\(fileID)")
    }
#endif
}
