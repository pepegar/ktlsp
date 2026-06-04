package com.example.fixture

// Live-verification probe (not part of the real fixture API). Each line below exercises a specific
// inference path; the headless harness requests completion at the end of the partial selector and
// asserts the expected member is offered.

fun _probeMembers() {
    val g = BasicGreeter()
    g.gr
}

fun _probeReturnType() {
    greeterFor("en").gr
}

fun _probeCompanion() {
    BasicGreeter.def
}

fun _probeChain() {
    BasicGreeter.default().sal
}

fun _probeStdlibString() {
    val s = "hello"
    s.upper
}

fun _probeStdlibType(): String = ""
