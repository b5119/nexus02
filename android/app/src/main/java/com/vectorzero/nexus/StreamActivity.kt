package com.vectorzero.nexus

import android.os.Bundle
import android.view.KeyEvent
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
 * onto a SurfaceView, and forwards touch / pointer / keyboard input back over
 * the gRPC stream.
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
                        if (size != null) {
                            inputHandler.setVideoSize(size.first, size.second)
                            binding.videoSurface.setVideoSize(size.first, size.second)
                        }
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
                // Tear the codec+stream only when leaving; during rotation the
                // surface is recreated and the stream survives via setSurface().
                if (isFinishing || isDestroyed) viewModel.release()
            }
        })

        binding.videoSurface.setOnTouchListener { _, event ->
            val vw = binding.videoSurface.width
            val vh = binding.videoSurface.height
            if (vw > 0 && vh > 0) inputHandler.handleTouch(event, vw, vh)
            true
        }

        setupControls()
    }

    private fun setupControls() {
        // Default: touch mode selected.
        binding.modeToggle.check(binding.touchModeButton.id)

        binding.modeToggle.addOnButtonCheckedListener { _, checkedId, isChecked ->
            if (!isChecked) return@addOnButtonCheckedListener
            if (checkedId == binding.pointerModeButton.id) {
                inputHandler.setMode(InputHandler.Mode.POINTER)
            } else {
                inputHandler.setMode(InputHandler.Mode.TOUCH)
            }
        }

        binding.keyboardButton.setOnClickListener { toggleKeyboard() }

        binding.fitToggle.setOnClickListener {
            binding.videoSurface.fillMode = !binding.videoSurface.fillMode
            binding.videoSurface.requestLayout()
        }
    }

    private fun toggleKeyboard() {
        // Toggles the on-screen soft keyboard for text entry.
        val cm = getSystemService(INPUT_METHOD_SERVICE) as android.view.inputmethod.InputMethodManager
        cm.toggleSoftInput(0, 0)
        binding.videoSurface.requestFocus()
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        // Forward usable keys to the host; keep system/navigation keys for the app.
        if (event.action == KeyEvent.ACTION_DOWN || event.action == KeyEvent.ACTION_UP) {
            val down = event.action == KeyEvent.ACTION_DOWN
            // Non-repeat, non-system keys only.
            if (isHostKey(event.keyCode)) {
                inputHandler.sendKey(event.keyCode, down)
                return true
            }
        }
        return super.dispatchKeyEvent(event)
    }

    private fun isHostKey(keyCode: Int): Boolean = when (keyCode) {
        KeyEvent.KEYCODE_BACK,
        KeyEvent.KEYCODE_HOME,
        KeyEvent.KEYCODE_MENU,
        KeyEvent.KEYCODE_APP_SWITCH,
        KeyEvent.KEYCODE_VOLUME_UP,
        KeyEvent.KEYCODE_VOLUME_DOWN,
        KeyEvent.KEYCODE_VOLUME_MUTE,
        KeyEvent.KEYCODE_POWER,
        KeyEvent.KEYCODE_CAMERA,
        KeyEvent.KEYCODE_HEADSETHOOK,
        KeyEvent.KEYCODE_WAKEUP,
        KeyEvent.KEYCODE_SLEEP -> false
        else -> true
    }

    override fun onDestroy() {
        super.onDestroy()
        viewModel.release()
    }
}