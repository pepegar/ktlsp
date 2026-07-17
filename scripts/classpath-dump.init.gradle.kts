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
allprojects {
    tasks.register("ktlspDumpClasspath") {
        doLast {
            val cfg = configurations.findByName("compileClasspath") ?: return@doLast
            println("PROJECT\t${project.path}\t${project.projectDir.absolutePath}")
            cfg.incoming.artifactView { isLenient = true }.files.files.forEach {
                println("CP\t${it.absolutePath}")
            }
            println("END\t${project.path}")
        }
    }
}
