package com.vectorzero.nexus

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class HostBeaconTest {
    private val id = "9c3e58d6-7272-4a01-b4a9-eb950c44642a"
    private fun bytes(s: String) = s.toByteArray(Charsets.UTF_8)

    @Test fun parsesTheHostFormat() {
        // Same text the Rust host produces in `beacon_roundtrips`.
        val b = HostBeacon.parse(bytes("NEXUS1\t$id\tFrank's Dell\t50051\t50053"))!!
        assertEquals(id, b.deviceId)
        assertEquals("Frank's Dell", b.name)
        assertEquals(50051, b.dataPort)
        assertEquals(50053, b.approvePort)
    }

    @Test fun respectsTheLengthOfTheReceiveBuffer() {
        val padded = bytes("NEXUS1\t$id\tDell\t1\t2") + ByteArray(50)
        assertEquals("Dell", HostBeacon.parse(padded, padded.size - 50)!!.name)
    }

    @Test fun rejectsMalformedBeacons() {
        for (bad in listOf(
            "", "NEXUS1?", "HELLO\t$id\tn\t1\t2", "NEXUS1\t$id\tn\t1", "NEXUS1\t$id\tn\tx\t2",
            "NEXUS1\t\tn\t1\t2", "NEXUS1\t$id\tn\t1\t2\textra", "NEXUS1\t$id\tn\t0\t2",
            "NEXUS1\t$id\tn\t1\t70000",
        )) {
            assertNull("'$bad'", HostBeacon.parse(bytes(bad)))
        }
    }

    @Test fun cleansHostileNames() {
        val long = HostBeacon.parse(bytes("NEXUS1\t$id\t${"x".repeat(100)}\t1\t2"))!!
        assertEquals(40, long.name.length)
        assertEquals("Nexus host", HostBeacon.parse(bytes("NEXUS1\t$id\t\u0007\u001b\t1\t2"))!!.name)
        assertEquals("ab", HostBeacon.parse(bytes("NEXUS1\t$id\ta\u0007b\t1\t2"))!!.name)
    }

    @Test fun theProbeIsExactlyWhatTheHostExpects() {
        assertEquals("NEXUS1?", String(HostBeacon.PROBE, Charsets.US_ASCII))
        assertEquals(50054, HostBeacon.PORT)
    }

    @Test fun directedBroadcastForCommonPrefixes() {
        fun bc(ip: String, prefix: Int) = HostBeacon.directedBroadcast(
            ip.split('.').map { it.toInt().toByte() }.toByteArray(), prefix
        ).joinToString(".") { (it.toInt() and 0xFF).toString() }
        assertEquals("172.16.255.255", bc("172.16.239.64", 16))
        assertEquals("192.168.1.255", bc("192.168.1.37", 24))
        assertEquals("10.255.255.255", bc("10.4.5.6", 8))
        assertEquals("192.168.1.63", bc("192.168.1.37", 26))
        assertEquals("1.2.3.4", bc("1.2.3.4", 32))
        assertEquals("255.255.255.255", bc("1.2.3.4", 0))
    }

    @Test fun directedBroadcastRejectsBadInput() {
        for ((addr, prefix) in listOf(ByteArray(3) to 24, ByteArray(4) to 33, ByteArray(4) to -1)) {
            try {
                HostBeacon.directedBroadcast(addr, prefix)
                throw AssertionError("expected rejection")
            } catch (_: IllegalArgumentException) {
            }
        }
        assertArrayEquals(byteArrayOf(10, 0, 0, 255.toByte()), HostBeacon.directedBroadcast(byteArrayOf(10, 0, 0, 1), 24))
    }
}
