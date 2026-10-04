package com.vectorzero.nexus

/**
 * A host's announcement on the LAN, `NEXUS1<TAB>device_id<TAB>name<TAB>data_port<TAB>approve_port`
 * (see `beacon.rs` on the host side). Used where multicast DNS is blocked.
 */
data class HostBeacon(
    val deviceId: String,
    val name: String,
    val dataPort: Int,
    val approvePort: Int,
) {
    companion object {
        const val PORT = 50054
        val PROBE = "NEXUS1?".toByteArray(Charsets.US_ASCII)

        /** @return the beacon, or null if [payload] is not a well-formed Nexus beacon. */
        fun parse(payload: ByteArray, length: Int = payload.size): HostBeacon? {
            val text = try {
                String(payload, 0, length, Charsets.UTF_8)
            } catch (_: Exception) {
                return null
            }
            val parts = text.split('\t')
            if (parts.size != 5 || parts[0] != "NEXUS1") return null
            val deviceId = parts[1]
            if (deviceId.isEmpty()) return null
            val data = parts[3].toIntOrNull() ?: return null
            val approve = parts[4].toIntOrNull() ?: return null
            if (data !in 1..65535 || approve !in 1..65535) return null
            // Shown to the user, so drop control characters and cap the length.
            val name = parts[2].filter { !it.isISOControl() }.take(40).ifBlank { "Nexus host" }
            return HostBeacon(deviceId, name, data, approve)
        }

        /** The directed broadcast address (e.g. 172.16.255.255) of [address]/[prefixLength]. */
        fun directedBroadcast(address: ByteArray, prefixLength: Int): ByteArray {
            require(address.size == 4 && prefixLength in 0..32) { "IPv4 address and prefix 0..32 required" }
            val out = address.copyOf()
            for (i in 0 until 4) {
                val bitsInThisByte = (prefixLength - i * 8).coerceIn(0, 8)
                val hostMask = (0xFF shr bitsInThisByte) and 0xFF
                out[i] = (address[i].toInt() or hostMask).toByte()
            }
            return out
        }
    }
}
