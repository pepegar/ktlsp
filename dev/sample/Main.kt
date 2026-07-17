package demo

/** Helpful hover docs. */
fun helper() = Greeter("helper")

fun showGreeter(greeter: Greeter) = greeter

class TraceSpan {
    fun setTag() {}
}

fun withGreeter(greeter: Greeter, block: Greeter.() -> Greeter) {
    block(greeter)
}

fun span(block: TraceSpan.() -> TraceSpan) {
    block(TraceSpan())
}

fun main() {
    val g = Greeter("world")
    showGreeter(g.greet())
    val potato = ""
    withGreeter(g) {
        greet()
        this
    }
    span {
        setTag()
        this
    }
    showGreeter(helper())
}
