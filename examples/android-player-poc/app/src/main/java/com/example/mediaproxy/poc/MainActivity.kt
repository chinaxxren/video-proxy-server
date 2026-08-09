package com.example.mediaproxy.poc

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.Gravity
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import androidx.media3.common.MediaItem
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.ui.PlayerView
import com.example.mediaproxy.MediaProxyCache
import java.io.File

@androidx.annotation.OptIn(UnstableApi::class)
class MainActivity : Activity() {
    private lateinit var player: ExoPlayer
    private var proxy: MediaProxyCache? = null
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var statusView: TextView
    private lateinit var positionView: TextView
    private lateinit var cacheView: TextView

    private val updateMetrics = object : Runnable {
        override fun run() {
            if (::player.isInitialized) {
                val seconds = player.currentPosition.coerceAtLeast(0) / 1_000
                positionView.text = getString(R.string.position, seconds / 60, seconds % 60)
            }
            val bytes = cacheDir().walkTopDown().filter(File::isFile).sumOf(File::length)
            cacheView.text = getString(
                R.string.cache_size,
                android.text.format.Formatter.formatFileSize(this@MainActivity, bytes),
            )
            handler.postDelayed(this, 1_000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(buildContent())
        startPlayback(intent.getStringExtra("origin_url"))
        handler.post(updateMetrics)
    }

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        if (::player.isInitialized) player.release()
        proxy?.close()
        proxy = null
        super.onDestroy()
    }

    private fun startPlayback(originUrl: String?) {
        if (originUrl.isNullOrBlank()) {
            statusView.setText(R.string.status_missing_origin)
            return
        }
        val host = runCatching { java.net.URI(originUrl).host }.getOrNull()
        if (host.isNullOrBlank()) {
            statusView.setText(R.string.status_invalid_origin)
            return
        }
        runCatching {
            val proxy = MediaProxyCache.create(0, cacheDir().absolutePath, listOf(host))
            this.proxy = proxy
            val port = proxy.start()
            val headers = mapOf(
                "X-Original-Url" to originUrl,
                "X-Cache-User-Id" to "android-poc-user",
                "X-Cache-Asset-Id" to "aa-video",
                "X-Cache-Asset-Revision" to "1",
            )
            val httpFactory = DefaultHttpDataSource.Factory().setDefaultRequestProperties(headers)
            player = ExoPlayer.Builder(this).setMediaSourceFactory(
                androidx.media3.exoplayer.source.DefaultMediaSourceFactory(httpFactory)
            ).build()
            findViewById<PlayerView>(PLAYER_ID).player = player
            player.addListener(object : Player.Listener {
                override fun onPlaybackStateChanged(playbackState: Int) {
                    statusView.text = when (playbackState) {
                        Player.STATE_BUFFERING -> getString(R.string.status_buffering)
                        Player.STATE_READY -> getString(R.string.status_playing, port)
                        Player.STATE_ENDED -> getString(R.string.status_ended)
                        else -> getString(R.string.status_starting)
                    }
                }

                override fun onPlayerError(error: androidx.media3.common.PlaybackException) {
                    statusView.text = getString(R.string.status_failed, error.errorCodeName)
                }
            })
            player.setMediaItem(MediaItem.fromUri("http://127.0.0.1:$port/playback"))
            player.prepare()
            player.play()
        }.onFailure {
            statusView.text = getString(R.string.status_failed, it.javaClass.simpleName)
        }
    }

    private fun buildContent(): LinearLayout {
        val density = resources.displayMetrics.density
        fun dp(value: Int) = (value * density).toInt()
        fun label(text: String, tag: String) = TextView(this).apply {
            this.text = text
            textSize = 17f
            setTextColor(Color.rgb(30, 35, 32))
            this.tag = tag
            setPadding(0, dp(6), 0, dp(6))
        }

        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(dp(20), dp(20), dp(20), dp(20))
            addView(PlayerView(context).apply {
                id = PLAYER_ID
                contentDescription = getString(R.string.video_player)
            }, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(label(getString(R.string.title), "title").apply { textSize = 24f })
            statusView = label(getString(R.string.status_starting), "playback-status")
            positionView = label(getString(R.string.position, 0, 0), "playback-position")
            cacheView = label(getString(R.string.cache_size, "0 B"), "cache-size")
            addView(statusView)
            addView(positionView)
            addView(cacheView)
            addView(Button(context).apply {
                setText(R.string.seek_forward)
                contentDescription = text
                tag = "seek-forward"
                setOnClickListener {
                    if (::player.isInitialized) player.seekTo(player.currentPosition + 10_000)
                }
            })
        }
    }

    private fun cacheDir() = File(cacheDir, "MediaProxyCachePlayerPOC")

    private companion object {
        const val PLAYER_ID = 0x4d5043
    }
}
