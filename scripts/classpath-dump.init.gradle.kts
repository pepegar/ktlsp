// Dump each module's resolved `compileClasspath` as a machine-readable line protocol, for ktlsp's
// Kotlin compile-daemon backend to feed the compiler. Run once and cache (the classpath is stable
// across .kt edits); re-run only when build files change.
//
//   ./gradlew -I scripts/classpath-dump.init.gradle.kts ktlspDumpClasspath -q
//
// Output (stdout), one record per module:
//   PROJECT\t<gradle path>\t<absolute projectDir>
//   CP\t<absolute jar/dir>          (repeated)
//   UNRESOLVED\t<selector>\t<problem> (lenient-resolution drops, repeated)
//   END\t<gradle path>
//
// `isLenient = true` so an unresolved dependency in one module doesn't abort the whole dump.
// Each block is buffered and printed with a single `println`: Gradle runs these tasks in parallel
// and per-line printing interleaves the protocol blocks, scrambling ktlsp's per-module
// attribution (observed: ~3.6k interleave anomalies in one GoodNotes dump). One synchronized
// PrintStream call per module keeps the block atomic.
//
// UNRESOLVED lines report what the lenient view dropped, so silent under-indexing is observable.
// They are captured eagerly in `afterEvaluate` (configuration time) as strings so the task never
// touches a `Configuration` object at execution time.
//
// Configuration-cache compatibility: all `Project` state (path, projectDir, the
// `compileClasspath` FileCollection, and the pre-resolved unresolved-dep strings) is captured at
// configuration time as CC-serializable values. The `doLast` action only reads those serialized
// values — it never calls `Task.getProject()` or touches a `Configuration` object at execution
// time. `afterEvaluate` defers the capture until after the project's build script has applied the
// plugins that create `compileClasspath` (Kotlin, Java, etc.), since init scripts run before
// project build scripts. Modules without a `compileClasspath` configuration skip task registration
// entirely.
allprojects {
    val projectPath = project.path
    val projectDirPath = project.projectDir.absolutePath

    afterEvaluate {
        val compileClasspath = configurations.findByName("compileClasspath")
        if (compileClasspath != null) {
            val classpathFiles = compileClasspath.incoming.artifactView {
                isLenient = true
            }.files

            val unresolvedLines = try {
                compileClasspath.resolvedConfiguration.lenientConfiguration
                    .unresolvedModuleDependencies.map {
                        val problem = it.problem.message?.replace('\n', ' ')?.replace('\t', ' ')
                        "UNRESOLVED\t${it.selector}\t${problem}"
                    }
            } catch (e: Exception) {
                emptyList()
            }

            tasks.register("ktlspDumpClasspath") {
                doLast {
                    val out = StringBuilder()
                    out.append("PROJECT\t${projectPath}\t${projectDirPath}\n")
                    classpathFiles.files.forEach {
                        out.append("CP\t${it.absolutePath}\n")
                    }
                    unresolvedLines.forEach {
                        out.append(it).append('\n')
                    }
                    out.append("END\t${projectPath}")
                    println(out.toString())
                }
            }
        }
    }
}
