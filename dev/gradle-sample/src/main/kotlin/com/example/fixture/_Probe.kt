package com.example.fixture

import kotlinx.coroutines.delay // intentionally unused: exercises the unused-import diagnostic

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

// --- Gradual-checker (type-inference) probes ---

fun _probeSmartCast(x: Any) {
    if (x is BasicGreeter) {
        x.sal
    }
}

fun _probeEarlyReturn(y: Any) {
    if (y !is BasicGreeter) return
    y.sal
}

fun _probeGenericChain() {
    // listOf(BasicGreeter()) -> List<BasicGreeter>; first() -> BasicGreeter
    listOf(BasicGreeter()).first().sal
}

fun _probeLambdaIt() {
    listOf(BasicGreeter()).map {
        it.sal
    }
}
