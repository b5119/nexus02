package com.vectorzero.nexus

import java.nio.ByteBuffer
import java.security.MessageDigest

/**
 * The short code shown on BOTH the tablet and the computer when pairing by approval.
 *
 * It is `SHA-256("<this device's id>|<host certificate fingerprint>")` mod 10,000, where
 * the fingerprint is of the certificate the tablet actually received in the TLS
 * handshake. The host computes the same value from its own certificate. If someone
 * relays the connection with a different certificate the two codes differ, and the
 * user, who must compare them before clicking Pair, sees it.
 *
 * Must stay identical to `short_auth_string` in `crates/agent/src/approval.rs`; both
 * assert the same test vector.
 */
object Sas {
    fun compute(deviceId: String, hostCertFingerprintHex: String): String {
        val digest = MessageDigest.getInstance("SHA-256")
            .digest("$deviceId|$hostCertFingerprintHex".toByteArray(Charsets.UTF_8))
        val n = (ByteBuffer.wrap(digest, 0, 4).int.toLong() and 0xFFFFFFFFL) % 10_000
        return "%04d".format(n)
    }
}
