package com.vectorzero.nexus

import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.Editable
import android.text.TextWatcher
import android.util.Log
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.lifecycle.lifecycleScope
import com.google.android.material.button.MaterialButton
import com.vectorzero.nexus.databinding.ActivityPairingBinding
import io.grpc.Status
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch

/**
 * Pairs this viewer with a Nexus host. The normal path is two taps:
 *
 *  1. **Tap your computer** in the "Nearby hosts" list. It is found by UDP broadcast (works on
 *     campus Wi-Fi that blocks mDNS) and mDNS (home networks).
 *  2. The computer shows an approval dialog with a 4-digit code; if it matches the code
 *     shown here, click **Pair** there. No typing.
 *
 * Fallbacks when nothing is listed: type the computer's address and tap Pair (approval
 * again), or add the 6-digit code from `nexus-agent pair-mode` for the old code flow.
 *
 * On success the returned host cert + auth token are persisted and used for the TLS
 * data plane.
 */
class PairingActivity : AppCompatActivity() {

    private companion object {
        const val TAG = "NexusPair"

        /** Port of the host's approval-pairing listener (`APPROVAL_PORT` in approval.rs). */
        const val APPROVE_PORT = 50053
        const val NO_HOSTS_HINT_DELAY_MS = 8_000L
    }

    private lateinit var binding: ActivityPairingBinding
    private lateinit var nearby: NearbyHosts
    private var pairing = false
    private var foundAny = false

    /** Display name of the host last tapped, kept so the saved host has a friendly name. */
    private var selectedName: String? = null

    private val handler = Handler(Looper.getMainLooper())
    private val noHostsHint = Runnable {
        if (!foundAny) {
            binding.nearbyHint.text =
                "No computers found yet. Make sure the computer is on this Wi-Fi network and " +
                    "running Nexus. You can also type its address below."
        }
    }

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
        binding.pairButton.setOnClickListener { pairManually() }

        // Code flow only: pair automatically once the sixth digit is typed.
        binding.codeInput.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
            override fun afterTextChanged(s: Editable?) {
                if (s?.length == 6 && binding.addressInput.text?.isNotBlank() == true) pairManually()
            }
        })

        nearby = NearbyHosts(this) { hosts -> showNearby(hosts) }
    }

    override fun onStart() {
        super.onStart()
        foundAny = false
        nearby.start()
        handler.postDelayed(noHostsHint, NO_HOSTS_HINT_DELAY_MS)
    }

    override fun onStop() {
        handler.removeCallbacks(noHostsHint)
        nearby.stop()
        super.onStop()
    }

    override fun onSupportNavigateUp(): Boolean {
        onBackPressedDispatcher.onBackPressed()
        return true
    }

    private fun showNearby(hosts: List<NearbyHost>) {
        val paired = HostStore.loadHosts(this).map { it.hostDeviceId }.toSet()
        foundAny = hosts.isNotEmpty()
        binding.nearbyList.removeAllViews()
        binding.nearbyHint.text =
            if (hosts.isEmpty()) getString(R.string.nearby_hint)
            else "Tap your computer. It will ask you to confirm on its screen."
        for (host in hosts) {
            val isPaired = host.deviceId in paired
            val button = MaterialButton(this, null, com.google.android.material.R.attr.materialButtonOutlinedStyle)
            button.text = "${host.name}  (${host.address})" + if (isPaired) "  ✓ paired" else ""
            button.setOnClickListener {
                if (isPaired) {
                    binding.statusText.text = "Already paired. Go back and tap it in the list to connect."
                } else {
                    startApproval(host)
                }
            }
            binding.nearbyList.addView(button)
        }
    }

    /**
     * Pair by approval: ask the host, show the short code, and wait while the user clicks
     * "Pair" in the dialog on the computer. The code is computed from the certificate we
     * actually received, so it only matches the host's code if nobody is in between.
     */
    private fun startApproval(host: NearbyHost) {
        if (pairing) return
        selectedName = host.name
        setBusy(true)
        binding.statusText.textSize = 14f
        binding.statusText.text = "Contacting ${host.name}…"
        lifecycleScope.launch {
            try {
                val deviceName = "${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}".trim()
                val resp = GrpcClient.pairByApproval(
                    host.address, host.approvePort, deviceId, deviceName,
                    onRetry = { attempt, of ->
                        runOnUiThread {
                            binding.statusText.textSize = 14f
                            binding.statusText.text = "Reaching ${host.name}… (try $attempt of $of)\nThe Wi-Fi link is slow to answer."
                        }
                    },
                ) { fingerprint ->
                    val code = Sas.compute(deviceId, fingerprint)
                    runOnUiThread {
                        binding.statusText.textSize = 22f
                        binding.statusText.text =
                            "Click “Pair” on ${host.name} if it shows this code:\n\n$code"
                    }
                }
                if (resp.accepted) {
                    saveAndFinish(resp, host.name, host.address)
                } else {
                    fail(resp.errorMessage.ifEmpty { "Pairing was not approved" })
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Log.w(TAG, "approval pairing failed (${host.address}:${host.approvePort})", e)
                fail("Could not pair with ${host.address}: ${describe(e)}")
            }
        }
    }

    private fun pairManually() {
        if (pairing) return
        val host = binding.addressInput.text.toString().trim()
        val code = binding.codeInput.text.toString().trim()
        when {
            host.isEmpty() -> binding.statusText.text = "Tap a computer above, or type its address"
            // Address only: pair by approval, same as tapping a listed computer.
            code.isEmpty() -> startApproval(NearbyHost(host, selectedName ?: host, host, 50051, "", APPROVE_PORT))
            code.length != 6 || !code.all { it.isDigit() } ->
                binding.statusText.text = "Code must be 6 digits"
            else -> pairWithCode(host, code)
        }
    }

    /** The older flow: a 6-digit code printed by `nexus-agent pair-mode`. */
    private fun pairWithCode(host: String, code: String) {
        setBusy(true)
        binding.statusText.textSize = 14f
        binding.statusText.text = "Pairing…"
        lifecycleScope.launch {
            try {
                val resp = GrpcClient.pair(host, code, deviceId)
                if (resp.accepted) {
                    saveAndFinish(resp, "Nexus host ($host)", host)
                } else {
                    fail("Pairing rejected: ${resp.errorMessage}")
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Log.w(TAG, "code pairing failed ($host)", e)
                fail("Pairing failed: ${describe(e)}")
            }
        }
    }

    private fun saveAndFinish(resp: nexus.pair.v1.PairServiceOuterClass.PairResponse, name: String, address: String) {
        HostStore.saveHost(
            this,
            PairedHost(
                id = resp.hostDeviceId,
                name = name,
                address = address,
                hostDeviceId = resp.hostDeviceId,
                certPem = resp.hostCertPem,
                authToken = resp.authToken
            )
        )
        Toast.makeText(this, "Paired with $name", Toast.LENGTH_SHORT).show()
        finish()
    }

    /** A readable reason: gRPC status plus the underlying cause (timeout, refused, TLS...). */
    private fun describe(e: Exception): String {
        val status = Status.fromThrowable(e)
        val cause = generateSequence<Throwable>(e) { it.cause }.last()
        val detail = status.description ?: cause.message ?: cause.javaClass.simpleName
        return "${status.code}: $detail"
    }

    private fun setBusy(busy: Boolean) {
        pairing = busy
        binding.pairButton.isEnabled = !busy
    }

    private fun fail(message: String) {
        binding.statusText.textSize = 14f
        binding.statusText.text = message
        setBusy(false)
    }
}
