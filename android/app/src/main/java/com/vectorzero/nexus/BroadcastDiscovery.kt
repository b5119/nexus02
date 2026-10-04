package com.vectorzero.nexus

import android.content.Context
import android.net.ConnectivityManager
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.Inet4Address
import java.net.InetAddress
import java.net.InetSocketAddress

/**
 * Finds Nexus hosts with UDP broadcast, for networks (campus Wi-Fi such as eduroam)
 * that drop multicast so mDNS discovery sees nothing, but still pass broadcast and
 * direct traffic.
 *
 * Every [PROBE_INTERVAL_MS] it broadcasts the probe `NEXUS1?`; hosts reply directly with
 * a beacon, and also broadcast beacons on their own, so either direction is enough.
 * The host's address is taken from the packet's **source**, so the address shown is one
 * this device just received a packet from. A host not heard for [EXPIRY_MS] is dropped.
 * Results are delivered on the main thread.
 */
class BroadcastDiscovery(
    context: Context,
    private val onChange: (List<NearbyHost>) -> Unit,
) {
    private val appContext = context.applicationContext
    private val main = Handler(Looper.getMainLooper())
    private val seen = LinkedHashMap<String, Pair<NearbyHost, Long>>()

    @Volatile private var running = false
    private var thread: Thread? = null
    private var socket: DatagramSocket? = null
    private var multicastLock: WifiManager.MulticastLock? = null

    fun start() {
        if (running) return
        running = true
        // Some devices filter broadcast/multicast frames unless an app holds this lock.
        val wifi = appContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("nexus-discovery").apply {
            setReferenceCounted(false)
            acquire()
        }
        thread = Thread(::loop, "nexus-broadcast-discovery").also { it.isDaemon = true; it.start() }
    }

    fun stop() {
        running = false
        socket?.close()
        thread?.interrupt()
        thread = null
        multicastLock?.takeIf { it.isHeld }?.release()
        multicastLock = null
        main.post { seen.clear() }
    }

    private fun loop() {
        val sock = try {
            DatagramSocket(null).apply {
                reuseAddress = true
                broadcast = true
                soTimeout = 500
                bind(InetSocketAddress(HostBeacon.PORT))
            }
        } catch (e: Exception) {
            Log.w(TAG, "cannot open discovery socket", e)
            return
        }
        socket = sock
        val buffer = ByteArray(256)
        var nextProbe = 0L
        try {
            while (running) {
                val now = System.currentTimeMillis()
                if (now >= nextProbe) {
                    sendProbes(sock)
                    nextProbe = now + PROBE_INTERVAL_MS
                }
                try {
                    val packet = DatagramPacket(buffer, buffer.size)
                    sock.receive(packet)
                    handle(packet)
                } catch (_: java.net.SocketTimeoutException) {
                    // expected: loop to send probes and expire old hosts
                }
                expire(System.currentTimeMillis())
            }
        } catch (e: Exception) {
            if (running) Log.w(TAG, "discovery loop stopped", e)
        } finally {
            sock.close()
        }
    }

    private fun sendProbes(sock: DatagramSocket) {
        val targets = mutableListOf<InetAddress>(InetAddress.getByName("255.255.255.255"))
        targets += directedBroadcasts()
        for (target in targets) {
            try {
                sock.send(DatagramPacket(HostBeacon.PROBE, HostBeacon.PROBE.size, target, HostBeacon.PORT))
            } catch (e: Exception) {
                Log.d(TAG, "probe to $target failed: ${e.message}")
            }
        }
    }

    private fun directedBroadcasts(): List<InetAddress> {
        val cm = appContext.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val props = cm.getLinkProperties(cm.activeNetwork) ?: return emptyList()
        return props.linkAddresses
            .filter { it.address is Inet4Address }
            .map { InetAddress.getByAddress(HostBeacon.directedBroadcast(it.address.address, it.prefixLength)) }
    }

    private fun handle(packet: DatagramPacket) {
        val beacon = HostBeacon.parse(packet.data, packet.length) ?: return
        val address = packet.address as? Inet4Address ?: return
        val host = NearbyHost(
            serviceName = beacon.deviceId,
            name = beacon.name,
            address = address.hostAddress ?: return,
            port = beacon.dataPort,
            deviceId = beacon.deviceId,
            approvePort = beacon.approvePort,
        )
        val now = System.currentTimeMillis()
        main.post {
            val before = seen[beacon.deviceId]?.first
            seen[beacon.deviceId] = host to now
            if (before != host) publish()
        }
    }

    private fun expire(now: Long) {
        main.post {
            if (seen.values.removeAll { now - it.second > EXPIRY_MS }) publish()
        }
    }

    private fun publish() = onChange(seen.values.map { it.first })

    companion object {
        private const val TAG = "NexusBroadcast"
        private const val PROBE_INTERVAL_MS = 2_000L
        private const val EXPIRY_MS = 12_000L
    }
}
