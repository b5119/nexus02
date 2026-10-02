package com.vectorzero.nexus.documents

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class DocIdTest {
    @Test fun parsesHostAndPath() {
        val id = DocId.parse("9f2c:/photos/a.jpg")
        assertEquals("9f2c", id.hostId)
        assertEquals("/photos/a.jpg", id.path)
        assertEquals("a.jpg", id.name)
        assertEquals("9f2c:/photos/a.jpg", id.encoded)
    }

    @Test fun rootRoundTrips() {
        val root = DocId.root("h")
        assertTrue(root.isRoot)
        assertEquals("h:/", root.encoded)
        assertEquals(root, DocId.parse("h:/"))
        assertEquals("", root.name)
    }

    @Test fun collapsesEmptySegmentsAndTrailingSlash() {
        assertEquals("/a/b", DocId.parse("h://a///b/").path)
    }

    @Test fun rejectsTraversal() {
        for (bad in listOf("h:/../etc", "h:/a/../b", "h:/./a", "h:/a/..")) {
            try {
                DocId.parse(bad)
                fail("expected rejection of $bad")
            } catch (_: IllegalArgumentException) {
            }
        }
    }

    @Test fun rejectsMalformed() {
        for (bad in listOf("", "nocolon", ":/a", "h:", "h:relative")) {
            try {
                DocId.parse(bad)
                fail("expected rejection of '$bad'")
            } catch (_: IllegalArgumentException) {
            }
        }
    }

    @Test fun childBuildsPaths() {
        assertEquals("/a", DocId.root("h").child("a").path)
        assertEquals("/a/b", DocId.root("h").child("a").child("b").path)
    }

    @Test fun childRejectsBadNames() {
        for (bad in listOf("", "a/b", ".", "..")) {
            try {
                DocId.root("h").child(bad)
                fail("expected rejection of '$bad'")
            } catch (_: IllegalArgumentException) {
            }
        }
    }

    @Test fun descendantChecksComponentsNotPrefixes() {
        val dir = DocId.parse("h:/photos")
        assertTrue(DocId.parse("h:/photos/a.jpg").isDescendantOf(dir))
        assertTrue(DocId.parse("h:/photos/2026/a.jpg").isDescendantOf(dir))
        assertFalse("sibling with shared prefix", DocId.parse("h:/photos2/a.jpg").isDescendantOf(dir))
        assertFalse("itself", dir.isDescendantOf(dir))
        assertFalse("other host", DocId.parse("x:/photos/a.jpg").isDescendantOf(dir))
        assertTrue("everything is below the root", DocId.parse("h:/a").isDescendantOf(DocId.root("h")))
        assertFalse("root is not below itself", DocId.root("h").isDescendantOf(DocId.root("h")))
    }
}
