package app

class Greeter(val name: String) {
    fun greet(): String = name
    fun self(): String = this.greet()
}

fun demo(p: Greeter) {
    val a: Greeter = p
    val b = Greeter("x")
    val c: Greeter? = null
    a.greet()
    b.greet()
    p.greet()
}
