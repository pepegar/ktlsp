@file:OptIn(ExperimentalBuildToolsApi::class)

package ktlsp.sidecar

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.jetbrains.kotlin.buildtools.api.CompilationResult
import org.jetbrains.kotlin.buildtools.api.CompilationService
import org.jetbrains.kotlin.buildtools.api.ExperimentalBuildToolsApi
import org.jetbrains.kotlin.buildtools.api.ProjectId
import org.jetbrains.kotlin.buildtools.api.SourcesChanges
import org.jetbrains.kotlin.buildtools.api.jvm.ClassSnapshotGranularity
import org.jetbrains.kotlin.buildtools.api.jvm.ClasspathSnapshotBasedIncrementalCompilationApproachParameters
import java.io.File
import java.util.UUID

// Protocol (line-based, one JSON object per line):
//   stdin  <- {"type":"ping"} | {"type":"shutdown"} | {"type":"compile", ...CompileRequest}
//   stdout -> {"type":"ready",...} | {"type":"pong"} | {"type":"result",...} | {"type":"error",...}
// stdout is reserved for the protocol; everything the compiler prints is forced to stderr so it
// can't corrupt a response frame.

@Serializable
data class CompileRequest(
    val module: String,
    val sourceRoots: List<String>,
    val classpath: List<String>,
    val cacheDir: String,
    val jvmTarget: String = "17",
)

@Serializable
data class ResultResponse(
    val type: String = "result",
    val success: Boolean,
    val executed: Boolean,
    val diagnostics: List<String>,
)

fun main() {
    val protocol = System.out
    System.setOut(System.err) // keep the protocol channel clean of compiler chatter

    val json = Json { ignoreUnknownKeys = true }
    fun send(line: String) {
        protocol.println(line)
        protocol.flush()
    }

    val compiler = Compiler()
    send("""{"type":"ready","compilerVersion":"${compiler.version}"}""")

    val reader = System.`in`.bufferedReader()
    while (true) {
        val line = reader.readLine() ?: break
        val trimmed = line.trim()
        if (trimmed.isEmpty()) continue
        val obj = runCatching { json.parseToJsonElement(trimmed) as JsonObject }.getOrNull()
        when (obj?.get("type")?.jsonPrimitive?.content) {
            "shutdown" -> break
            "ping" -> send("""{"type":"pong"}""")
            "compile" -> {
                val resp = runCatching {
                    val req = json.decodeFromString(CompileRequest.serializer(), trimmed)
                    compiler.compile(req)
                }.getOrElse { e ->
                    ResultResponse(success = false, executed = false, diagnostics = listOf("e: sidecar: ${e.message}"))
                }
                send(json.encodeToString(ResultResponse.serializer(), resp))
            }
            else -> send("""{"type":"error","message":"unknown request"}""")
        }
    }
}

/// Drives the Kotlin compiler in-process and warm. State (ProjectId, "already compiled once") is
/// kept across requests so incremental compilation stays warm for the sidecar's lifetime.
class Compiler {
    private val service = CompilationService.loadImplementation(Thread.currentThread().contextClassLoader)
    val version: String = service.getCompilerVersion()

    // One stable ProjectId per module for the sidecar lifetime (keys the daemon session + caches).
    private val projectIds = HashMap<String, ProjectId>()
    // Modules compiled at least once this session — after the first compile the classpath is assumed
    // unchanged, so snapshot comparison can be skipped.
    private val warmed = HashSet<String>()

    fun compile(req: CompileRequest): ResultResponse {
        val projectId = projectIds.getOrPut(req.module) { ProjectId.ProjectUUID(UUID.randomUUID()) }
        val cache = File(req.cacheDir)
        val workingDir = File(cache, "ic").apply { mkdirs() }
        val outDir = File(cache, "out").apply { mkdirs() }
        val snapDir = File(cache, "snapshots").apply { mkdirs() }
        val shrunk = File(cache, "shrunk-classpath-snapshot.bin")

        val snapshotFiles = req.classpath.map { entry -> snapshotFor(File(entry), snapDir) }

        val diagnostics = ArrayList<String>()
        val logger = CollectingLogger(diagnostics)

        val strategy = service.makeCompilerExecutionStrategyConfiguration().useInProcessStrategy()
        val config = service.makeJvmCompilationConfiguration().useLogger(logger)
        val ic = config.makeClasspathSnapshotBasedIncrementalCompilationConfiguration()
            .setBuildDir(outDir)
            .keepIncrementalCompilationCachesInMemory(true)
            .useOutputDirs(listOf(outDir, workingDir))
            .assureNoClasspathSnapshotsChanges(req.module in warmed)
        val params = ClasspathSnapshotBasedIncrementalCompilationApproachParameters(snapshotFiles, shrunk)
        config.useIncrementalCompilation(workingDir, SourcesChanges.ToBeCalculated, params, ic)

        val sources = req.sourceRoots.flatMap { root ->
            File(root).walkTopDown().filter { it.isFile && (it.extension == "kt" || it.extension == "java") }.toList()
        }
        val moduleName = req.module.trim(':').replace(':', '-').ifEmpty { "module" }
        val args = listOf(
            "-no-stdlib",
            "-no-reflect",
            "-classpath", req.classpath.joinToString(File.pathSeparator),
            "-d", outDir.absolutePath,
            "-jvm-target", req.jvmTarget,
            "-module-name", moduleName,
            "-language-version", "2.1",
            "-api-version", "2.1",
        )

        val result = service.compileJvm(projectId, strategy, config, sources, args)
        warmed.add(req.module)
        return ResultResponse(
            success = result == CompilationResult.COMPILATION_SUCCESS,
            executed = true,
            diagnostics = diagnostics,
        )
    }

    // Snapshot per classpath entry, cached on disk by entry path + mtime so a stable classpath isn't
    // re-snapshotted on every compile.
    private fun snapshotFor(entry: File, snapDir: File): File {
        val key = Integer.toHexString("${entry.absolutePath}:${entry.lastModified()}".hashCode())
        val out = File(snapDir, "$key.snapshot")
        if (!out.exists() && entry.exists()) {
            service.calculateClasspathSnapshot(entry, ClassSnapshotGranularity.CLASS_LEVEL).saveSnapshot(out)
        }
        return out
    }
}
