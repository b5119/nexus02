package com.vectorzero.nexus

import java.security.SecureRandom
import java.security.cert.CertificateException
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import javax.net.ssl.SSLContext
import javax.net.ssl.X509TrustManager

/**
 * Trust manager that pins the agent's self-signed host certificate obtained
 * through the pairing exchange. Only that certificate (matched by public key)
 * is trusted, so the TLS data plane cannot be intercepted with a different
 * certificate.
 *
 * The Android viewer is a client-only participant: it presents no mTLS
 * identity certificate (PairRequest.initiator_cert_pem is sent empty), so the
 * host authenticates this device purely via the x-nexus-token metadata header.
 * mTLS-capable clients are not supported by this viewer yet.
 */
object TrustStore {

    fun sslContext(hostCertPem: String): SSLContext {
        val pinned = parsePem(hostCertPem)
        val trustManager = PinnedTrustManager(pinned)
        return SSLContext.getInstance("TLS").apply {
            init(null, arrayOf(trustManager), SecureRandom())
        }
    }

    /**
     * Trust-on-first-use context for the PAIRING port (50052) only.
     *
     * The host cert is unknown before the 6-digit-code exchange, so the
     * pairing handshake deliberately accepts any certificate; the pairing
     * listener is authenticated by the short-lived one-time code, not by TLS
     * identity (ADR 0013). The returned host_cert_pem is then pinned for all
     * later data-plane connections via [sslContext]. Never use this context
     * for the data plane.
     */
    fun pairingContext(): SSLContext {
        val trustAll = object : X509TrustManager {
            override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) {}
            override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {}
            override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
        }
        return SSLContext.getInstance("TLS").apply {
            init(null, arrayOf(trustAll), SecureRandom())
        }
    }

    private fun parsePem(pem: String): X509Certificate {
        val begin = pem.indexOf("-----BEGIN CERTIFICATE-----")
        val end = pem.indexOf("-----END CERTIFICATE-----")
        require(begin >= 0 && end > begin) { "Invalid PEM certificate" }
        val body = pem.substring(begin, end + "-----END CERTIFICATE-----".length)
        val cf = CertificateFactory.getInstance("X.509")
        return cf.generateCertificate(body.byteInputStream()) as X509Certificate
    }

    private class PinnedTrustManager(private val pinned: X509Certificate) : X509TrustManager {

        override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) {
            throw CertificateException("Client certificates are not accepted")
        }

        override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {
            verify(chain)
        }

        override fun getAcceptedIssuers(): Array<X509Certificate> = arrayOf(pinned)

        private fun verify(chain: Array<out X509Certificate>?) {
            val leaf = chain?.firstOrNull() ?: throw CertificateException("Empty certificate chain")
            if (leaf.publicKey != pinned.publicKey) {
                throw CertificateException("Certificate is not the pinned host certificate")
            }
        }
    }
}
