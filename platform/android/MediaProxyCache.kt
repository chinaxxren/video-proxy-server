package com.example.mediaproxy

/** Thin Kotlin ownership wrapper around the shared Rust C ABI. */
class MediaProxyCache private constructor(private var handle: Long) : AutoCloseable {
    companion object {
        init { System.loadLibrary("proxy_server") }

        @JvmStatic
        fun create(port: Int, cacheDirectory: String, allowedHosts: List<String>): MediaProxyCache {
            require(port in 0..65535)
            require(cacheDirectory.isNotBlank())
            require(allowedHosts.isNotEmpty())
            require(allowedHosts.none { it.isBlank() || ',' in it })
            return MediaProxyCache(nativeCreate(port, cacheDirectory, allowedHosts.joinToString(",")))
        }

        @JvmStatic private external fun nativeCreate(port: Int, cacheDirectory: String, allowedHosts: String): Long
    }

    @Synchronized fun start(): Int {
        check(handle != 0L) { "MediaProxyCache is closed" }
        return nativeStart(handle).also { check(it != 0) { "proxy server failed to start" } }
    }

    @Synchronized fun stop() { if (handle != 0L) nativeStop(handle) }

    @Synchronized override fun close() {
        if (handle != 0L) {
            nativeDestroy(handle)
            handle = 0L
        }
    }

    private external fun nativeStart(handle: Long): Int
    private external fun nativeStop(handle: Long)
    private external fun nativeDestroy(handle: Long)
}
