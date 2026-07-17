package com.example.fixture

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.encodeToString

/**
 * kotlinx.serialization fixture.
 *
 * Exercises `@Serializable`, typed properties, nested serializable types, enums, and the
 * `Json.encodeToString` / `Json.decodeFromString` round-trip. ktlsp should resolve `Json`,
 * `encodeToString`, and `decodeFromString` into the kotlinx-serialization-json sources jar,
 * and `Serializable`/`SerialName` into the serialization-core sources it transitively pulls.
 */

@Serializable
enum class Role {
    @SerialName("admin")
    ADMIN,

    @SerialName("member")
    MEMBER,

    @SerialName("guest")
    GUEST,
}

@Serializable
data class Address(
    val street: String,
    val city: String,
    val postalCode: String,
    val country: String = "ES",
)

@Serializable
data class User(
    val id: Long,
    val name: String,
    val email: String,
    val role: Role = Role.MEMBER,
    val active: Boolean = true,
    val tags: List<String> = emptyList(),
    val address: Address? = null,
)

/** Pretty-printing, lenient JSON used across the fixture. */
val json: Json = Json {
    prettyPrint = true
    ignoreUnknownKeys = true
    encodeDefaults = true
}

/** Serialize a [User] to a JSON string. Return type is explicit on purpose (type-inference test). */
fun encodeUser(user: User): String = json.encodeToString(user)

/** Parse a [User] back out of a JSON string. */
fun decodeUser(text: String): User = json.decodeFromString<User>(text)

/** Round-trip a list of users through JSON and back. */
fun roundTrip(users: List<User>): List<User> {
    val encoded: String = json.encodeToString(users)
    return json.decodeFromString(encoded)
}

fun sampleUser(): User =
    User(
        id = 7L,
        name = "Ada Lovelace",
        email = "ada@example.com",
        role = Role.ADMIN,
        tags = listOf("math", "engines"),
        address =
            Address(
                street = "12 Analytical Way",
                city = "London",
                postalCode = "EC1A",
                country = "UK",
            ),
    )
