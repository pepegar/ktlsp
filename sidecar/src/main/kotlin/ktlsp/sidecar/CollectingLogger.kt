@file:OptIn(ExperimentalBuildToolsApi::class)

package ktlsp.sidecar

import org.jetbrains.kotlin.buildtools.api.ExperimentalBuildToolsApi
import org.jetbrains.kotlin.buildtools.api.KotlinLogger

// Captures the compiler's error/warning messages. The build-tools-impl renders each diagnostic to a
// GRADLE_STYLE string ("e: file:///abs/Foo.kt:L:C message") before handing it here, which is exactly
// the format ktlsp's parse_output already parses — so we collect the strings verbatim. info/debug/
// lifecycle are dropped (not diagnostics).
class CollectingLogger(private val sink: MutableList<String>) : KotlinLogger {
    override val isDebugEnabled: Boolean = false

    override fun error(msg: String, throwable: Throwable?) {
        sink.add(msg)
    }

    override fun warn(msg: String) {
        sink.add(msg)
    }

    override fun warn(msg: String, throwable: Throwable?) {
        sink.add(msg)
    }

    override fun info(msg: String) {}
    override fun debug(msg: String) {}
    override fun lifecycle(msg: String) {}
}
