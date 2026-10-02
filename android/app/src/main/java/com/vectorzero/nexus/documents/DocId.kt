package com.vectorzero.nexus.documents

/**
 * A SAF document id: `<hostId>:<absolute path on that host>`, for example
 * `9f2c:/photos/a.jpg`. The root of a host is `<hostId>:/`.
 *
 * Paths are normalised on parse (empty segments collapsed) and `.` / `..`
 * segments are rejected, so a document id can never ask the host to leave the
 * served directory. The host enforces this too (see `FileServiceImpl::resolve`),
 * this is the first line of defence.
 */
data class DocId(val hostId: String, val path: String) {

    val isRoot: Boolean get() = path == "/"

    val encoded: String get() = "$hostId:$path"

    /** Display name of the last path segment ("" for a root). */
    val name: String get() = if (isRoot) "" else path.substringAfterLast('/')

    fun child(name: String): DocId {
        require(name.isNotEmpty() && '/' !in name && name != "." && name != "..") {
            "invalid child name: $name"
        }
        return DocId(hostId, if (isRoot) "/$name" else "$path/$name")
    }

    /** True if this id is strictly below [ancestor] on the same host. O(path length). */
    fun isDescendantOf(ancestor: DocId): Boolean {
        if (hostId != ancestor.hostId || path == ancestor.path) return false
        return ancestor.isRoot || path.startsWith(ancestor.path + "/")
    }

    companion object {
        /** @throws IllegalArgumentException if [id] is malformed or contains `.`/`..`. */
        fun parse(id: String): DocId {
            val sep = id.indexOf(':')
            require(sep > 0 && sep < id.length - 1 && id[sep + 1] == '/') {
                "malformed document id: $id"
            }
            val segments = id.substring(sep + 1).split('/').filter { it.isNotEmpty() }
            require(segments.none { it == "." || it == ".." }) {
                "path traversal in document id: $id"
            }
            val path = if (segments.isEmpty()) "/" else segments.joinToString("/", prefix = "/")
            return DocId(id.substring(0, sep), path)
        }

        fun root(hostId: String): DocId = DocId(hostId, "/")
    }
}
