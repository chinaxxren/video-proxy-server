import Foundation

@main
struct MediaProxyCacheSmokeTest {
    static func main() throws {
        do {
            _ = try MediaProxyCache(
                configuration: .init(
                    cacheDirectory: URL(string: "https://example.com/cache")!,
                    allowedHosts: ["media.example.com"]
                )
            )
            fatalError("A non-file cache URL must be rejected")
        } catch MediaProxyCacheError.invalidCacheDirectory {
            // Expected.
        }

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        let cache = try MediaProxyCache(
            configuration: .init(
                cacheDirectory: directory,
                allowedHosts: ["media.example.com"]
            )
        )
        let endpoint = try cache.start()
        precondition(endpoint.host == "127.0.0.1")
        precondition(endpoint.port != nil && endpoint.port != 0)

        do {
            _ = try cache.start()
            fatalError("A repeated start must be rejected")
        } catch MediaProxyCacheError.alreadyStarted {
            // Expected.
        }

        cache.stop()
        cache.stop()

        do {
            _ = try cache.start()
            fatalError("A stopped one-shot instance must not restart")
        } catch MediaProxyCacheError.stopped {
            // Expected.
        }
    }
}
