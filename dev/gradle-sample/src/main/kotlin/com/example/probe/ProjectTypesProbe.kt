package com.example.probe

// PROJECT TYPES + INFERENCE DEPTH probe (Volatile-tier project symbols).
//
// Cross-package: this file is in com.example.probe; the types live in com.example.fixture.
// The headless harness requests completion at the END of each partial selector and asserts the
// expected member is offered. EVERY cursor-marker line is textually UNIQUE (distinct receiver
// names / member prefixes) so the substring-based find_line matches the intended line.

import com.example.fixture.BasicGreeter
import com.example.fixture.ShoutingGreeter
import com.example.fixture.SpanishGreeter
import com.example.fixture.Greeter
import com.example.fixture.greeterFor
import com.example.fixture.shout            // project-local extension String.shout
import com.example.fixture.User
import com.example.fixture.normalizeName    // INTENTIONALLY UNUSED: unused-import diagnostic target

// ---- member completion on project types ------------------------------------------------------

fun probeBasicOwnMember() {
    val basic = BasicGreeter()
    basic.salu                                 // own member: salutation
}

fun probeBasicInheritedMember() {
    val basicInh = BasicGreeter()
    basicInh.loca                              // inherited-from-interface property: locale
}

fun probeBasicGreetMember() {
    val basicGreet = BasicGreeter()
    basicGreet.gree                            // interface method via supertype walk: greet
}

fun probeShoutingOwn() {
    val shouter = ShoutingGreeter()
    shouter.greetLou                           // own member of ShoutingGreeter: greetLoudly
}

fun probeShoutingInheritedFromBase() {
    val shouterBase = ShoutingGreeter()
    shouterBase.salu                           // inherited from BasicGreeter (override): salutation
}

fun probeShoutingInheritedFromInterface() {
    val shouterIface = ShoutingGreeter()
    shouterIface.loca                          // inherited transitively from Greeter: locale
}

// ---- companion / static access ---------------------------------------------------------------

fun probeCompanionFun() {
    BasicGreeter.defa                          // companion function: default
}

fun probeCompanionConst() {
    BasicGreeter.DEFAULT_LOC                    // companion const: DEFAULT_LOCALE
}

// ---- chained calls ---------------------------------------------------------------------------

fun probeChainCompanionToMember() {
    BasicGreeter.default().salu                // default() -> BasicGreeter; then .salutation
}

fun probeChainGreeterForMember() {
    greeterFor("en").gree                      // greeterFor -> Greeter; then .greet
}

// ---- extension function (project-local String.shout from TextUtils) --------------------------

fun probeExtensionOnString() {
    val plain = "hello"
    plain.sho                                  // project extension: shout
}

fun probeExtensionAfterChain() {
    val word = "  hi  "
    word.trim().sho                            // chained stdlib trim() then project extension shout
}

// ---- smart casts -----------------------------------------------------------------------------

fun probeSmartCastIfIs(anyShout: Any) {
    if (anyShout is ShoutingGreeter) {
        anyShout.greetLou                      // smart-cast inside if-is: greetLoudly
    }
}

fun probeSmartCastWhenIs(anyWhen: Any) {
    when (anyWhen) {
        is ShoutingGreeter -> anyWhen.greetLou // smart-cast inside when is-branch: greetLoudly
        else -> {}
    }
}

fun probeSmartCastAsCast(anyAs: Any) {
    val casted = anyAs as ShoutingGreeter
    casted.greetLou                            // explicit as-cast then member: greetLoudly
}

fun probeSmartCastEarlyReturn(anyEarly: Any) {
    if (anyEarly !is ShoutingGreeter) return
    anyEarly.greetLou                          // negated is + early return narrows: greetLoudly
}

// ---- scope functions -------------------------------------------------------------------------

fun probeLetIt() {
    greeterFor("en").let { itGreeter ->
        itGreeter.gree                         // let { } receiver-via-param is Greeter: greet
    }
}

fun probeLetItImplicit() {
    greeterFor("es").let {
        it.loca                                // let { } implicit it is Greeter: locale
    }
}

fun probeAlsoIt() {
    val alsoBasic = BasicGreeter()
    alsoBasic.also { itAlso ->
        itAlso.salu                            // also { } it is BasicGreeter: salutation
    }
}

fun probeApplyThis() {
    BasicGreeter().apply {
        this.salu                              // apply { } this-receiver is BasicGreeter: salutation
    }
}

// ---- find-references target ------------------------------------------------------------------
// SpanishGreeter is used here in the probe AND in Greetings.kt (greeterFor). find-references on the
// declaration should surface both files.
fun probeUsesSpanishGreeter(): Greeter = SpanishGreeter()

// A second cross-file reference to BasicGreeter for find-references breadth.
fun probeAnotherBasicRef(): BasicGreeter = BasicGreeter()

// User is referenced to keep that import "used" (no false-positive unused-import expected).
fun probeUserRef(u: User): String = u.name

// Genuine full uses of the `shout` extension so its import is truly used (the `.sho` partial
// selectors above are truncated for completion and don't count as a real reference). This makes the
// unused-import false-positive check meaningful.
fun probeShoutFullUse(): String = "real".shout()
