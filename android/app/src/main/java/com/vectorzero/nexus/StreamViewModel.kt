package com.vectorzero.nexus

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import io.grpc.Status
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import nexus.stream.v1.StreamServiceOuterClass.InputEvent
import nexus.stream.v1.StreamServiceOuterClass.VideoFrame
import java.io.IOException
import java.util.concurrent.atomic.AtomicLong

/**
 * Owns the data-plane gRPC stream, the MediaCodec H.264 decoder, and the shared flow that
 * carries touch input back to the host.
 *
 * Reliability, in order of how often it matters:
 *  - **Reconnects by itself.** A Wi-Fi hiccup or a host restart no longer leaves a dead
 *    "Stream ended" screen: the stream is retried with a growing delay ([ReconnectPolicy]).
 *  - **Notices a stalled link.** If no video arrives for [STALL_MS] the connection is dropped
 *    and retried (a link can go quiet without ever reporting an error).
 *  - **Never decodes what it cannot decode.** A fresh or damaged decoder is only fed
 *    keyframes ([KeyframeGate]); a frame lost for any reason makes it wait for the next one
 *    instead of showing blocky garbage.
 *  - **Survives a decoder error** by rebuilding the decoder, rather than ending the stream.
 *
 * The stream must also be restartable across surface changes (background to foreground,
 * rotation): [release] tears the codec down and the next [connect] rebuilds it.
 */
class StreamViewModel(private val host: PairedHost) : ViewModel() {

    companion object {
        private const val TAG = "NexusStream"
        private const val TIMEOUT_US = 5_000L
        private const val INPUT_RETRIES = 4
        private const val STALL_MS = 8_000L

        fun factory(host: PairedHost): ViewModelProvider.Factory = viewModelFactory {
            initializer { StreamViewModel(host) }
        }
    }

    private val inputFlow = MutableSharedFlow<InputEvent>(extraBufferCapacity = 256)

    private val _state = MutableStateFlow(
        StreamState(StreamPhase.CONNECTING, "Connecting to ${host.name}…")
    )
    val state: StateFlow<StreamState> = _state

    private val _stats = MutableStateFlow(StreamStats())
    val stats: StateFlow<StreamStats> = _stats

    private val _videoSize = MutableStateFlow<Pair<Int, Int>?>(null)
    val videoSize: StateFlow<Pair<Int, Int>?> = _videoSize

    val hostName: String get() = host.name

    @Volatile private var surface: Surface? = null
    @Volatile private var codec: MediaCodec? = null
    private var connected = false
    private var streamJob: Job? = null
    private var codecEpoch = 0

    private val gate = KeyframeGate()
    private val framesDecoded = AtomicLong(0)
    private val bytesReceived = AtomicLong(0)
    @Volatile private var lastFrameAtMs = 0L
    @Volatile private var reconnects = 0

    init {
        val meter = RateMeter()
        viewModelScope.launch {
            while (isActive) {
                delay(1_000)
                val (fps, kbps) = meter.sample(framesDecoded.get(), bytesReceived.get())
                _stats.value = _stats.value.copy(fps = fps, kbps = kbps, reconnects = reconnects)
            }
        }
    }

    fun setSurface(surface: Surface) {
        this.surface = surface
        // Surface changed (e.g. rotation): drop the codec bound to the old
        // surface; the running stream re-creates it on the next frame.
        stopCodec()
        // Reset connected so connect() will restart the codec
        // pipeline when the new surface is ready after rotation.
        if (streamJob?.isActive == true) connected = false
    }

    fun sendInput(event: InputEvent) {
        inputFlow.tryEmit(event)
    }

    fun connect() {
        if (connected) return
        connected = true
        val epoch = codecEpoch
        streamJob?.cancel()
        streamJob = viewModelScope.launch(Dispatchers.Default) {
            var attempt = 0
            while (isActive) {
                val startedAt = System.currentTimeMillis()
                _state.value =
                    if (attempt == 0) StreamState(StreamPhase.CONNECTING, "Connecting to ${host.name}…")
                    else StreamState(StreamPhase.RECONNECTING, "Reconnecting to ${host.name}… (try $attempt)")

                val failure: Exception? = try {
                    runOnce()
                    null // the host closed the stream normally
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    Log.w(TAG, "stream failed", e)
                    e
                }
                if (!isActive) break

                if (failure != null && isRejected(failure)) {
                    _state.value = StreamState(
                        StreamPhase.ENDED,
                        "${host.name} rejected this tablet. Remove it and pair again."
                    )
                    break
                }

                // A connection that stayed up for a while was healthy: start the backoff over.
                val healthy = System.currentTimeMillis() - startedAt >= ReconnectPolicy.HEALTHY_AFTER_MS
                attempt = if (healthy) 1 else attempt + 1
                reconnects++
                gate.reset()
                val waitMs = ReconnectPolicy.delayMs(attempt)
                val reason = failure?.let { shortReason(it) } ?: "stream ended"
                _state.value = StreamState(
                    StreamPhase.RECONNECTING,
                    "Connection lost ($reason). Retrying in ${waitMs / 1000}s…"
                )
                delay(waitMs)
            }
            // Only tear the codec down if no newer stream epoch claimed it.
            if (codecEpoch == epoch) stopCodec()
        }
    }

