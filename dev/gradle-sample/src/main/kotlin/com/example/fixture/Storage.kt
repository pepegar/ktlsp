package com.example.fixture

import okio.Buffer
import okio.ByteString
import okio.ByteString.Companion.encodeUtf8
import okio.ByteString.Companion.toByteString
import okio.Path
import okio.Path.Companion.toPath

/**
 * Okio (com.squareup.okio:okio) fixture — the fourth dependency.
 *
 * Exercises distinctive okio types: [Buffer], [ByteString], [Path], plus the `encodeUtf8`,
 * `toByteString`, `decodeBase64`, and `toPath` extensions. ktlsp should resolve every one of
 * these into the okio sources jar. All APIs used here were verified against okio 3.9.1 sources.
 */

/** Encode a string as a UTF-8 [ByteString] (okio extension on String). */
fun toBytes(text: String): ByteString = text.encodeUtf8()

/** Hex-encode arbitrary bytes by wrapping them in an okio [ByteString]. */
fun hexOf(bytes: ByteArray): String = bytes.toByteString().hex()

/** Decode a Base64 string into bytes, or null if it isn't valid Base64. */
fun fromBase64(encoded: String): ByteString? = encoded.decodeBase64()

/** Build up a JSON payload in an okio [Buffer] and snapshot it as an immutable [ByteString]. */
fun bufferedJson(user: User): ByteString {
    val buffer = Buffer()
    buffer.writeUtf8("{\"user\":")
    buffer.writeUtf8(encodeUser(user))
    buffer.writeUtf8("}")
    return buffer.snapshot()
}

/** Read a buffer back out as a UTF-8 string. */
fun drain(buffer: Buffer): String = buffer.readUtf8()

/** Parse a filesystem path with okio's platform-independent [Path]. */
fun configPath(name: String): Path {
    val base: Path = "/etc/ktlsp".toPath()
    return base.resolve(name)
}

/** SHA-256 a string using okio's ByteString digest helpers. */
fun digest(text: String): String = text.encodeUtf8().sha256().hex()
