// Build script for the ktlsp gradle-sample test fixture.
//
// The dependencies here are declared through the version catalog (libs.*) so they line up
// exactly with what ktlsp parses out of gradle/libs.versions.toml. ktlsp downloads each
// dependency's `-sources.jar` from Maven Central to power goto-definition and completion.

plugins {
    kotlin("jvm") version "2.1.20"
    kotlin("plugin.serialization") version "2.1.20"
    application
}

group = "com.example"
version = "0.1.0"

repositories {
    mavenCentral()
}

dependencies {
    implementation(libs.kotlin.stdlib)
    implementation(libs.kotlinx.serialization.json)
    implementation(libs.kotlinx.coroutines.core)
    implementation(libs.okio)
}

application {
    mainClass.set("com.example.fixture.MainKt")
}

kotlin {
    jvmToolchain(17)
}
