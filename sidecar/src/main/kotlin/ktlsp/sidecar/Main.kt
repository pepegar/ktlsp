@file:OptIn(ExperimentalBuildToolsApi::class)

package ktlsp.sidecar

import org.jetbrains.kotlin.buildtools.api.CompilationService
import org.jetbrains.kotlin.buildtools.api.ExperimentalBuildToolsApi

// Protocol (line-based, one JSON object per line):
//   stdin  <- requests   {"type":"ping"} | {"type":"shutdown"} | (compile, added in U3)
//   stdout -> responses  {"type":"ready","compilerVersion":"..."} | {"type":"pong"} | ...
// stdout is reserved for the protocol; everything the compiler prints is forced to stderr so it
// can't corrupt a response frame.
fun main() {
    val protocol = System.out
    System.setOut(System.err)

    fun send(json: String) {
        protocol.println(json)
        protocol.flush()
    }

    val service = CompilationService.loadImplementation(Thread.currentThread().contextClassLoader)
    val version = service.getCompilerVersion()
    send("""{"type":"ready","compilerVersion":"$version"}""")

    val reader = System.`in`.bufferedReader()
    while (true) {
        val line = reader.readLine() ?: break
        val trimmed = line.trim()
        if (trimmed.isEmpty()) continue
        when {
            trimmed.contains("\"shutdown\"") -> break
            trimmed.contains("\"ping\"") -> send("""{"type":"pong"}""")
            else -> send("""{"type":"error","message":"unknown request"}""")
        }
    }
}
