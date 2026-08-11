package com.vectorzero.nexus

import android.os.Bundle
import android.view.SurfaceHolder
import androidx.activity.viewModels
import androidx.appcompat.app.AppCompatActivity
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import com.vectorzero.nexus.databinding.ActivityStreamBinding
import kotlinx.coroutines.launch

/**
 * Decodes and displays the H.264 stream from the paired host via MediaCodec
 * onto a SurfaceView, and forwards touch input back over the gRPC stream.
 */
class StreamActivity : AppCompatActivity() {

    companion object {
        const val EXTRA_HOST_ID = "host_id"
    }

    private lateinit var binding: ActivityStreamBinding
    private lateinit var inputHandler: InputHandler

    private val viewModel: StreamViewModel by viewModels {
        val host = HostStore.loadHosts(this).firstOrNull {
            it.id == intent.getStringExtra(EXTRA_HOST_ID)
        }
        requireNotNull(host) { "Paired host not found" }
        StreamViewModel.factory(host)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = ActivityStreamBinding.inflate(layoutInflater)
        setContentView(binding.root)

        inputHandler = InputHandler { event -> viewModel.sendInput(event) }

        lifecycleScope.launch {
            repeatOnLifecycle(Lifecycle.State.STARTED) {
                launch {
                    viewModel.status.collect { binding.statusText.text = it }
                }
                launch {
                    viewModel.videoSize.collect { size ->
                        if (size != null) inputHandler.setVideoSize(size.first, size.second)
                    }
                }
            }
        }

        binding.videoSurface.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                viewModel.setSurface(holder.surface)
                viewModel.connect()
            }

            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

            override fun surfaceDestroyed(holder: SurfaceHolder) {
                viewModel.release()
            }
        })

        binding.videoSurface.setOnTouchListener { _, event ->
            val vw = binding.videoSurface.width
            val vh = binding.videoSurface.height
            if (vw > 0 && vh > 0) inputHandler.handleTouch(event, vw, vh)
            true
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        viewModel.release()
    }
}
