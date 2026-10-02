package com.vectorzero.nexus.documents

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class MetaCacheTest {
    private fun entry(name: String) = RemoteEntry(name, false, 1, 2)

    @Test fun returnsWhatWasStored() {
        val c = MetaCache()
        c.put("k", entry("a"))
        assertEquals(entry("a"), c.get("k"))
        assertNull(c.get("missing"))
    }

    @Test fun entriesExpireAfterTtl() {
        var t = 0L
        val c = MetaCache(ttlMs = 100, now = { t })
        c.put("k", entry("a"))
        t = 100
        assertNotNull("exactly at the TTL is still fresh", c.get("k"))
        t = 101
        assertNull(c.get("k"))
        assertEquals("expired entries are dropped", 0, c.size())
    }

    @Test fun evictsLeastRecentlyUsedWhenFull() {
        val c = MetaCache(maxEntries = 2)
        c.put("a", entry("a"))
        c.put("b", entry("b"))
        c.get("a") // "b" is now the least recently used
        c.put("c", entry("c"))
        assertEquals(2, c.size())
        assertNotNull(c.get("a"))
        assertNull(c.get("b"))
        assertNotNull(c.get("c"))
    }

    @Test fun putRefreshesTimestamp() {
        var t = 0L
        val c = MetaCache(ttlMs = 100, now = { t })
        c.put("k", entry("a"))
        t = 90
        c.put("k", entry("b"))
        t = 150
        assertEquals(entry("b"), c.get("k"))
    }
}
