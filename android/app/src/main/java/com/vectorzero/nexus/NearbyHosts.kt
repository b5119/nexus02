package com.vectorzero.nexus

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Handler
import android.os.Looper
import android.util.Log

/** A Nexus host announcing itself on the same network. */
data class NearbyHost(
    val serviceName: String,
    val name: String,
    val address: String,
    val port: Int,
    /** The host's device id (matches `PairedHost.hostDeviceId` once paired). */
    val deviceId: String,
    /** Port of the host's approval-pairing listener. */
    val approvePort: Int,
)

/**
 * Finds Nexus hosts on the network through the `_nexus._tcp` announcement that every
 * running `nexus-agent serve` publishes, so the user can just tap one to pair. Results
 * are delivered on the main thread.
 *
 * NsdManager can resolve only one service at a time on older Android versions, so
 * resolves are queued and run one after another.
 */
class NearbyHosts(context: Context, private val onChange: (List<NearbyHost>) -> Unit) {

    private val nsd = context.getSystemService(Context.NSD_SERVICE) as NsdManager
    private val main = Handler(Looper.getMainLooper())
    private val found = LinkedHashMap<String, NearbyHost>()
    private val pending = ArrayDeque<NsdServiceInfo>()
    private var resolving = false
    private var running = false
    private var broadcastHosts: List<NearbyHost> = emptyList()
    private val broadcast = BroadcastDiscovery(context) { hosts ->
        broadcastHosts = hosts
        publish()
    }

    private val discovery = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(serviceType: String) {}
        override fun onDiscoveryStopped(serviceType: String) {}
        override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
            Log.w(TAG, "discovery start failed: $errorCode")
            running = false
        }

        override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}

        override fun onServiceFound(info: NsdServiceInfo) {
            main.post {
                pending.addLast(info)
                resolveNext()
            }
        }

        override fun onServiceLost(info: NsdServiceInfo) {
            main.post {
                if (found.remove(info.serviceName) != null) publish()
            }
        }
    }

    fun start() {
        if (running) return
        running = true
        // Two independent routes: mDNS (home networks) and UDP broadcast (networks that
        // drop multicast, such as campus Wi-Fi). Results are merged by device id.
        broadcast.start()
        nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, discovery)
    }

    fun stop() {
        if (!running) return
        running = false
        broadcast.stop()
        broadcastHosts = emptyList()
        try {
            nsd.stopServiceDiscovery(discovery)
        } catch (_: IllegalArgumentException) {
            // already stopped
        }
        pending.clear()
        found.clear()
    }

    private fun resolveNext() {
        if (resolving) return
        val next = pending.removeFirstOrNull() ?: return
        resolving = true
        @Suppress("DEPRECATION")
        nsd.resolveService(next, object : NsdManager.ResolveListener {
            override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) {
                main.post {
                    resolving = false
                    resolveNext()
                }
            }

            override fun onServiceResolved(info: NsdServiceInfo) {
                main.post {
                    resolving = false
                    val address = ipv4Address(info)
                    if (address != null && running) {
                        val attr = { key: String -> info.attributes[key]?.toString(Charsets.UTF_8) }
                        found[info.serviceName] = NearbyHost(
                            serviceName = info.serviceName,
                            name = attr("display_name")?.ifBlank { null } ?: info.serviceName,
                            address = address,
                            port = info.port,
                            deviceId = attr("device_id").orEmpty(),
                            approvePort = attr("approve_port")?.toIntOrNull() ?: DEFAULT_APPROVE_PORT,
                        )
                        publish()
                    }
                    resolveNext()
                }
            }
        })
    }

    /** Prefers an IPv4 address: the tablet reaches the host on its LAN, not via link-local IPv6. */
    @Suppress("DEPRECATION")
    private fun ipv4Address(info: NsdServiceInfo): String? {
        val candidates = if (android.os.Build.VERSION.SDK_INT >= 34) info.hostAddresses else listOfNotNull(info.host)
        return (candidates.firstOrNull { it is java.net.Inet4Address } ?: candidates.firstOrNull())?.hostAddress
    }

    /** mDNS hosts first, then broadcast hosts replace any with the same device id: the broadcast
     *  address is the packet's real source, so it is the one known to be reachable. */
    private fun publish() {
        val merged = LinkedHashMap<String, NearbyHost>()
        for (h in found.values) merged[h.deviceId.ifEmpty { h.serviceName }] = h
        for (h in broadcastHosts) merged[h.deviceId.ifEmpty { h.serviceName }] = h
        onChange(merged.values.toList())
    }

    companion object {
        private const val TAG = "NexusNearby"
        private const val SERVICE_TYPE = "_nexus._tcp."
        private const val DEFAULT_APPROVE_PORT = 50053
    }
}
