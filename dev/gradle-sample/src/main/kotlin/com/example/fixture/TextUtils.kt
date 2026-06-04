package com.example.fixture

/**
 * Kotlin stdlib fixture.
 *
 * Exercises `String` members (`uppercase`, `lowercase`, `trim`, `split`, `replace`), collection
 * operations (`map`, `filter`, `first`, `groupBy`, `associateBy`, `sumOf`), numeric work, and an
 * extension function. ktlsp should resolve all of these into the kotlin-stdlib sources jar.
 */

/** Extension function on String — exercises project-local extension indexing + stdlib `repeat`. */
fun String.shout(): String = trim().uppercase() + "!".repeat(3)

/** Normalize a free-form name: collapse whitespace, title-case each word. */
fun normalizeName(raw: String): String {
    val words: List<String> = raw.trim().split(Regex("\\s+"))
    return words.joinToString(" ") { word ->
        word.lowercase().replaceFirstChar { ch -> ch.uppercase() }
    }
}

/** Split a comma-separated tag string into a cleaned, de-duplicated list. */
fun parseTags(csv: String): List<String> =
    csv
        .split(",")
        .map { it.trim() }
        .filter { it.isNotEmpty() }
        .distinct()

/** Index users by their first tag (or "untagged"), exercising map/groupBy/associate. */
fun tagIndex(users: List<User>): Map<String, List<User>> =
    users.groupBy { user -> user.tags.firstOrNull() ?: "untagged" }

/** A by-id lookup map built with associateBy. */
fun byId(users: List<User>): Map<Long, User> = users.associateBy { it.id }

/** Numeric reduction over a list of ints. */
fun stats(numbers: List<Int>): String {
    val total: Int = numbers.sum()
    val mean: Double = if (numbers.isEmpty()) 0.0 else total.toDouble() / numbers.size
    val biggest: Int = numbers.maxOrNull() ?: 0
    val evens = numbers.filter { it % 2 == 0 }
    return "sum=$total mean=${"%.2f".format(mean)} max=$biggest evens=$evens"
}

fun firstActiveEmail(users: List<User>): String =
    users.first { it.active }.email.lowercase()
