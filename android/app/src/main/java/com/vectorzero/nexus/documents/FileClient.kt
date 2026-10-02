package com.vectorzero.nexus.documents

import com.vectorzero.nexus.GrpcClient
import com.vectorzero.nexus.PairedHost
import io.grpc.ManagedChannel
import io.grpc.Status
import io.grpc.StatusRuntimeException
import nexus.fs.v1.FileServiceGrpc
import nexus.fs.v1.FileServiceOuterClass.ListDirRequest
import nexus.fs.v1.FileServiceOuterClass.ReadFileRequest
import nexus.fs.v1.FileServiceOuterClass.StatRequest
import java.io.ByteArrayOutputStream
import java.io.FileNotFoundException
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit

/**
 * Blocking `FileService` client for one paired host, used by the documents
 * provider. Provider callbacks run on binder threads where blocking is fine; every
 * call carries a deadline so a dead host cannot hang the system file picker.
 *
 * It owns its own channel so it never disturbs the streaming channel in [GrpcClient].
 */
class FileClient(private val host: PairedHost) {

    private val channel: ManagedChannel =
        GrpcClient.newDataChannel(host.address, host.certPem, host.authToken)
    private val stub = FileServiceGrpc.newBlockingStub(channel)

    /** True if [other] still describes the same connection (address, cert, token). */
    fun sameConnection(other: PairedHost) =
        host.address == other.address && host.certPem == other.certPem && host.authToken == other.authToken

    fun close() {
        channel.shutdownNow()
    }

    fun list(path: String): List<RemoteEntry> = call("list $path") {
        stub.withDeadlineAfter(LIST_DEADLINE_S, TimeUnit.SECONDS)
            .listDir(ListDirRequest.newBuilder().setPath(path).build())
            .entriesList
            .map { RemoteEntry(it.name, it.isDir, it.sizeBytes, it.modifiedUnix) }
    }

    /** @return the entry, or null if the host reports it does not exist. */
    fun stat(path: String): RemoteEntry? = call("stat $path") {
        val resp = stub.withDeadlineAfter(STAT_DEADLINE_S, TimeUnit.SECONDS)
            .stat(StatRequest.newBuilder().setPath(path).build())
        if (!resp.found) null
        else RemoteEntry(
            // The host reports the leaf name; fall back to the path's last segment.
            resp.entry.name.ifEmpty { path.substringAfterLast('/') },
            resp.entry.isDir,
            resp.entry.sizeBytes,
            resp.entry.modifiedUnix,
        )
    }

    /** Reads up to [length] bytes at [offset] (fewer only at end of file). */
    fun read(path: String, offset: Long, length: Int): ByteArray = call("read $path@$offset") {
        val chunks = stub.withDeadlineAfter(READ_DEADLINE_S, TimeUnit.SECONDS)
            .readFile(
                ReadFileRequest.newBuilder()
                    .setPath(path).setOffset(offset).setLength(length.toLong()).build()
            )
        val out = ByteArrayOutputStream(length)
        while (chunks.hasNext() && out.size() < length) {
            val data = chunks.next().data
            val room = length - out.size()
            if (data.size() <= room) data.writeTo(out) else out.write(data.toByteArray(), 0, room)
        }
        out.toByteArray()
    }

    private inline fun <T> call(what: String, block: () -> T): T =
        try {
            block()
        } catch (e: StatusRuntimeException) {
            throw when (e.status.code) {
                Status.Code.NOT_FOUND -> FileNotFoundException("$what: not found")
                else -> IOException("$what failed: ${e.status.code}", e)
            }
        }

    companion object {
        private const val STAT_DEADLINE_S = 10L
        private const val LIST_DEADLINE_S = 15L
        private const val READ_DEADLINE_S = 30L
    }
}

/** One cached [FileClient] per paired host; rebuilt if the host is re-paired. */
object FileClients {
    private val clients = ConcurrentHashMap<String, FileClient>()

    fun forHost(host: PairedHost): FileClient =
        clients.compute(host.id) { _, existing ->
            if (existing != null && existing.sameConnection(host)) existing
            else {
                existing?.close()
                FileClient(host)
            }
        }!!
}
