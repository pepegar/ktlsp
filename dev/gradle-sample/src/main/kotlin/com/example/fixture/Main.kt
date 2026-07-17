package com.example.fixture

import kotlinx.coroutines.runBlocking

/**
 * Entry point that ties every fixture file together, so the whole graph of project + library
 * symbols is reachable from one place. Touches serialization, coroutines, stdlib, okio, and the
 * project-local greeter hierarchy.
 */
fun main() {
    // --- serialization (Model.kt) ---
    val user: User = sampleUser()
    val encoded: String = encodeUser(user)
    println("encoded user:\n$encoded")
    val decoded: User = decodeUser(encoded)
    check(decoded == user)

    // --- stdlib (TextUtils.kt) ---
    println(normalizeName("  ada   LOVELACE "))
    println(parseTags("math, engines, , math, history"))
    println(stats(listOf(1, 2, 3, 4, 5, 6)))
    println("ktlsp".shout())

    // --- okio (Storage.kt) ---
    val payload = bufferedJson(user)
    println("payload sha256: ${digest(payload.utf8())}")
    println("config path: ${configPath("settings.json")}")

    // --- project-local types (Greetings.kt) ---
    val greeters: List<Greeter> =
        listOf(BasicGreeter.default(), ShoutingGreeter(), greeterFor("es"))
    greeters.forEach { greeter -> println(greeter.greet(user)) }
    println(ShoutingGreeter().greetLoudly(user))

    // --- coroutines (Concurrency.kt) ---
    val users: List<User> = runBlocking { fetchAll(listOf(10L, 20L, 30L)) }
    println("fetched ${users.size} users")
    runConcurrencyDemo()
}
