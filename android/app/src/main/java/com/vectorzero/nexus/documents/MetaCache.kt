package com.vectorzero.nexus.documents

/** What the host told us about one file or directory (a subset of `FileEntry`). */
data class RemoteEntry(
    val name: String,
    val isDir: Boolean,
    val sizeBytes: Long,
    val modifiedUnix: Long,
)

/**
 * Small LRU cache with a time-to-live for file metadata.
 *
 * The system file picker calls `queryDocument` once per visible row right after
 * `queryChildDocuments`; without this each of those would be a network `Stat`.
 * Listing a directory fills the cache for all its children, so a screen of n
 * entries costs one `ListDir` RPC instead of 1 + n.
 *
 * `get` and `put` are O(1) (access-ordered `LinkedHashMap`); memory is bounded
 * by [maxEntries]; an entry older than [ttlMs] is treated as absent so changes
 * on the host show up within the TTL.
 */
class MetaCache(
    private val maxEntries: Int = 1024,
    private val ttlMs: Long = 10_000,
    private val now: () -> Long = System::currentTimeMillis,
) {
    private class Slot(val entry: RemoteEntry, val storedAt: Long)

    private val map = object : LinkedHashMap<String, Slot>(16, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, Slot>): Boolean =
            size > maxEntries
    }

    @Synchronized
    fun get(key: String): RemoteEntry? {
        val slot = map[key] ?: return null
        if (now() - slot.storedAt > ttlMs) {
            map.remove(key)
            return null
        }
        return slot.entry
    }

    @Synchronized
    fun put(key: String, entry: RemoteEntry) {
        map[key] = Slot(entry, now())
    }

    @Synchronized
    fun remove(key: String) {
        map.remove(key)
    }

    @Synchronized
    fun clear() = map.clear()

    @Synchronized
    fun size(): Int = map.size
}
