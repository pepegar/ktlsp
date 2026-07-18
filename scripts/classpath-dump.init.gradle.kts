// Dump each module's resolved `compileClasspath` as a machine-readable line protocol, for ktlsp's
// Kotlin compile-daemon backend to feed the compiler. Run once and cache (the classpath is stable
// across .kt edits); re-run only when build files change.
//
//   ./gradlew -I scripts/classpath-dump.init.gradle.kts ktlspDumpClasspath -q
//
// Output (stdout), one record per module:
//   PROJECT\t<gradle path>\t<absolute projectDir>
//   CP\t<absolute jar/dir>          (repeated)
//   END\t<gradle path>
//
// `isLenient = true` so an unresolved dependency in one module doesn't abort the whole dump.
// Each block is buffered and printed with a single `println`: Gradle runs these tasks in parallel
// and per-line printing interleaves the protocol blocks, scrambling ktlsp's per-module
// attribution (observed: ~3.6k interleave anomalies in one GoodNotes dump). One synchronized
// PrintStream call per module keeps the block atomic.
// UNRESOLVED lines report what the lenient view dropped, so silent under-indexing is observable.
allprojects {
    tasks.register("ktlspDumpClasspath") {
        doLast {
            val cfg = configurations.findByName("compileClasspath") ?: return@doLast
            val out = StringBuilder()
            out.append("PROJECT\t${project.path}\t${project.projectDir.absolutePath}\n")
            cfg.incoming.artifactView { isLenient = true }.files.files.forEach {
                out.append("CP\t${it.absolutePath}\n")
            }
            cfg.resolvedConfiguration.lenientConfiguration.unresolvedModuleDependencies.forEach {
                val problem = it.problem.message?.replace('\n', ' ')?.replace('\t', ' ')
                out.append("UNRESOLVED\t${it.selector}\t$problem\n")
            }
            out.append("END\t${project.path}")
            println(out.toString())
        }
    }
}
