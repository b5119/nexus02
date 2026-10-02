package com.vectorzero.nexus.documents

import android.database.Cursor
import android.database.MatrixCursor
import android.os.CancellationSignal
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.os.ProxyFileDescriptorCallback
import android.os.storage.StorageManager
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.system.ErrnoException
import android.system.OsConstants
import android.util.Log
import android.webkit.MimeTypeMap
import com.vectorzero.nexus.HostStore
import com.vectorzero.nexus.PairedHost
import java.io.FileNotFoundException

/**
 * Shows every paired Nexus host as a root in the system file picker (ADR 0017,
 * feature A). Read-only for now.
 *
 * | Android callback          | Nexus RPC                                              |
 * |---------------------------|--------------------------------------------------------|
 * | queryRoots                | one root per host in [HostStore]                       |
 * | queryChildDocuments       | ListDir                                                |
 * | queryDocument             | Stat (usually served from [MetaCache])                 |
 * | openDocument (read)       | ReadFile ranges, via a proxy file descriptor           |
 *
 * Document ids are `<hostId>:<path>` ([DocId]). Reads use
 * `StorageManager.openProxyFileDescriptor`, which gives callers real random
 * access (seeking in a video does not download the whole file), backed by a
 * [ReadAheadBuffer] so sequential reads cost one RPC per 256 KiB.
 */
class NexusDocumentsProvider : DocumentsProvider() {

    private val cache = MetaCache()
    private lateinit var ioThread: HandlerThread
    private lateinit var ioHandler: Handler

    override fun onCreate(): Boolean {
        ioThread = HandlerThread("nexus-docs-io").also { it.start() }
        ioHandler = Handler(ioThread.looper)
        return true
    }

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val ctx = requireNotNull(context)
        val cursor = MatrixCursor(projection ?: DEFAULT_ROOT_PROJECTION)
        for (host in HostStore.loadHosts(ctx)) {
            cursor.newRow()
                .add(Root.COLUMN_ROOT_ID, host.id)
                .add(Root.COLUMN_DOCUMENT_ID, DocId.root(host.id).encoded)
                .add(Root.COLUMN_TITLE, host.name)
                .add(Root.COLUMN_SUMMARY, host.address)
                .add(Root.COLUMN_ICON, ctx.applicationInfo.icon)
                .add(Root.COLUMN_FLAGS, Root.FLAG_SUPPORTS_IS_CHILD)
                .add(Root.COLUMN_MIME_TYPES, "*/*")
        }
        return cursor
    }

    override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor {
        val id = parse(documentId)
        val host = hostFor(id)
        val cursor = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        if (id.isRoot) {
            addRow(cursor, id, RemoteEntry(host.name, true, 0, 0))
        } else {
            addRow(cursor, id, lookup(host, id))
        }
        return cursor
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val parent = parse(parentDocumentId)
        val host = hostFor(parent)
        val entries = try {
            FileClients.forHost(host).list(parent.path)
        } catch (e: java.io.IOException) {
            Log.w(TAG, "list failed for $parentDocumentId", e)
            throw FileNotFoundException("cannot list ${parent.path}: ${e.message}")
        }
        val cursor = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        for (entry in entries) {
            val child = parent.child(entry.name)
            cache.put(child.encoded, entry) // the picker will queryDocument each row next
            addRow(cursor, child, entry)
        }
        return cursor
    }

    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean =
        try {
            parse(documentId).isDescendantOf(parse(parentDocumentId))
        } catch (e: IllegalArgumentException) {
            false
        }

    override fun openDocument(
        documentId: String,
        mode: String,
        signal: CancellationSignal?,
    ): ParcelFileDescriptor {
        // Writes arrive in a later phase (ADR 0017); refuse them explicitly.
        if (mode != "r") throw UnsupportedOperationException("read-only provider (mode=$mode)")
        val id = parse(documentId)
        val host = hostFor(id)
        if (id.isRoot) throw FileNotFoundException("$documentId is a directory")
        val entry = lookup(host, id)
        if (entry.isDir) throw FileNotFoundException("$documentId is a directory")

        val client = FileClients.forHost(host)
        val buffer = ReadAheadBuffer(entry.sizeBytes) { offset, length ->
            client.read(id.path, offset, length)
        }
        val callback = object : ProxyFileDescriptorCallback() {
            override fun onGetSize(): Long = entry.sizeBytes

            override fun onRead(offset: Long, size: Int, data: ByteArray): Int =
                try {
                    buffer.read(offset, size, data)
                } catch (e: Exception) {
                    Log.w(TAG, "read failed for $documentId@$offset", e)
                    throw ErrnoException("onRead", OsConstants.EIO)
                }

            override fun onRelease() {}
        }
        val storage = requireNotNull(context).getSystemService(StorageManager::class.java)
        return storage.openProxyFileDescriptor(ParcelFileDescriptor.MODE_READ_ONLY, callback, ioHandler)
    }

    override fun shutdown() {
        ioThread.quitSafely()
    }

    // ---- helpers ----------------------------------------------------------

    private fun parse(documentId: String): DocId =
        try {
            DocId.parse(documentId)
        } catch (e: IllegalArgumentException) {
            throw FileNotFoundException(e.message)
        }

    private fun hostFor(id: DocId): PairedHost =
        HostStore.loadHosts(requireNotNull(context)).firstOrNull { it.id == id.hostId }
            ?: throw FileNotFoundException("host ${id.hostId} is not paired")

    private fun lookup(host: PairedHost, id: DocId): RemoteEntry {
        cache.get(id.encoded)?.let { return it }
        val entry = try {
            FileClients.forHost(host).stat(id.path)
        } catch (e: java.io.IOException) {
            throw FileNotFoundException("cannot stat ${id.path}: ${e.message}")
        } ?: throw FileNotFoundException("${id.path} not found")
        cache.put(id.encoded, entry)
        return entry
    }

    private fun addRow(cursor: MatrixCursor, id: DocId, entry: RemoteEntry) {
        cursor.newRow()
            .add(Document.COLUMN_DOCUMENT_ID, id.encoded)
            .add(Document.COLUMN_DISPLAY_NAME, entry.name)
            .add(Document.COLUMN_SIZE, if (entry.isDir) null else entry.sizeBytes)
            .add(Document.COLUMN_MIME_TYPE, if (entry.isDir) Document.MIME_TYPE_DIR else mimeOf(entry.name))
            .add(Document.COLUMN_LAST_MODIFIED, entry.modifiedUnix * 1000)
            .add(Document.COLUMN_FLAGS, 0) // read-only: no write/delete/rename flags yet
    }

    private fun mimeOf(name: String): String =
        MimeTypeMap.getSingleton()
            .getMimeTypeFromExtension(name.substringAfterLast('.', "").lowercase())
            ?: "application/octet-stream"

    companion object {
        private const val TAG = "NexusDocs"

        private val DEFAULT_ROOT_PROJECTION = arrayOf(
            Root.COLUMN_ROOT_ID, Root.COLUMN_DOCUMENT_ID, Root.COLUMN_TITLE, Root.COLUMN_SUMMARY,
            Root.COLUMN_ICON, Root.COLUMN_FLAGS, Root.COLUMN_MIME_TYPES,
        )
        private val DEFAULT_DOCUMENT_PROJECTION = arrayOf(
            Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_SIZE,
            Document.COLUMN_MIME_TYPE, Document.COLUMN_LAST_MODIFIED, Document.COLUMN_FLAGS,
        )
    }
}
