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

    data class TlsMaterials(val sslContext: SSLContext, val trustManager: X509TrustManager)

    fun buildTlsMaterials(hostCertPem: String): TlsMaterials {
        val pinned = parsePem(hostCertPem)
        val trustManager = PinnedTrustManager(pinned)
        val sslContext = SSLContext.getInstance("TLS").apply {
            init(null, arrayOf(trustManager), SecureRandom())
        }
        return TlsMaterials(sslContext, trustManager)
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

        override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?, host: String?) {
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
