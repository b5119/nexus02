package com.vectorzero.nexus

/** What the stream screen should show: connecting, live, trying again, or finished. */
enum class StreamPhase { CONNECTING, LIVE, RECONNECTING, ENDED }

data class StreamState(val phase: StreamPhase, val message: String)

/** Live numbers for the optional stats overlay. */
data class StreamStats(
    val fps: Int = 0,
    val kbps: Int = 0,
    val width: Int = 0,
    val height: Int = 0,
    val reconnects: Int = 0,
)

/**
 * Delay before reconnect attempt number `attempt` (1 = first retry): 1 s, 2 s, 4 s, then capped
 * at 8 s. Quick at first because most hiccups are brief; capped so a host that is down does
 * not get hammered and the user is never waiting minutes.
 */
object ReconnectPolicy {
    const val BASE_DELAY_MS = 1_000L
    const val MAX_DELAY_MS = 8_000L

    /** A connection that stayed live this long counts as healthy: the attempt counter resets. */
    const val HEALTHY_AFTER_MS = 5_000L

    fun delayMs(attempt: Int): Long {
        require(attempt >= 1) { "attempt starts at 1" }
        val shift = (attempt - 1).coerceAtMost(10)
        return (BASE_DELAY_MS shl shift).coerceAtMost(MAX_DELAY_MS)
    }
}

/**
 * Guards the decoder against frames it cannot decode.
 *
 * H.264 delta frames depend on earlier frames. A decoder that has just started, or that lost a
 * frame, shows blocky garbage if it is fed deltas, until the next keyframe arrives. So after
 * [reset] every non-keyframe is refused until a keyframe is accepted.
 */
class KeyframeGate {
    private var waiting = true

    /** @return true if [isKeyframe] may be decoded now. */
    fun accept(isKeyframe: Boolean): Boolean {
        if (waiting) {
            if (!isKeyframe) return false
            waiting = false
        }
        return true
    }

    /** Call when the decoder is (re)created or a frame was lost. */
    fun reset() {
        waiting = true
    }

    val isWaiting: Boolean get() = waiting
}

/**
 * Turns running counters into per-second rates: call [sample] about once a second with the
 * cumulative frame and byte counts. Takes the clock as a parameter so it can be tested.
 */
class RateMeter(private val nowMs: () -> Long = System::currentTimeMillis) {
    private var lastAt = -1L
    private var lastFrames = 0L
    private var lastBytes = 0L

    /** @return (frames per second, kilobits per second) since the previous sample, or (0, 0) on the first call. */
    fun sample(frames: Long, bytes: Long): Pair<Int, Int> {
        val now = nowMs()
        val result = if (lastAt < 0 || now <= lastAt) {
            0 to 0
        } else {
            val seconds = (now - lastAt) / 1000.0
            Math.round((frames - lastFrames) / seconds).toInt() to
                Math.round((bytes - lastBytes) * 8 / 1000.0 / seconds).toInt()
        }
        lastAt = now
        lastFrames = frames
        lastBytes = bytes
        return result
    }
}
