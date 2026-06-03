package com.example.app

import com.other.Helper
import com.other.util.format as fmt
import com.other.*

interface Named {
    val label: String
    fun describe(): String
}

enum class Color { RED, GREEN, BLUE }

object Registry {
    val items: MutableList<String> = mutableListOf()
    fun add(item: String) { items.add(item) }
}

class Greeter(val name: String, private var count: Int) : Named {
    companion object {
        const val DEFAULT = "world"
        fun create(): Greeter = Greeter(DEFAULT, 0)
    }

    override val label: String = name
    override fun describe(): String = "Greeter($name)"

    fun <T> greetAll(items: List<T>, prefix: String): Int {
        for (item in items) {
            val line = "$prefix $item"
            println(line)
        }
        return items.size
    }

    constructor(name: String) : this(name, 0)
}

fun String.shout(): String = this.uppercase()

fun main(args: Array<String>) {
    val g = Greeter("hi", 1)
    val n = g.name
    g.greetAll(listOf("a", "b"), "x")
    val result = when (n.length) {
        0 -> "empty"
        else -> n.shout()
    }
    listOf(1, 2, 3).map { x -> x * 2 }
    println(result)
}
