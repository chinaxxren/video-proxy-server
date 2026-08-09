package com.example.mediaproxy

import java.io.File

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
    private external fun nativeRegisterP2PDirectory(
        handle: Long,
        manifestJson: String,
        pieceDirectory: String,
    ): Long
    private external fun nativeVerifyP2PSource(handle: Long, sourceId: Long): Boolean
    private external fun nativeRemoveP2PSource(handle: Long, sourceId: Long): Boolean
    private external fun nativeAddAuthorizedTorrent(handle: Long, magnetUri: String): Long
    private external fun nativeRemoveTorrent(
        handle: Long,
        torrentId: Long,
        deleteFiles: Boolean,
    ): Boolean
}
