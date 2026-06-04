// Settings for the ktlsp gradle-sample test fixture.
//
// This is a minimal, self-contained single-module Gradle (Kotlin DSL) project whose only
// purpose is to give ktlsp a realistic project to index: a version catalog at
// gradle/libs.versions.toml plus Kotlin sources that exercise the catalog's libraries.

pluginManagement {
    repositories {
        gradlePluginPortal()
        mavenCentral()
    }
}

dependencyResolutionManagement {
    repositories {
        mavenCentral()
    }
}

rootProject.name = "ktlsp-gradle-sample"
