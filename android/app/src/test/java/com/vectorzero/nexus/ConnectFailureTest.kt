package com.vectorzero.nexus

import io.grpc.Status
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.SocketTimeoutException
import java.net.UnknownHostException

class ConnectFailureTest {
    private fun unavailable(cause: Throwable?) =
        Status.UNAVAILABLE.withCause(cause).asRuntimeException()

    @Test fun connectLevelFailuresAreRetriable() {
        for (cause in listOf(
            NoRouteToHostException("Host unreachable"),
            ConnectException("Connection refused"),
            SocketTimeoutException("connect timed out"),
            UnknownHostException("nope"),
        )) {
            assertTrue(cause.javaClass.simpleName, GrpcClient.isConnectFailure(unavailable(cause)))
        }
    }

    @Test fun findsTheCauseDeepInTheChain() {
        val nested = RuntimeException("wrapper", java.io.IOException("io", NoRouteToHostException("x")))
        assertTrue(GrpcClient.isConnectFailure(unavailable(nested)))
    }

    @Test fun otherFailuresAreNotRetried() {
        // The request may have been delivered: a retry could show a second dialog.
        assertFalse(GrpcClient.isConnectFailure(unavailable(java.io.IOException("connection reset"))))
        assertFalse(GrpcClient.isConnectFailure(unavailable(null)))
        assertFalse(GrpcClient.isConnectFailure(Status.DEADLINE_EXCEEDED.withCause(ConnectException()).asRuntimeException()))
        assertFalse(GrpcClient.isConnectFailure(Status.PERMISSION_DENIED.asRuntimeException()))
        assertFalse(GrpcClient.isConnectFailure(IllegalStateException("not grpc")))
    }

    @Test fun retryBudgetIsBoundedAndBrief() {
        assertEquals(6, GrpcClient.CONNECT_ATTEMPTS)
        assertTrue("worst-case waiting stays under 15 s", GrpcClient.CONNECT_RETRY_DELAY_MS * GrpcClient.CONNECT_ATTEMPTS <= 15_000)
    }
}
