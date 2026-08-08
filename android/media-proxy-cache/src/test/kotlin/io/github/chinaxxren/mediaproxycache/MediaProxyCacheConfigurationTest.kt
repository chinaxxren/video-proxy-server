package io.github.chinaxxren.mediaproxycache

import java.io.File
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith

class MediaProxyCacheConfigurationTest {
    @Test
    fun acceptsDynamicAndMaximumPorts() {
        assertEquals(0, configuration(port = 0).validated().port)
        assertEquals(65535, configuration(port = 65535).validated().port)
    }

    @Test
    fun rejectsPortsOutsideUnsignedShortRange() {
        assertFailsWith<IllegalArgumentException> { configuration(port = -1).validated() }
        assertFailsWith<IllegalArgumentException> { configuration(port = 65536).validated() }
    }

    @Test
    fun rejectsMissingOrAmbiguousHosts() {
        assertFailsWith<IllegalArgumentException> { configuration(hosts = emptyList()).validated() }
        assertFailsWith<IllegalArgumentException> { configuration(hosts = listOf(" ")).validated() }
        assertFailsWith<IllegalArgumentException> {
            configuration(hosts = listOf("one.example,two.example")).validated()
        }
        assertFailsWith<IllegalArgumentException> {
            configuration(hosts = listOf("media.example.com\u0000.invalid")).validated()
        }
    }

    @Test
    fun encodesUnicodePathsAsStandardUtf8() {
        val configuration = MediaProxyCacheConfiguration(
            File("缓存-🎵"),
            listOf("media.example.com"),
        ).validated()
        assertEquals(
            configuration.cacheDirectory.absolutePath,
            configuration.cacheDirectoryUtf8().toString(Charsets.UTF_8),
        )
    }

    private fun configuration(
        port: Int = 0,
        hosts: List<String> = listOf("media.example.com"),
    ) = MediaProxyCacheConfiguration(File("cache"), hosts, port)
}
