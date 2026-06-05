package com.example.probe

import com.example.fixture.BasicGreeter
import com.example.fixture.User
import com.example.fixture.sampleUser

/**
 * KOTLIN STDLIB live-verification probe.
 *
 * Each `_sx*` function exercises one inference path. The harness requests completion at the END of
 * the partial selector on a TEXTUALLY-UNIQUE marker line and asserts the expected member is offered.
 * Distinct receiver names (sx1, sx2, ...) keep find_line's substring match from colliding.
 *
 * Goto-definition targets live on dedicated lines with unique surrounding text.
 */

// --- String members / extensions ---------------------------------------------------------------

fun _sxStringUppercase() {
    val sx1 = "hello"
    sx1.upper
}

fun _sxStringTrim() {
    val sx2 = "  padded  "
    sx2.tri
}

fun _sxStringIsBlank() {
    val sx3 = "  "
    sx3.isBla
}

fun _sxStringLength() {
    val sx4 = "abc"
    sx4.leng
}

fun _sxStringSplitChain() {
    // split returns List<String>; first() -> String; String member completion through the chain
    val sx5 = "a,b,c"
    sx5.split(",").first().upper
}

fun _sxStringReplace() {
    val sx6 = "a-b"
    sx6.repla
}

fun _sxStringSubstring() {
    val sx7 = "abcdef"
    sx7.subs
}

// --- Collection operations ---------------------------------------------------------------------

fun _sxListFirstMember() {
    // listOf(User) -> List<User>; first() -> User; User member completion (project type element)
    val sx8 = listOf(sampleUser())
    sx8.first().ema
}

fun _sxListFirstOrNullSafeCall() {
    // firstOrNull() -> User?; safe-call member completion on a nullable receiver
    val sx9 = listOf(sampleUser())
    sx9.firstOrNull()?.ema
}

fun _sxListMapLambdaIt() {
    // map { it } where it : User -> member completion on the lambda parameter
    val sxA = listOf(sampleUser())
    sxA.map { ititem ->
        ititem.ema
    }
}

fun _sxListFilterChain() {
    // filter returns List<User>; first() -> User; member completion after a filter chain
    val sxB = listOf(sampleUser())
    sxB.filter { it.active }.first().ema
}

fun _sxMapMember() {
    // associateBy -> Map<Long, User>; Map member completion (keys/values/entries)
    val sxC = listOf(sampleUser())
    sxC.associateBy { it.id }.ke
}

fun _sxGroupByMember() {
    // groupBy -> Map<String, List<User>>; Map member completion
    val sxD = listOf(sampleUser())
    sxD.groupBy { it.name }.val
}

fun _sxJoinToString() {
    val sxE = listOf(sampleUser())
    sxE.joinToSt
}

fun _sxSumOf() {
    val sxF = listOf(sampleUser())
    sxF.sumO
}

// --- Numbers / ranges --------------------------------------------------------------------------

fun _sxNumberToLong() {
    val sxG = "42"
    sxG.toLo
}

fun _sxNumberCoerceIn() {
    val sxH = 5
    sxH.coerc
}

// --- Nullability ------------------------------------------------------------------------------

fun _sxElvisDefault() {
    // address is User.address: Address?; elvis -> Address (non-null); but here test ?: on String?
    val sxI: String? = null
    val sxJ = sxI ?: "fallback"
    sxJ.upper
}

fun _sxNotNullAssert() {
    val sxK: User? = sampleUser()
    sxK!!.ema
}

fun _sxSafeCallChainNonNull() {
    // address?.city -> String?; then ?.uppercase chain
    val sxL = sampleUser()
    sxL.address?.city?.upper
}

// --- Goto-definition targets ------------------------------------------------------------------

// goto on `String` return type -> kotlin-stdlib String.kt.
// NOTE: fn name must NOT contain the substring "String" or the harness targets the name token.
fun _sxGotoReturnType(): String = ""

// goto on `listOf` call -> kotlin-stdlib Collections.kt
fun _sxGotoListOf() {
    val sxGotoListOf = listOf(1, 2, 3)
}

// goto on a project type used as element -> Model.kt (User).
// NOTE: var name avoids the substring "User" so the harness can target the TYPE token cleanly.
fun _sxGotoProjectType() {
    val gotoTargetVar: User = sampleUser()
}

// find-references anchor: User is referenced across the fixture + this probe
fun _sxRefsAnchor(u: User): String = u.email

// --- Isolation probes: project-type element via constructor vs function call ---------------------

fun _sxListCtorElem() {
    // listOf(User(...)) via CONSTRUCTOR (mirrors reference harness's BasicGreeter()).
    val sxM = listOf(User(1L, "a", "a@a"))
    sxM.first().ema
}

fun _sxListGreeterElem() {
    // Exactly the reference-harness shape but inline (BasicGreeter element).
    val sxN = listOf(BasicGreeter())
    sxN.first().sal
}

fun _sxUserDirectMember() {
    // Direct local of project type (no generics): does plain member completion work?
    val sxO = sampleUser()
    sxO.ema
}

fun _sxUserCtorDirectMember() {
    val sxP = User(2L, "b", "b@b")
    sxP.ema
}

// goto on a project type via constructor call -> Model.kt.
// NOTE: var name avoids "User" so the harness targets the CTOR's type token.
fun _sxGotoUserCtor() {
    val gotoCtorVar = User(3L, "c", "c@c")
}

// --- Discriminator probes: data class vs @Serializable vs plain class --------------------------

/** Plain data class, NO annotations, defined in THIS probe file. */
data class PlainDc(val pid: Long, val pname: String) {
    fun pmethod(): String = pname
}

/** Plain (non-data) class defined in THIS probe file. */
class PlainClass(val cid: Long) {
    fun cmethod(): String = "x"
}

fun _sxPlainDcMember() {
    val sxQ = PlainDc(1L, "n")
    sxQ.pna
}

fun _sxPlainDcMethod() {
    // body-declared METHOD on a data class (not a constructor-param val)
    val sxS = PlainDc(1L, "n")
    sxS.pmeth
}

/** Regular (non-data) class whose properties are declared in the PRIMARY CONSTRUCTOR. */
class CtorPropClass(val xprop: Long, val yprop: String)

fun _sxCtorPropMember() {
    // ctor-param val on a NON-data class: isolates "data" modifier vs "ctor-param val".
    val sxT = CtorPropClass(1L, "n")
    sxT.xpr
}

fun _sxGreeterCtorVal() {
    // BasicGreeter has `override val locale` as a ctor-param val (non-data class).
    val sxU = BasicGreeter()
    sxU.loc
}

fun _sxPlainClassMember() {
    val sxR = PlainClass(1L)
    sxR.cme
}

// goto on a same-file plain data class -> StdlibProbe.kt (self file is acceptable here)
fun _sxGotoPlainDc() {
    val sxGotoPlainDc = PlainDc(9L, "z")
}