    /** One connection: collect frames until the stream ends, fails or stalls. */
    private suspend fun runOnce() = coroutineScope {
        lastFrameAtMs = System.currentTimeMillis()
        val watchdog = launch {
            while (isActive) {
                delay(1_000)
                if (System.currentTimeMillis() - lastFrameAtMs > STALL_MS) {
                    throw IOException("no video for ${STALL_MS / 1000} s")
                }
            }
        }
        try {
            GrpcClient.stream(host.address, host.certPem, host.authToken, inputFlow)
                .collect { frame ->
                    lastFrameAtMs = System.currentTimeMillis()
                    decodeFrame(frame)
                }
        } finally {
            watchdog.cancel()
        }
    }

    fun release() {
        surface = null
        codecEpoch++
        streamJob?.cancel()
        streamJob = null
        connected = false
        stopCodec()
    }

    override fun onCleared() {
        release()
        super.onCleared()
    }

    private fun stopCodec() {
        val c = codec
        codec = null
        gate.reset()
        runCatching { c?.stop() }
        runCatching { c?.release() }
    }

    private fun decodeFrame(frame: VideoFrame) {
        bytesReceived.addAndGet(frame.data.size().toLong())
        try {
            val codec = ensureCodec(frame.width, frame.height) ?: return
            // A decoder that just started, or lost a frame, may only restart on a keyframe.
            if (!gate.accept(frame.keyframe)) return

            var index = -1
            for (attempt in 0 until INPUT_RETRIES) {
                index = codec.dequeueInputBuffer(TIMEOUT_US)
                if (index >= 0) break
                drainCodec(codec) // free output buffers so the decoder can take more input
            }
            if (index < 0) {
                // We had to drop this frame: everything after it is undecodable until a keyframe.
                gate.reset()
                return
            }
            val buffer = codec.getInputBuffer(index)
            val data = frame.data.asReadOnlyByteBuffer()
            val size = data.remaining()
            if (buffer == null || size > buffer.capacity()) {
                codec.queueInputBuffer(index, 0, 0, 0, 0) // hand the buffer back unused
                gate.reset()
                return
            }
            buffer.clear()
            buffer.put(data)
            val flags = if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
            codec.queueInputBuffer(index, 0, size, frame.timestampMs * 1000, flags)
            framesDecoded.incrementAndGet()
            drainCodec(codec)
        } catch (e: MediaCodec.CodecException) {
            recoverFromCodecError(e)
        } catch (e: IllegalStateException) {
            recoverFromCodecError(e)
        }
    }

    /** The decoder broke (it can on some devices after a surface change): rebuild it. */
    private fun recoverFromCodecError(e: Exception) {
        Log.w(TAG, "decoder error, rebuilding", e)
        stopCodec()
    }

    private fun ensureCodec(width: Int, height: Int): MediaCodec? {
        codec?.let { return it }
        if (width <= 0 || height <= 0) return null
        val sfc = surface ?: return null
        _videoSize.value = width to height
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
        // Hints for a live stream: lowest latency, real-time priority, full speed.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        format.setInteger(MediaFormat.KEY_PRIORITY, 0)
        format.setInteger(MediaFormat.KEY_OPERATING_RATE, Short.MAX_VALUE.toInt())
        val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec.configure(format, sfc, null, 0)
        codec.start()
        gate.reset() // a new decoder must begin at a keyframe
        this.codec = codec
        return codec
    }

    private fun drainCodec(codec: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (true) {
            val out = codec.dequeueOutputBuffer(info, 0L)
            when {
                out == MediaCodec.INFO_TRY_AGAIN_LATER -> return
                out == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    val fmt = codec.outputFormat
                    _stats.value = _stats.value.copy(
                        width = fmt.getInteger(MediaFormat.KEY_WIDTH),
                        height = fmt.getInteger(MediaFormat.KEY_HEIGHT),
                    )
                }
                out == MediaCodec.INFO_OUTPUT_BUFFERS_CHANGED -> continue
                out >= 0 -> {
                    val render = info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0
                    codec.releaseOutputBuffer(out, render)
                    // The first picture actually shown is when the stream is really live.
                    if (render && _state.value.phase != StreamPhase.LIVE) {
                        _state.value = StreamState(StreamPhase.LIVE, "Streaming")
                    }
                    if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) return
                }
            }
        }
    }

    /** The host refused us (token revoked or unpaired): retrying cannot help. */
    private fun isRejected(e: Exception): Boolean =
        Status.fromThrowable(e).code.let {
            it == Status.Code.UNAUTHENTICATED || it == Status.Code.PERMISSION_DENIED
        }

    private fun shortReason(e: Exception): String {
        val status = Status.fromThrowable(e)
        val cause = generateSequence<Throwable>(e) { it.cause }.last()
        return when {
            cause is java.net.NoRouteToHostException -> "host unreachable"
            cause is java.net.ConnectException -> "connection refused"
            e is IOException && e.message?.startsWith("no video") == true -> e.message!!
            status.code != Status.Code.UNKNOWN -> status.code.name.lowercase().replace('_', ' ')
            else -> cause.message ?: cause.javaClass.simpleName
        }
    }
}
