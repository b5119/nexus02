package com.vectorzero.nexus

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class StreamLogicTest {
    @Test fun reconnectDelayDoublesThenCaps() {
        assertEquals(1_000L, ReconnectPolicy.delayMs(1))
        assertEquals(2_000L, ReconnectPolicy.delayMs(2))
        assertEquals(4_000L, ReconnectPolicy.delayMs(3))
        assertEquals(8_000L, ReconnectPolicy.delayMs(4))
        assertEquals(8_000L, ReconnectPolicy.delayMs(5))
        assertEquals(8_000L, ReconnectPolicy.delayMs(1_000), "huge attempt counts must not overflow")
    }

    private fun assertEquals(expected: Long, actual: Long, message: String) =
        org.junit.Assert.assertEquals(message, expected, actual)

    @Test(expected = IllegalArgumentException::class)
    fun reconnectAttemptsStartAtOne() {
        ReconnectPolicy.delayMs(0)
    }

    @Test fun gateRefusesDeltaFramesUntilAKeyframe() {
        val gate = KeyframeGate()
        assertTrue(gate.isWaiting)
        assertFalse("a decoder that just started cannot use a delta frame", gate.accept(false))
        assertFalse(gate.accept(false))
        assertTrue(gate.accept(true))
        assertFalse(gate.isWaiting)
        assertTrue("after the keyframe, deltas flow", gate.accept(false))
        assertTrue(gate.accept(false))
    }

    @Test fun gateWaitsAgainAfterALoss() {
        val gate = KeyframeGate()
        gate.accept(true)
        assertTrue(gate.accept(false))
        gate.reset() // a frame was dropped
        assertFalse(gate.accept(false))
        assertTrue(gate.accept(true))
    }

    @Test fun rateMeterComputesPerSecondRates() {
        var t = 10_000L
        val meter = RateMeter { t }
        assertEquals(0 to 0, meter.sample(0, 0))
        t += 1_000
        // 30 frames and 500,000 bytes in one second = 30 fps, 4,000 kbit/s.
        assertEquals(30 to 4_000, meter.sample(30, 500_000))
        t += 2_000
        // 60 more frames and 250,000 bytes over two seconds = 30 fps, 1,000 kbit/s.
        assertEquals(30 to 1_000, meter.sample(90, 750_000))
    }

    @Test fun rateMeterIgnoresAClockThatDidNotAdvance() {
        val meter = RateMeter { 5_000L }
        meter.sample(0, 0)
        assertEquals(0 to 0, meter.sample(100, 100_000))
    }
}
