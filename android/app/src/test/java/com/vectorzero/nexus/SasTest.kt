package com.vectorzero.nexus

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

class SasTest {
    private val device = "3f9a21bc-0000-4000-8000-000000000001"
    private val fingerprint = "ab".repeat(32)

    @Test fun matchesTheVectorAssertedByTheRustHost() {
        // Same vector as `short_auth_string_matches_the_shared_test_vector` in approval.rs.
        assertEquals("0637", Sas.compute(device, fingerprint))
    }

    @Test fun dependsOnBothInputs() {
        val base = Sas.compute(device, fingerprint)
        assertNotEquals(base, Sas.compute(device, "cd".repeat(32)))
        assertNotEquals(base, Sas.compute("3f9a21bc-0000-4000-8000-000000000002", fingerprint))
    }

    @Test fun isAlwaysFourDigits() {
        for (i in 0 until 200) {
            val code = Sas.compute("device-$i", fingerprint)
            assertEquals(4, code.length)
            assertEquals(true, code.all { it.isDigit() })
        }
    }
}
