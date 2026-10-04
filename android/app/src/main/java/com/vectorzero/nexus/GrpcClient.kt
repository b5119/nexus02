package com.vectorzero.nexus

import io.grpc.ManagedChannel
import io.grpc.Metadata
import io.grpc.okhttp.OkHttpChannelBuilder
import io.grpc.stub.MetadataUtils
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.withContext
import nexus.pair.v1.PairServiceGrpcKt
import nexus.pair.v1.PairServiceOuterClass.PairRequest
import nexus.pair.v1.PairServiceOuterClass.PairResponse
import nexus.stream.v1.StreamServiceGrpcKt
import nexus.stream.v1.StreamServiceOuterClass.InputEvent
import nexus.stream.v1.StreamServiceOuterClass.VideoFrame
import java.util.concurrent.TimeUnit

/**
 * gRPC client for the nexus host.
 *
 * - Pairing (port 50052) runs over TLS with trust-on-first-use: the host cert
 *   is unknown before the exchange, so the pairing handshake accepts any cert
 *   and relies on the one-time 6-digit code (ADR 0013). The returned host
 *   cert + auth token are then used for the data plane.
 * - Data plane (port 50051) runs over TLS with the host certificate pinned in
 *   [TrustStore], authenticated by the x-nexus-token metadata header.
 */
object GrpcClient {
    private const val TAG = "NexusGrpc"
    private const val AUTH_HEADER = "x-nexus-token"
    private const val PAIR_PORT = 50052
    private const val DATA_PORT = 50051

    private var pairChannel: ManagedChannel? = null
    private var dataChannel: ManagedChannel? = null

    /** Exchanges the pairing code for the host cert + token. Trust-on-first-use TLS. */
    suspend fun pair(host: String, code: String, deviceId: String): PairResponse =
        withContext(Dispatchers.IO) {
            // The host certificate is unknown before the exchange (trust-on-first-use);
            // the one-time code is what authenticates this connection (ADR 0013).
            val context = TrustStore.pairingContext()
            val channel = OkHttpChannelBuilder.forAddress(host, PAIR_PORT)
                .sslSocketFactory(context.socketFactory)
                .hostnameVerifier { _, _ -> true }
                .build()
            pairChannel?.shutdownNow()
            pairChannel = channel
            val stub = PairServiceGrpcKt.PairServiceCoroutineStub(channel)
            stub.requestPair(
                PairRequest.newBuilder()
                    .setCode(code)
                    .setInitiatorDeviceId(deviceId)
                    .setInitiatorCertPem("") // viewer-only client: no mTLS identity cert
                    .setInitiatorDisplayName("Nexus Android Viewer")
                    .build()
            )
        }

    /**
     * Asks the host to pair by approval: the user clicks "Pair" in a dialog on the host
     * and compares the short code (see [Sas]). [onHostCertificate] receives the SHA-256
     * fingerprint of the certificate the host presented, during the handshake, so the
     * code can be displayed while this call waits for the answer (up to ~75 s).
     *
     * Wi-Fi between two clients can be flaky (a sleeping radio makes the first packets fail
     * with "host unreachable"), so a failure to even CONNECT is retried up to
     * [CONNECT_ATTEMPTS] times, calling [onRetry]. A failure after the request was
     * delivered (denial, timeout) is never retried, so the user is not shown a second dialog.
     */
    suspend fun pairByApproval(
        host: String,
        port: Int,
        deviceId: String,
        deviceName: String,
        onRetry: (attempt: Int, of: Int) -> Unit = { _, _ -> },
        onHostCertificate: (String) -> Unit,
    ): PairResponse = withContext(Dispatchers.IO) {
        var attempt = 1
        while (true) {
            val context = TrustStore.capturingPairingContext(onHostCertificate)
            val channel = OkHttpChannelBuilder.forAddress(host, port)
                .sslSocketFactory(context.socketFactory)
                .hostnameVerifier { _, _ -> true }
                .build()
            try {
                return@withContext PairServiceGrpcKt.PairServiceCoroutineStub(channel)
                    .withDeadlineAfter(75, TimeUnit.SECONDS)
                    .requestPair(
                        PairRequest.newBuilder()
                            .setRequestApproval(true)
                            .setInitiatorDeviceId(deviceId)
                            .setInitiatorCertPem("") // viewer-only client: no mTLS identity cert
                            .setInitiatorDisplayName(deviceName)
                            .build()
                    )
            } catch (e: io.grpc.StatusRuntimeException) {
                if (attempt >= CONNECT_ATTEMPTS || !isConnectFailure(e)) throw e
                attempt++
                onRetry(attempt, CONNECT_ATTEMPTS)
                kotlinx.coroutines.delay(CONNECT_RETRY_DELAY_MS)
            } finally {
                channel.shutdown()
            }
        }
        @Suppress("UNREACHABLE_CODE")
        throw IllegalStateException("unreachable")
    }

    const val CONNECT_ATTEMPTS = 6
    const val CONNECT_RETRY_DELAY_MS = 1_500L

    /**
     * True if [e] means the TCP/TLS connection could not be established at all (so the
     * request never reached the host and a retry is safe): status UNAVAILABLE caused by
     * a refused/unreachable/timed-out/unresolvable connect.
     */
    fun isConnectFailure(e: Throwable): Boolean {
        if (io.grpc.Status.fromThrowable(e).code != io.grpc.Status.Code.UNAVAILABLE) return false
        return generateSequence(e) { it.cause }.any {
            it is java.net.ConnectException ||
                it is java.net.NoRouteToHostException ||
                it is java.net.SocketTimeoutException ||
                it is java.net.UnknownHostException
        }
    }

    /**
     * Builds a data-plane channel: TLS pinned to the host certificate obtained at
     * pairing, authenticated by the `x-nexus-token` header. Callers own the
     * returned channel and must shut it down.
     */
    fun newDataChannel(host: String, certPem: String, authToken: String): ManagedChannel {
        val sslContext = TrustStore.sslContext(certPem)
        return OkHttpChannelBuilder.forAddress(host, DATA_PORT)
            .sslSocketFactory(sslContext.socketFactory)
            // Host identity is pinned in TrustStore (public-key match), so
            // hostname matching against the self-signed cert is redundant.
            .hostnameVerifier { _, _ -> true }
            .intercept(MetadataUtils.newAttachHeadersInterceptor(authHeaders(authToken)))
            .keepAliveTime(30, TimeUnit.SECONDS)
            .keepAliveTimeout(10, TimeUnit.SECONDS)
            .build()
    }

    /**
     * Opens the pinned-TLS data channel and starts the bidirectional stream.
     * Returns the host-to-viewer VideoFrame flow; errors surface to the caller
     * when the flow fails or completes.
     */
    suspend fun stream(
        host: String,
        certPem: String,
        authToken: String,
        requests: Flow<InputEvent>
    ): Flow<VideoFrame> = withContext(Dispatchers.IO) {
        val channel = newDataChannel(host, certPem, authToken)
        dataChannel?.shutdownNow()
        dataChannel = channel
        StreamServiceGrpcKt.StreamServiceCoroutineStub(channel).remoteControl(requests)
    }

    fun shutdown() {
        pairChannel?.shutdown()
        dataChannel?.shutdown()
    }

    private fun authHeaders(token: String): Metadata =
        Metadata().apply {
            put(Metadata.Key.of(AUTH_HEADER, Metadata.ASCII_STRING_MARSHALLER), token)
        }
}
