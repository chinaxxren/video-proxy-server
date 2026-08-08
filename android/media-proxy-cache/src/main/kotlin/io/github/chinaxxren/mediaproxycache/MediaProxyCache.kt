package io.github.chinaxxren.mediaproxycache

import java.io.File
import java.net.URI

data class MediaProxyCacheConfiguration(
    val cacheDirectory: File,
    val allowedHosts: List<String>,
    val port: Int = 0,
) {
    internal fun validated(): MediaProxyCacheConfiguration {
        require(port in 0..65535) { "port must be between 0 and 65535" }
        require(cacheDirectory.path.isNotBlank()) { "cacheDirectory must not be blank" }
        require('\u0000' !in cacheDirectory.path) { "cacheDirectory must not contain NUL" }
        require(allowedHosts.isNotEmpty()) { "allowedHosts must not be empty" }
        require(allowedHosts.none { it.isBlank() || ',' in it || '\u0000' in it }) {
            "allowedHosts must contain non-blank hosts without commas or NUL"
        }
        return this
    }

    internal fun cacheDirectoryUtf8(): ByteArray = cacheDirectory.absolutePath.toByteArray(Charsets.UTF_8)

    internal fun allowedHostsUtf8(): ByteArray = allowedHosts.joinToString(",").toByteArray(Charsets.UTF_8)
}

class MediaProxyCache private constructor(private var nativeHandle: Long) : AutoCloseable {
    private enum class State { NEW, RUNNING, STOPPED, CLOSED }

    private var state = State.NEW

    companion object {
        init {
            System.loadLibrary("proxy_server")
            System.loadLibrary("media_proxy_cache_jni")
        }

        @JvmStatic
        fun create(configuration: MediaProxyCacheConfiguration): MediaProxyCache {
            val valid = configuration.validated()
            val handle = nativeCreate(
                valid.port,
                valid.cacheDirectoryUtf8(),
                valid.allowedHostsUtf8(),
            )
            check(handle != 0L) { "native MediaProxyCache creation failed" }
            return MediaProxyCache(handle)
        }

        @JvmStatic private external fun nativeCreate(
            port: Int,
            cacheDirectory: ByteArray,
            allowedHosts: ByteArray,
        ): Long

        @JvmStatic private external fun nativeStart(handle: Long): Int
        @JvmStatic private external fun nativeStop(handle: Long)
        @JvmStatic private external fun nativeDestroy(handle: Long)
    }

    @Synchronized
    fun start(): URI {
        check(state == State.NEW) { "MediaProxyCache can only be started once" }
        val port = nativeStart(nativeHandle)
        if (port == 0) {
            state = State.STOPPED
            error("proxy server failed to start")
        }
        state = State.RUNNING
        return URI("http://127.0.0.1:$port")
    }

    @Synchronized
    fun stop() {
        if (state == State.CLOSED || state == State.STOPPED) return
        nativeStop(nativeHandle)
        state = State.STOPPED
    }

    @Synchronized
    override fun close() {
        if (state == State.CLOSED) return
        nativeDestroy(nativeHandle)
        nativeHandle = 0
        state = State.CLOSED
    }
}
