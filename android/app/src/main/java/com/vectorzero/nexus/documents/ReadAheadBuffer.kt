package com.vectorzero.nexus.documents

/**
 * Turns many small random reads into few large network reads.
 *
 * Android asks a proxy file descriptor for small chunks (often 4 to 128 KiB). A
 * `ReadFile` RPC per chunk would make a 100 MB video cost thousands of round
 * trips. This keeps ONE aligned block ([blockSize], default 256 KiB) in memory:
 * a read inside it is a memory copy; a read outside it fetches the aligned
 * block containing the requested offset. Sequential playback therefore costs
 * about `fileSize / blockSize` RPCs, and seeking costs one RPC. Memory is O(blockSize)
 * per open file.
 *
 * [fetch] must return up to `length` bytes starting at `offset` (fewer only at
 * end of file or on a short read) and may throw.
 */
class ReadAheadBuffer(
    private val fileSize: Long,
    private val blockSize: Int = 256 * 1024,
    private val fetch: (offset: Long, length: Int) -> ByteArray,
) {
    private var start = -1L
    private var data = ByteArray(0)

    /** Copies up to [size] bytes at [offset] into [dest]; returns the count (0 at/after EOF). */
    @Synchronized
    fun read(offset: Long, size: Int, dest: ByteArray): Int {
        if (offset < 0 || offset >= fileSize || size <= 0) return 0
        val want = minOf(size.toLong(), fileSize - offset, dest.size.toLong()).toInt()
        var copied = 0
        var pos = offset
        while (copied < want) {
            if (!covers(pos)) {
                fill(pos)
                if (!covers(pos)) break // EOF or short read that does not reach `pos`
            }
            val within = (pos - start).toInt()
            val n = minOf(want - copied, data.size - within)
            System.arraycopy(data, within, dest, copied, n)
            copied += n
            pos += n
        }
        return copied
    }

    private fun covers(pos: Long) = start >= 0 && pos >= start && pos < start + data.size

    private fun fill(pos: Long) {
        val blockStart = pos / blockSize * blockSize
        val length = minOf(blockSize.toLong(), fileSize - blockStart).toInt()
        val bytes = fetch(blockStart, length)
        if (bytes.isNotEmpty()) {
            start = blockStart
            data = bytes
        }
    }
}
