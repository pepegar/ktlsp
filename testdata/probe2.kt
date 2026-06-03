fun scopes(flag: Boolean, items: List<Int>) {
    val outer = 1
    if (flag) {
        val inner = 2
        println(inner + outer)
    }
    while (flag) {
        val w = 3
        println(w)
    }
    for (i in items) {
        println(i)
    }
    run {
        val lam = 4
        println(lam)
    }
    val (a, b) = Pair(1, 2)
    println(a + b)
    when (val s = outer) {
        else -> println(s)
    }
}
