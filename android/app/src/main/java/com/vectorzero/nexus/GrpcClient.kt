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
            val channel = OkHttpChannelBuilder.forAddress(host, PAIR_PORT)
                .sslSocketFactory(TrustStore.pairingContext().socketFactory)
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
        val sslContext = TrustStore.sslContext(certPem)
        val channel = OkHttpChannelBuilder.forAddress(host, DATA_PORT)
            .sslSocketFactory(sslContext.socketFactory)
            // Host identity is pinned in TrustStore (public-key match), so
            // hostname matching against the self-signed cert is redundant.
            .hostnameVerifier { _, _ -> true }
            .intercept(MetadataUtils.newAttachHeadersInterceptor(authHeaders(authToken)))
            .keepAliveTime(30, TimeUnit.SECONDS)
            .keepAliveTimeout(10, TimeUnit.SECONDS)
            .build()
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
