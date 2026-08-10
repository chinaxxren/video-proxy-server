package com.example.mediaproxy

import java.io.File
import org.json.JSONArray
import org.json.JSONObject

data class TorrentFile(val fileId: Long, val relativePath: String, val length: Long)
data class TorrentStatus(
    val state: String,
    val totalBytes: Long,
    val downloadedBytes: Long,
    val uploadedBytes: Long,
    val finished: Boolean,
    val error: String?,
)

fun interface SourceRefreshProvider {
    fun refreshSource(sourceId: Long): String?
}

/** Thin Kotlin ownership wrapper around the shared Rust JNI bridge. */
class MediaProxyCache private constructor(private var handle: Long) : AutoCloseable {
    private var boundPort: Int = 0
    companion object {
        init { System.loadLibrary("proxy_server") }

        @JvmStatic
        fun create(port: Int, cacheDirectory: String, allowedHosts: List<String>): MediaProxyCache {
            require(port in 0..65535)
            require(cacheDirectory.isNotBlank())
            require(allowedHosts.isNotEmpty())
            require(allowedHosts.none { it.isBlank() || ',' in it })
            val handle = nativeCreate(port, cacheDirectory, allowedHosts.joinToString(","))
            check(handle != 0L) { "proxy server creation failed" }
            return MediaProxyCache(handle)
        }

        @JvmStatic private external fun nativeCreate(port: Int, cacheDirectory: String, allowedHosts: String): Long
    }

    @Synchronized fun start(): Int {
        check(handle != 0L) { "MediaProxyCache is closed" }
        return nativeStart(handle).also {
            check(it != 0) { "proxy server failed to start" }
            boundPort = it
        }
    }

    @Synchronized fun stop() {
        if (handle != 0L) nativeStop(handle)
        boundPort = 0
    }

    @Synchronized fun registerSource(identity: String, url: String): Long {
        check(handle != 0L && boundPort != 0) { "MediaProxyCache is not running" }
        require(identity.isNotBlank() && (url.startsWith("https://") || url.startsWith("http://")))
        return nativeRegisterSource(handle, identity, url).also { check(it > 0L) { "source registration failed" } }
    }

