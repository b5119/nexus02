package com.vectorzero.nexus

import android.media.MediaCodec
import android.media.MediaFormat
import android.util.Log
import android.view.Surface
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.initializer
import androidx.lifecycle.viewmodel.viewModelFactory
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import nexus.stream.v1.StreamServiceOuterClass.InputEvent
import nexus.stream.v1.StreamServiceOuterClass.VideoFrame

/**
 * Owns the data-plane gRPC stream, the MediaCodec H.264 decoder, and the
 * shared flow that carries touch input back to the host.
 *
 * The stream must be restartable: every surface destroy/regenerate cycle
 * (background→foreground, config changes) tears the codec down in [release]
 * and re-establishes the stream on the next [connect].
 */
class StreamViewModel(private val host: PairedHost) : ViewModel() {

    companion object {
        private const val TAG = "NexusStream"
        private const val TIMEOUT_US = 10_000L

        fun factory(host: PairedHost): ViewModelProvider.Factory = viewModelFactory {
            initializer { StreamViewModel(host) }
        }
    }

    private val inputFlow = MutableSharedFlow<InputEvent>(extraBufferCapacity = 256)
    private val _status = MutableStateFlow("Connecting...")
    val status: StateFlow<String> = _status

    private val _videoSize = MutableStateFlow<Pair<Int, Int>?>(null)
    val videoSize: StateFlow<Pair<Int, Int>?> = _videoSize

    @Volatile private var surface: Surface? = null
    @Volatile private var codec: MediaCodec? = null
    private var connected = false
    private var streamJob: Job? = null
    private var codecEpoch = 0

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
        _status.value = "Connecting..."
        val epoch = codecEpoch
        streamJob?.cancel()
        streamJob = viewModelScope.launch(Dispatchers.Default) {
            try {
                GrpcClient.stream(host.address, host.certPem, host.authToken, inputFlow)
                    .collect { frame -> decodeFrame(frame) }
                _status.value = "Stream ended"
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                _status.value = "Stream ended: ${e.message}"
                Log.e(TAG, "Stream failed", e)
            } finally {
                // Only tear the codec down if no newer stream epoch claimed it.
                if (codecEpoch == epoch) stopCodec()
            }
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

    private fun stopCodec() {
        val c = codec
        codec = null
        runCatching { c?.stop() }
        runCatching { c?.release() }
    }

    private fun decodeFrame(frame: VideoFrame) {
        val codec = ensureCodec(frame.width, frame.height) ?: return
        val inputIndex = codec.dequeueInputBuffer(TIMEOUT_US)
        if (inputIndex >= 0) {
            val buffer = codec.getInputBuffer(inputIndex) ?: return
            val data = frame.data.asReadOnlyByteBuffer()
            val size = data.remaining()
            buffer.clear()
            if (size <= buffer.remaining()) {
                buffer.put(data)
                val flags = if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                codec.queueInputBuffer(inputIndex, 0, size, frame.timestampMs * 1000, flags)
            }
        }
        drainCodec(codec)
    }

    private fun ensureCodec(width: Int, height: Int): MediaCodec? {
        codec?.let { return it }
        if (width <= 0 || height <= 0) return null
        val sfc = surface ?: return null
        _videoSize.value = width to height
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
        val codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        codec.configure(format, sfc, null, 0)
        codec.start()
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
                    _status.value = "Streaming (${fmt.getInteger(MediaFormat.KEY_WIDTH)}x" +
                        "${fmt.getInteger(MediaFormat.KEY_HEIGHT)})"
                }
                out == MediaCodec.INFO_OUTPUT_BUFFERS_CHANGED -> continue
                out >= 0 -> {
                    val render = info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0
                    codec.releaseOutputBuffer(out, render)
                    if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) return
                }
            }
        }
    }
}
