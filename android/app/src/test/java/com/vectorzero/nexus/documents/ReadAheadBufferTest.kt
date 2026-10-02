package com.vectorzero.nexus.documents

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test

class ReadAheadBufferTest {
    private val file = ByteArray(1000) { (it % 251).toByte() }

    private class Counting(val file: ByteArray) {
        var calls = 0
        val fetch: (Long, Int) -> ByteArray = { offset, length ->
            calls++
            val from = offset.toInt()
            file.copyOfRange(from, minOf(file.size, from + length))
        }
    }

    @Test fun sequentialSmallReadsShareOneFetchPerBlock() {
        val src = Counting(file)
        val buf = ReadAheadBuffer(file.size.toLong(), blockSize = 256, fetch = src.fetch)
        val out = ByteArray(file.size)
        var pos = 0
        val chunk = ByteArray(40)
        while (pos < file.size) {
            val n = buf.read(pos.toLong(), chunk.size, chunk)
            System.arraycopy(chunk, 0, out, pos, n)
            pos += n
        }
        assertArrayEquals(file, out)
        assertEquals("1000 bytes / 256-byte blocks = 4 fetches, not 25", 4, src.calls)
    }

    @Test fun readSpanningTwoBlocksIsAssembledCorrectly() {
        val src = Counting(file)
        val buf = ReadAheadBuffer(file.size.toLong(), blockSize = 256, fetch = src.fetch)
        val out = ByteArray(100)
        assertEquals(100, buf.read(230, 100, out))
        assertArrayEquals(file.copyOfRange(230, 330), out)
        assertEquals(2, src.calls)
    }

    @Test fun seekingBackRefetchesOnlyWhenOutsideTheBlock() {
        val src = Counting(file)
        val buf = ReadAheadBuffer(file.size.toLong(), blockSize = 256, fetch = src.fetch)
        val out = ByteArray(10)
        buf.read(600, 10, out) // block 512..767
        buf.read(700, 10, out) // same block
        assertEquals(1, src.calls)
        buf.read(0, 10, out) // different block
        assertEquals(2, src.calls)
        assertArrayEquals(file.copyOfRange(0, 10), out)
    }

    @Test fun readsAreClampedAtEndOfFile() {
        val buf = ReadAheadBuffer(file.size.toLong(), blockSize = 256, fetch = Counting(file).fetch)
        val out = ByteArray(100)
        assertEquals(10, buf.read(990, 100, out))
        assertArrayEquals(file.copyOfRange(990, 1000), out.copyOf(10))
        assertEquals(0, buf.read(1000, 10, out))
        assertEquals(0, buf.read(5000, 10, out))
        assertEquals(0, buf.read(-1, 10, out))
        assertEquals(0, buf.read(0, 0, out))
    }

    @Test fun shortFetchDoesNotLoopForever() {
        val buf = ReadAheadBuffer(1000, blockSize = 256) { _, _ -> ByteArray(0) }
        assertEquals(0, buf.read(0, 10, ByteArray(10)))
    }

    @Test fun emptyFileReadsNothing() {
        val buf = ReadAheadBuffer(0, fetch = { _, _ -> ByteArray(0) })
        assertEquals(0, buf.read(0, 10, ByteArray(10)))
    }
}
