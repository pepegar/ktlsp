plugins {
    kotlin("jvm")
    application
}

dependencies {
    implementation(project(":lib"))
}

application {
    mainClass.set("com.example.app.MainKt")
}
