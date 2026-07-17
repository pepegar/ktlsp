package kotlin

class Throwable

class Result<T>

fun <R> runCatching(block: () -> R): Result<R> = TODO()

fun <T> Result<T>.onFailure(action: (Throwable) -> Unit): Result<T> = this

fun <T> Result<T>.getOrThrow(): T = TODO()
