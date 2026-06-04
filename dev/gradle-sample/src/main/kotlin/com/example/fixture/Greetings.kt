package com.example.fixture

/**
 * Project-local types fixture (Volatile-tier indexing + type-inference material).
 *
 * Exercises an interface, an `open class` with an `override`, a subclass extending it, properties
 * with explicit types, functions with explicit return types, and a `companion object`. These have
 * no external dependency — they're here so goto-definition and completion have project-owned
 * symbols to resolve, distinct from library symbols.
 */

/** A thing that can greet a named user. */
interface Greeter {
    val locale: String

    fun greet(user: User): String
}

/** Base greeter with an overridable salutation. Open so subclasses can specialize it. */
open class BasicGreeter(override val locale: String = "en") : Greeter {

    /** Subclasses override this to change the leading word. */
    open fun salutation(): String = "Hello"

    override fun greet(user: User): String {
        val name: String = user.name
        return "${salutation()}, $name"
    }

    companion object {
        const val DEFAULT_LOCALE: String = "en"

        fun default(): BasicGreeter = BasicGreeter(DEFAULT_LOCALE)
    }
}

/** A louder greeter — subclass overriding [salutation] and reusing the base [greet]. */
class ShoutingGreeter(locale: String = "en") : BasicGreeter(locale) {

    override fun salutation(): String = "HEY"

    /** Combines the inherited greet with the project-local String.shout() extension. */
    fun greetLoudly(user: User): String = greet(user).shout()
}

/** A localized greeter that ignores the base salutation entirely. */
class SpanishGreeter : Greeter {
    override val locale: String = "es"

    override fun greet(user: User): String = "Hola, ${user.name}"
}

/** Pick a greeter for a locale — explicit return type is the supertype [Greeter]. */
fun greeterFor(locale: String): Greeter =
    when (locale.lowercase()) {
        "es" -> SpanishGreeter()
        else -> BasicGreeter(locale)
    }
