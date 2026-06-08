// Root build script. Each module applies the Kotlin JVM plugin itself; the root exists so the
// directory is a single Gradle build with two modules (:lib and :app).
plugins {
    kotlin("jvm") version "2.1.20" apply false
}
