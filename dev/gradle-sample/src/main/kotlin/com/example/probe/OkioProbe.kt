package com.example.probe

import com.example.fixture.User
import com.example.fixture.sampleUser
import okio.Buffer
import okio.ByteString
import okio.ByteString.Companion.encodeUtf8
import okio.ByteString.Companion.toByteString
import okio.ByteString.Companion.decodeBase64
import okio.Path
import okio.Path.Companion.toPath
import kotlinx.coroutines.flow.Flow // intentionally unused: exercises the unused-import diagnostic

// Okio live-verification probe. Each marker line below is TEXTUALLY UNIQUE so the headless harness
// can `find_line` it unambiguously, then request completion at the end of the partial selector.
// All signatures were confirmed against the extracted okio 3.9.1 sources under ~/.cache/ktlsp.

// --- Member completion on Buffer (BufferedSink/BufferedSource members) ---

fun _okBufferWrite() {
    val bufW = Buffer()
    bufW.writeUtf // -> writeUtf8
}

fun _okBufferRead() {
    val bufR = Buffer()
    bufR.readUtf // -> readUtf8 (inherited from BufferedSource)
}

fun _okBufferSnapshot() {
    val bufS = Buffer()
    bufS.snaps // -> snapshot
}

fun _okBufferSize() {
    val bufZ = Buffer()
    bufZ.siz // -> size (var size: Long)
}

// --- Member completion on ByteString ---

fun _okByteStringUtf8() {
    val bsU: ByteString = "x".encodeUtf8()
    bsU.utf // -> utf8
}

fun _okByteStringHex() {
    val bsH: ByteString = "x".encodeUtf8()
    bsH.he // -> hex
}

fun _okByteStringSha256() {
    val bsS: ByteString = "x".encodeUtf8()
    bsS.sha2 // -> sha256
}

fun _okByteStringBase64() {
    val bsB: ByteString = "x".encodeUtf8()
    bsB.base // -> base64
}

// --- Member completion on Path ---

fun _okPathResolve() {
    val pthR: Path = "/etc/ktlsp".toPath()
    pthR.resol // -> resolve
}

fun _okPathParent() {
    val pthP: Path = "/etc/ktlsp".toPath()
    pthP.paren // -> parent
}

fun _okPathSegments() {
    val pthS: Path = "/etc/ktlsp".toPath()
    pthS.segme // -> segments
}

// --- Construction + chained calls (inference through okio method-chains) ---

fun _okChainSha256Hex() {
    // "x".encodeUtf8() -> ByteString; .sha256() -> ByteString; result of .hex chain is String member
    "payload".encodeUtf8().sha256().he // -> hex (chain returns ByteString, hex on it)
}

fun _okChainByteArray() {
    val raw = byteArrayOf(1, 2, 3)
    raw.toByteString().he // -> hex (ByteArray.toByteString() -> ByteString)
}

fun _okChainBase64Nullable() {
    // decodeBase64() returns ByteString? — completion after ?. should still offer ByteString members
    "aGk=".decodeBase64()?.utf // -> utf8 (safe-call on nullable ByteString?)
}

fun _okChainPathResolveParent() {
    // toPath() -> Path; resolve(..) -> Path; parent is a Path? property
    "/var".toPath().resolve("log").paren // -> parent
}

fun _okBufferChainSnapshotHex() {
    // Buffer().snapshot() -> ByteString; .hex chain
    Buffer().snapshot().he // -> hex
}

// --- Goto-definition targets (into okio sources jar) ---

fun _okGotoBuffer(): Buffer = Buffer()

fun _okGotoByteString(): ByteString = "g".encodeUtf8()

fun _okGotoPath(): Path = "/g".toPath()

// --- Cross-fixture: okio buffer fed from a fixture User (inference through fixture + okio) ---

fun _okFixtureBridge() {
    val u: User = sampleUser()
    val buf2 = Buffer()
    buf2.writeUtf8(u.name)
    buf2.snapshot().sha2 // -> sha256
}