    @Synchronized fun refreshSource(sourceId: Long, url: String): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(sourceId > 0L && (url.startsWith("https://") || url.startsWith("http://")))
        return nativeRefreshSource(handle, sourceId, url)
    }

    @Synchronized fun removeSource(sourceId: Long): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(sourceId > 0L)
        return nativeRemoveSource(handle, sourceId)
    }

    @Synchronized fun playbackUrl(sourceId: Long): String {
        require(sourceId > 0L)
        check(boundPort != 0) { "MediaProxyCache is not running" }
        return "http://127.0.0.1:$boundPort/media/$sourceId"
    }

    @Synchronized fun setSourceRefreshProvider(provider: SourceRefreshProvider): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        return nativeSetSourceRefreshProvider(handle, provider)
    }

    /** Requires a native artifact built with P2P_ENABLED=1. */
    @Synchronized
    fun registerP2PDirectory(manifestJson: String, pieceDirectory: String): Long {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(manifestJson.isNotBlank())
        require(pieceDirectory.isNotBlank() && File(pieceDirectory).isAbsolute)
        return nativeRegisterP2PDirectory(handle, manifestJson, pieceDirectory).also {
            check(it > 0L) { "P2P source registration failed" }
        }
    }

    /** Performs piece and complete-content SHA-256 verification. */
    @Synchronized
    fun verifyP2PSource(sourceId: Long): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(sourceId > 0L)
        return nativeVerifyP2PSource(handle, sourceId)
    }

    /** Revokes the source and waits for active Provider reads to finish. */
    @Synchronized
    fun removeP2PSource(sourceId: Long): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(sourceId > 0L)
        return nativeRemoveP2PSource(handle, sourceId)
    }

    @Synchronized
    fun p2pPlaybackUrl(sourceId: Long): String {
        require(sourceId > 0L)
        check(boundPort != 0) { "MediaProxyCache is not running" }
        return "http://127.0.0.1:$boundPort/p2p/$sourceId"
    }

    /** Requires a native artifact built with the p2p-librqbit feature. */
    @Synchronized
    fun addAuthorizedTorrent(magnetUri: String): Long {
        check(handle != 0L) { "MediaProxyCache is closed" }
        check(boundPort != 0) { "MediaProxyCache is not running" }
        require(magnetUri.startsWith("magnet:?"))
        return nativeAddAuthorizedTorrent(handle, magnetUri).also {
            check(it >= 0L) { "torrent registration failed" }
        }
    }

    @Synchronized
    fun addAuthorizedTorrentFile(torrentBytes: ByteArray): Long {
        check(handle != 0L) { "MediaProxyCache is closed" }
        check(boundPort != 0) { "MediaProxyCache is not running" }
        require(torrentBytes.isNotEmpty() && torrentBytes.size <= 4 * 1024 * 1024)
        return nativeAddAuthorizedTorrentFile(handle, torrentBytes).also {
            check(it >= 0L) { "torrent file registration failed" }
        }
    }

    @Synchronized
    fun removeTorrent(torrentId: Long, deleteFiles: Boolean = false): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(torrentId >= 0L)
        return nativeRemoveTorrent(handle, torrentId, deleteFiles)
    }

    @Synchronized
    fun torrentPlaybackUrl(torrentId: Long, fileId: Long): String {
        require(torrentId >= 0L && fileId >= 0L)
        check(boundPort != 0) { "MediaProxyCache is not running" }
        return "http://127.0.0.1:$boundPort/torrent/$torrentId/$fileId"
    }

    @Synchronized
    fun torrentFiles(torrentId: Long): List<TorrentFile> {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(torrentId >= 0L)
        val values = JSONArray(checkNotNull(nativeTorrentFilesJson(handle, torrentId)) { "torrent files unavailable" })
        return List(values.length()) { index ->
            val value = values.getJSONObject(index)
            TorrentFile(value.getLong("file_id"), value.getString("relative_path"), value.getLong("length"))
        }
    }

    @Synchronized
    fun selectTorrentFiles(torrentId: Long, fileIds: LongArray): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(torrentId >= 0L && fileIds.isNotEmpty() && fileIds.size <= 4096)
        require(fileIds.all { it in 0..UInt.MAX_VALUE.toLong() })
        return nativeSelectTorrentFiles(handle, torrentId, fileIds.map { it.toInt() }.toIntArray())
    }

    @Synchronized
    fun torrentStatus(torrentId: Long): TorrentStatus {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(torrentId >= 0L)
        val value = JSONObject(checkNotNull(nativeTorrentStatusJson(handle, torrentId)) { "torrent status unavailable" })
        return TorrentStatus(
            value.getString("state"),
            value.getLong("total_bytes"),
            value.getLong("downloaded_bytes"),
            value.getLong("uploaded_bytes"),
            value.getBoolean("finished"),
            value.optString("error").takeUnless { value.isNull("error") },
        )
    }

    @Synchronized fun pauseTorrent(torrentId: Long): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(torrentId >= 0L)
        return nativeSetTorrentPaused(handle, torrentId, true)
    }

    @Synchronized fun resumeTorrent(torrentId: Long): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(torrentId >= 0L)
        return nativeSetTorrentPaused(handle, torrentId, false)
    }

    /** Sets session-wide bytes/second; zero removes the limit. */
    @Synchronized fun setTorrentDownloadLimit(bytesPerSecond: Long): Boolean {
        check(handle != 0L) { "MediaProxyCache is closed" }
        require(bytesPerSecond in 0..UInt.MAX_VALUE.toLong())
        return nativeSetTorrentDownloadLimit(handle, bytesPerSecond)
    }

    @Synchronized override fun close() {
        if (handle != 0L) {
            nativeDestroy(handle)
            handle = 0L
            boundPort = 0
        }
    }

    private external fun nativeStart(handle: Long): Int
    private external fun nativeStop(handle: Long)
    private external fun nativeDestroy(handle: Long)
    private external fun nativeRegisterSource(handle: Long, identity: String, url: String): Long
    private external fun nativeRefreshSource(handle: Long, sourceId: Long, url: String): Boolean
    private external fun nativeRemoveSource(handle: Long, sourceId: Long): Boolean
    private external fun nativeSetSourceRefreshProvider(handle: Long, provider: SourceRefreshProvider): Boolean
    private external fun nativeRegisterP2PDirectory(
        handle: Long,
        manifestJson: String,
        pieceDirectory: String,
    ): Long
    private external fun nativeVerifyP2PSource(handle: Long, sourceId: Long): Boolean
    private external fun nativeRemoveP2PSource(handle: Long, sourceId: Long): Boolean
    private external fun nativeAddAuthorizedTorrent(handle: Long, magnetUri: String): Long
    private external fun nativeAddAuthorizedTorrentFile(handle: Long, torrentBytes: ByteArray): Long
    private external fun nativeRemoveTorrent(
        handle: Long,
        torrentId: Long,
        deleteFiles: Boolean,
    ): Boolean
    private external fun nativeTorrentFilesJson(handle: Long, torrentId: Long): String?
    private external fun nativeTorrentStatusJson(handle: Long, torrentId: Long): String?
    private external fun nativeSelectTorrentFiles(handle: Long, torrentId: Long, fileIds: IntArray): Boolean
    private external fun nativeSetTorrentPaused(handle: Long, torrentId: Long, paused: Boolean): Boolean
    private external fun nativeSetTorrentDownloadLimit(handle: Long, bytesPerSecond: Long): Boolean
}
