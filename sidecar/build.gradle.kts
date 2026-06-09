// ktlsp's Kotlin compile-daemon sidecar: a long-lived JVM the Rust process spawns, which drives the
// real Kotlin compiler (kotlin-build-tools-api) incrementally and reports diagnostics over a line
// protocol. The `application` plugin's installDist gives a lib/ of unflattened jars (preserving each
// jar's META-INF/services so the build-tools-impl ServiceLoader resolves) plus a start script ktlsp
// invokes.
plugins {
    kotlin("jvm") version "2.1.20"
    application
}

repositories {
    mavenCentral()
}

dependencies {
    // The integrator contract...
    implementation("org.jetbrains.kotlin:kotlin-build-tools-api:2.1.20")
    // ...and the implementation (pulls kotlin-compiler-embeddable transitively).
    runtimeOnly("org.jetbrains.kotlin:kotlin-build-tools-impl:2.1.20")
}

application {
    mainClass.set("ktlsp.sidecar.MainKt")
}
