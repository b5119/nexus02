package com.vectorzero.nexus

import android.os.Bundle
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.lifecycle.lifecycleScope
import com.vectorzero.nexus.databinding.ActivityPairingBinding
import kotlinx.coroutines.launch

/**
 * Pairs this viewer with a nexus host. The host advertises itself on the LAN
 * via mDNS as _nexus._tcp, and the 6-digit code is exchanged over the
 * plaintext pairing port (50052). On success the returned host cert + auth
 * token are persisted and used for the TLS data plane.
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

        binding.discoverButton.setOnClickListener { discover() }
        binding.pairButton.setOnClickListener { pair() }
    }

    override fun onDestroy() {
        super.onDestroy()
        stopDiscovery()
    }

    private fun discover() {
        val nsdManager = getSystemService(NSD_SERVICE) as android.net.nsd.NsdManager
        binding.statusText.text = "Discovering nexus hosts..."

        val resolver = object : android.net.nsd.NsdManager.ResolveListener {
            override fun onResolveFailed(serviceInfo: android.net.nsd.NsdServiceInfo, errorCode: Int) {}
            override fun onServiceResolved(serviceInfo: android.net.nsd.NsdServiceInfo) {
                runOnUiThread {
                    serviceInfo.host?.hostAddress?.let { addr ->
                        binding.addressInput.setText(addr)
                    }
                }
            }
        }

        val discoveryListener = object : android.net.nsd.NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) {}
            override fun onServiceFound(serviceInfo: android.net.nsd.NsdServiceInfo) {
                if (serviceInfo.serviceType == "_nexus._tcp.") {
                    nsdManager.resolveService(serviceInfo, resolver)
                }
            }
            override fun onServiceLost(serviceInfo: android.net.nsd.NsdServiceInfo) {}
            override fun onDiscoveryStopped(serviceType: String) {}
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                runOnUiThread { binding.statusText.text = "Discovery failed (code $errorCode)" }
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}
        }

        nsdManager.discoverServices("_nexus._tcp.", android.net.nsd.NsdManager.PROTOCOL_DNS_SD, discoveryListener)
    }

    private fun stopDiscovery() {
        runCatching {
            val nsdManager = getSystemService(NSD_SERVICE) as android.net.nsd.NsdManager
            nsdManager.stopServiceDiscovery(null)
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
                }
            } catch (e: kotlinx.coroutines.CancellationException) {
                throw e
            } catch (e: Exception) {
                binding.statusText.text = "Pairing failed: ${e.message}"
            } finally {
                if (!isFinishing && !isDestroyed) binding.pairButton.isEnabled = true
            }
        }
    }
}
