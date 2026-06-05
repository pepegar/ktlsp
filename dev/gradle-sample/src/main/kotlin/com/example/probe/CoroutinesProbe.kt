package com.example.probe

import com.example.fixture.User
import com.example.fixture.fetchUser
import com.example.fixture.sampleUser
import com.example.fixture.userFlow
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext
import kotlinx.coroutines.coroutineScope

// Coroutines live-verification probe (not part of the real fixture API). Each marker line below is
// TEXTUALLY UNIQUE and exercises one inference / completion / goto path for kotlinx-coroutines-core.
// The headless harness requests completion at the END of each partial selector.

// --- 1) Builder completion on a CoroutineScope (launch / async / coroutineContext) ---
fun probeScopeBuilders() = runBlocking {
    // `this` is a CoroutineScope here. Partial "lau" should offer launch.
    val scopeLaunch = this.lau
    val scopeAsync = this.asy
    val scopeCtx = this.coroutineCon
    Unit
}

// --- 2) launch returns a Job; member completion on a Job value ---
fun probeJobMembers() = runBlocking {
    val jobValue: Job = launch { delay(1) }
    jobValue.isAct
    jobValue.canc
    jobValue.joi
    Unit
}

// --- 3) async returns Deferred<T>; await() infers T, plus Deferred members ---
fun probeDeferred() = runBlocking {
    val deferredUser: Deferred<User> = async { fetchUser(1L) }
    deferredUser.awa
    Unit
}

// --- 4) await() result type inference: Deferred<User>.await() -> User member access ---
fun probeAwaitInference() = runBlocking {
    val awaitedUser = async { fetchUser(2L) }.await()
    awaitedUser.nam
    Unit
}

// --- 5) Dispatchers object member completion ---
fun probeDispatchers() {
    val dispatcherDefault = Dispatchers.Defau
    val dispatcherIo = Dispatchers.I
    val dispatcherMain = Dispatchers.Mai
    val dispatcherUnconfined = Dispatchers.Unconf
}

// --- 6) Flow value member completion (map / filter / collect on a Flow) ---
fun probeFlowOps() {
    val numberFlow: Flow<Int> = flow {
        emit(1)
        emit(2)
    }
    numberFlow.ma
    numberFlow.filt
    numberFlow.collec
}

// --- 7) Flow.map transform-lambda `it` element type inference (Flow<User>.map { it.<member> }) ---
fun probeFlowMapLambda() {
    userFlow(listOf(1L)).map { flowedUser ->
        flowedUser.ema
    }
}

// --- 8) Chained Flow operator result inference: Flow<User>.map{}.filter{} -> still Flow, toList ---
suspend fun probeFlowChain(): List<String> =
    userFlow(listOf(1L, 2L))
        .map { mappedUser -> mappedUser.name }
        .filter { mappedName -> mappedName.isNotEmpty() }
        .toL

// --- 9) toList() terminal infers List<String>; first() element access ---
suspend fun probeFlowTerminalInference() {
    val collectedNames: List<String> = userFlow(listOf(3L)).map { u -> u.name }.toList()
    collectedNames.first().upper
}

// --- 10) withContext result type inference (block returns String -> contextResult is String) ---
suspend fun probeWithContextInference() {
    val contextResult = withContext(Dispatchers.Default) { "computed" }
    contextResult.upper
}

// --- 11) coroutineScope builder block has CoroutineScope receiver (async available inside) ---
suspend fun probeCoroutineScopeReceiver(): User = coroutineScope {
    val innerDeferred = this.asy
    fetchUser(9L)
}

// --- 12) goto-definition target lines (resolved by the harness via textDocument/definition) ---
suspend fun probeGotoTargets() {
    delay(1)
    val gotoFlow: Flow<Int> = flow { emit(0) }
    gotoFlow.toList()
}

// suspend function return-type inference: explicit suspend producer
suspend fun probeSuspendReturn(): User {
    delay(1)
    return fetchUser(42L)
}

fun probeSuspendReturnConsumer() = runBlocking {
    val suspendProduced = probeSuspendReturn()
    suspendProduced.rol
    Unit
}

// ===== DIAGNOSTIC ISOLATION PROBES (root-cause discrimination) =====

// D1) Explicit typed CoroutineScope local with bare-receiver member access (no `this.`)
fun probeExplicitScope(diagScope: CoroutineScope) {
    diagScope.lau
}

// D2) Explicit typed User from await(), then member access (isolates await-chain vs typed-local)
suspend fun probeAwaitTyped() {
    val typedAwaited: User = async2()
    typedAwaited.nam
}
suspend fun async2(): User = fetchUser(1L)

// D3) Flow<Int>.map lambda param member access on a primitive element (Int -> toLong etc.)
fun probeFlowMapIntLambda() {
    val intFlow: Flow<Int> = flow { emit(1) }
    intFlow.map { mappedInt ->
        mappedInt.toLo
    }
}

// D4) suspend-call result assigned to explicit-typed local then member (isolates inference vs call)
suspend fun probeSuspendTyped() {
    val typedSuspend: User = probeSuspendReturn()
    typedSuspend.act
}

// D5) CONTROL: non-suspend typed-User local in a NON-suspend fn (baseline that should work)
fun probeNonSuspendTyped() {
    val plainUser: User = sampleUser()
    plainUser.ema
}

// D6) non-suspend typed-User local INSIDE a suspend fn (isolates "suspend body" vs "suspend RHS")
suspend fun probeNonSuspendRhsInSuspendFn() {
    val plainInSuspend: User = sampleUser()
    plainInSuspend.tag
}

// D7) direct chained call on cross-package project type (no local var)
fun probeChainedUser() {
    sampleUser().rol
}

// D8) member completion on a String local (cross-package not involved; should work)
fun probeStringLocalControl() {
    val plainString: String = "x"
    plainString.upp
}

// D9) withContext returning an explicit stdlib String, no annotation on the local
suspend fun probeWithContextStdlib() {
    val ctxString = withContext(Dispatchers.Default) { "abc" }
    ctxString.repe
}

// D10) Flow<Int>.collect lambda param type (stdlib Int element via coroutines higher-order fn)
suspend fun probeFlowCollectLambda() {
    val collectFlow: Flow<Int> = flow { emit(7) }
    collectFlow.collect { collectedInt ->
        collectedInt.toBy
    }
}

// D11) stdlib higher-order control: listOf(1).map { it.<member> } (NOT coroutines)
fun probeStdlibMapLambda() {
    listOf(1, 2, 3).map { listInt ->
        listInt.toSh
    }
}

// D12) SAME as D11 but using implicit `it` (reference harness proved this works for project types)
fun probeStdlibMapItLambda() {
    listOf(1, 2, 3).map {
        it.toCh
    }
}

// D13) Flow<Int>.map with implicit `it` (coroutines higher-order fn, implicit param)
fun probeFlowMapItLambda() {
    val itFlow: Flow<Int> = flow { emit(1) }
    itFlow.map {
        it.toDo
    }
}

// D14) withContext result with EXPLICIT type annotation (isolates generic-return inference)
suspend fun probeWithContextExplicit() {
    val ctxExplicit: String = withContext(Dispatchers.Default) { "abc" }
    ctxExplicit.repla
}

// D15) await() result with EXPLICIT String type from a Deferred<String>
suspend fun probeAwaitStdlib() {
    val strDeferred: Deferred<String> = asyncString()
    val awaitedStr: String = strDeferred.await()
    awaitedStr.trimM
}
suspend fun asyncString(): Deferred<String> = throw RuntimeException()
