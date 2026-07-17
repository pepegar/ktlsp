package com.example.fixture

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext

/**
 * kotlinx.coroutines fixture.
 *
 * Exercises `suspend fun`, `Flow`/`flow`, `launch`, `async`/`awaitAll`, `delay`, `runBlocking`,
 * `withContext`, and `Dispatchers`. ktlsp should resolve all of these into the
 * kotlinx-coroutines-core sources jar (`delay`, `Flow`, `launch`, etc.).
 */

/** A suspending lookup with an explicit suspend modifier and explicit return type. */
suspend fun fetchUser(id: Long): User {
    delay(10)
    return sampleUser().copy(id = id)
}

/** Emit a stream of users as a cold [Flow]. */
fun userFlow(ids: List<Long>): Flow<User> =
    flow {
        for (id in ids) {
            delay(5)
            emit(fetchUser(id))
        }
    }

/** Collect a flow of users, mapping each to its display name. */
suspend fun displayNames(ids: List<Long>): List<String> =
    userFlow(ids)
        .map { user -> user.name.uppercase() }
        .toList()

/** Fan out concurrent fetches with async/awaitAll inside a structured scope. */
suspend fun fetchAll(ids: List<Long>): List<User> = coroutineScope {
    ids
        .map { id -> async(Dispatchers.Default) { fetchUser(id) } }
        .awaitAll()
}

/** Offload CPU-ish work onto the default dispatcher. */
suspend fun summarize(users: List<User>): String = withContext(Dispatchers.Default) {
    users.joinToString(separator = ", ") { it.name }
}

/** Top-level blocking entry that drives launch + the suspend helpers above. */
fun runConcurrencyDemo(): List<User> = runBlocking {
    val ids: List<Long> = listOf(1L, 2L, 3L)

    launch {
        val names = displayNames(ids)
        println("names: $names")
    }

    val users = fetchAll(ids)
    println(summarize(users))
    users
}
