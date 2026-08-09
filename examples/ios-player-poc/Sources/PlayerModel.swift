import AVFoundation
import Foundation

@MainActor
final class PlayerModel: ObservableObject {
    @Published private(set) var status = "Starting proxy"
    @Published private(set) var proxyPort: UInt16 = 0
    @Published private(set) var elapsed = "00:00"
    @Published private(set) var cacheSize = "0 B"

    let player = AVPlayer()

    private var proxy: MediaProxyCache?
    private var timeObserver: Any?
    private var statusObserver: NSKeyValueObservation?
    private let cacheDirectory: URL

    init() {
        cacheDirectory = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("MediaProxyCachePlayerPOC", isDirectory: true)
        start()
    }

    deinit {
        if let timeObserver {
            player.removeTimeObserver(timeObserver)
        }
        proxy?.close()
    }

    func togglePlayback() {
        if player.timeControlStatus == .playing {
            player.pause()
            status = "Paused"
        } else {
            player.play()
            status = "Playing"
        }
    }

    func seekForward() {
        let target = CMTimeAdd(player.currentTime(), CMTime(seconds: 10, preferredTimescale: 600))
        player.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero)
    }

    private func start() {
        guard let origin = ProcessInfo.processInfo.environment["MEDIA_PROXY_ORIGIN_URL"] else {
            status = "Set MEDIA_PROXY_ORIGIN_URL"
            return
        }
        guard let originURL = URL(string: origin), let host = originURL.host else {
            status = "Invalid origin URL"
            return
        }

        try? FileManager.default.createDirectory(
            at: cacheDirectory,
            withIntermediateDirectories: true
        )
        guard let proxy = MediaProxyCache(
            port: 0,
            cacheDirectory: cacheDirectory.path,
            allowedHosts: [host]
        ) else {
            status = "Proxy initialization failed"
            return
        }
        self.proxy = proxy

        do {
            proxyPort = try proxy.start()
            let playbackURL = URL(string: "http://127.0.0.1:\(proxyPort)/playback")!
            let headers = [
                "X-Original-Url": origin,
                "X-Cache-User-Id": "ios-poc-user",
                "X-Cache-Asset-Id": "aa-video",
                "X-Cache-Asset-Revision": "1",
            ]
            let asset = AVURLAsset(
                url: playbackURL,
                options: ["AVURLAssetHTTPHeaderFieldsKey": headers]
            )
            let item = AVPlayerItem(asset: asset)
            observe(item)
            player.replaceCurrentItem(with: item)
            player.play()
            status = "Loading through proxy"
        } catch {
            status = "Proxy start failed: \(error.localizedDescription)"
        }
    }

    private func observe(_ item: AVPlayerItem) {
        statusObserver = item.observe(\.status, options: [.initial, .new]) { [weak self] item, _ in
            Task { @MainActor in
                guard let self else { return }
                switch item.status {
                case .readyToPlay:
                    self.status = "Playing"
                case .failed:
                    self.status = "Playback failed: \(item.error?.localizedDescription ?? "unknown error")"
                case .unknown:
                    break
                @unknown default:
                    self.status = "Unknown player state"
                }
            }
        }
        timeObserver = player.addPeriodicTimeObserver(
            forInterval: CMTime(seconds: 1, preferredTimescale: 1),
            queue: .main
        ) { [weak self] time in
            Task { @MainActor in
                guard let self else { return }
                let seconds = max(0, Int(time.seconds.isFinite ? time.seconds : 0))
                self.elapsed = String(format: "%02d:%02d", seconds / 60, seconds % 60)
                self.refreshCacheSize()
            }
        }
    }

    private func refreshCacheSize() {
        let keys: Set<URLResourceKey> = [.isRegularFileKey, .fileSizeKey]
        let files = FileManager.default.enumerator(
            at: cacheDirectory,
            includingPropertiesForKeys: Array(keys)
        )
        var bytes = 0
        while let file = files?.nextObject() as? URL {
            let values = try? file.resourceValues(forKeys: keys)
            if values?.isRegularFile == true { bytes += values?.fileSize ?? 0 }
        }
        cacheSize = ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }
}
