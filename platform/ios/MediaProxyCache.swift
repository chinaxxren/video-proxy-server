import Foundation
import MediaProxyCacheCore

public struct TorrentFile: Codable, Equatable, Sendable {
    public let fileId: Int
    public let relativePath: String
    public let length: UInt64
    enum CodingKeys: String, CodingKey { case fileId = "file_id"; case relativePath = "relative_path"; case length }
}

public struct TorrentStatus: Codable, Equatable, Sendable {
    public let state: String
    public let totalBytes: UInt64
    public let downloadedBytes: UInt64
    public let uploadedBytes: UInt64
    public let finished: Bool
    public let error: String?
    enum CodingKeys: String, CodingKey {
        case state, finished, error
        case totalBytes = "total_bytes"
        case downloadedBytes = "downloaded_bytes"
        case uploadedBytes = "uploaded_bytes"
    }
}

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

    public func addAuthorizedTorrentFile(_ torrentData: Data) throws -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, boundPort != 0, !torrentData.isEmpty, torrentData.count <= 4 * 1024 * 1024 else {
            throw NSError(domain: "MediaProxyCache", code: 12)
        }
        let torrentID = torrentData.withUnsafeBytes { bytes in
            proxy_torrent_add_file_authorized(
                handle,
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count,
                1
            )
        }
        guard torrentID >= 0 else { throw NSError(domain: "MediaProxyCache", code: 13) }
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

    public func torrentFiles(torrentID: Int64) throws -> [TorrentFile] {
        try torrentJSON(torrentID: torrentID, query: proxy_torrent_files_json, as: [TorrentFile].self)
    }

    public func torrentStatus(torrentID: Int64) throws -> TorrentStatus {
        try torrentJSON(torrentID: torrentID, query: proxy_torrent_status_json, as: TorrentStatus.self)
    }

    public func pauseTorrent(_ torrentID: Int64) -> Bool {
        setTorrentPaused(torrentID, paused: true)
    }

    public func resumeTorrent(_ torrentID: Int64) -> Bool {
        setTorrentPaused(torrentID, paused: false)
    }

    private func setTorrentPaused(_ torrentID: Int64, paused: Bool) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, torrentID >= 0 else { return false }
        return proxy_torrent_set_paused(handle, torrentID, paused ? 1 : 0) == 1
    }

    /// Sets session-wide bytes/second; zero removes the limit.
    public func setTorrentDownloadLimit(bytesPerSecond: UInt32) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let handle else { return false }
        return proxy_torrent_set_download_limit(handle, bytesPerSecond) == 1
    }

    private func torrentJSON<T: Decodable>(
        torrentID: Int64,
        query: (OpaquePointer?, Int64, UnsafeMutablePointer<UInt8>?, Int) -> Int,
        as type: T.Type
    ) throws -> T {
        lock.lock()
        defer { lock.unlock() }
        guard let handle, torrentID >= 0 else { throw NSError(domain: "MediaProxyCache", code: 9) }
        let required = query(handle, torrentID, nil, 0)
        guard required > 1, required <= 4 * 1024 * 1024 + 1 else {
            throw NSError(domain: "MediaProxyCache", code: 10)
        }
        var bytes = [UInt8](repeating: 0, count: required)
        let written = bytes.withUnsafeMutableBufferPointer {
            query(handle, torrentID, $0.baseAddress, $0.count)
        }
        guard written == required, bytes.removeLast() == 0 else {
            throw NSError(domain: "MediaProxyCache", code: 11)
        }
        return try JSONDecoder().decode(type, from: Data(bytes))
    }
#endif

    public func close() {
        lock.lock()
        defer { lock.unlock() }
        if let handle { proxy_server_destroy(handle); self.handle = nil }
        boundPort = 0
    }
}
