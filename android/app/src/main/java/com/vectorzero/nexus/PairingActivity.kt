package com.vectorzero.nexus

import android.os.Bundle
import android.view.View
import android.widget.*
import androidx.appcompat.app.AppCompatActivity
import androidx.lifecycle.lifecycleScope
import com.vectorzero.nexus.databinding.ActivityPairingBinding
import kotlinx.coroutines.*

/**
 * Pairs this viewer with a nexus host. The host advertises itself on the LAN
 * via the pairing port (50052), and a 6-digit code is exchanged over plaintext.
 * On success the returned host cert + auth token are persisted and used for the TLS data plane.
 */
class PairingActivity : AppCompatActivity() {

    private lateinit var binding: ActivityPairingBinding

    private val deviceId by lazy {
        val prefs = getSharedPreferences("nexus_prefs", MODE_PRIVATE)
        prefs.getString("device_id", null) ?: java.util.UUID.randomUUID().toString().also {
            prefs.edit().putString("device_id", it).apply()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = ActivityPairingBinding.inflate(layoutInflater)
        setContentView(binding.root)

        binding.toolbar.setNavigationOnClickListener { finish() }

        binding.discoverButton.setOnClickListener { onDiscoverClicked() }
        binding.pairButton.setOnClickListener { pair() }
    }

    override fun onDestroy() {
        super.onDestroy()
    }

    override fun onSupportNavigateUp(): Boolean {
        onBackPressedDispatcher.onBackPressed()
        return true
    }

    private fun onDiscoverClicked() {
        binding.statusText.text = "Discovering hosts..."
        lifecycleScope.launch {
            kotlinx.coroutines.delay(2000)
            runOnUiThread {
                binding.statusText.text = "Enter the host address (e.g., 192.168.1.50)"
            }
        }
    }

    private fun pair() {
        val host = binding.addressInput.text.toString().trim()
        val code = binding.codeInput.text.toString().trim()
        when {
            host.isEmpty() -> binding.statusText.text = "Enter the host address"
            code.length != 6 || !code.all { it.isDigit() } ->
                binding.statusText.text = "Code must be 6 digits"
            else -> doPair(host, code)
        }
    }

    private fun doPair(host: String, code: String) {
        binding.pairButton.isEnabled = false
        binding.statusText.text = "Pairing..."
        lifecycleScope.launch {
            try {
                val resp = GrpcClient.pair(host, code, deviceId)
                if (resp.accepted) {
                    HostStore.saveHost(
                        this@PairingActivity,
                        PairedHost(
                            id = resp.hostDeviceId,
                            name = "Nexus host ($host)",
                            address = host,
                            hostDeviceId = resp.hostDeviceId,
                            certPem = resp.hostCertPem,
                            authToken = resp.authToken
                        )
                    )
                    Toast.makeText(this@PairingActivity, "Paired successfully", Toast.LENGTH_SHORT).show()
                    finish()
                } else {
                    binding.statusText.text = "Pairing rejected: ${resp.errorMessage}"
                    binding.pairButton.isEnabled = true
                }
            } catch (e: kotlinx.coroutines.CancellationException) {
                throw e
            } catch (e: Exception) {
                binding.statusText.text = "Pairing failed: ${e.message}"
                binding.pairButton.isEnabled = true
            }
        }
    }
}