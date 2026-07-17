package com.example.fixture

// Same-package multi-receiver isolation probe (no imports needed; all in com.example.fixture).
fun spProbeBasicCtor() {
    val spBasic = BasicGreeter()
    spBasic.salu                 // reference-style: constructor receiver -> salutation
}

fun spProbeUserCtor() {
    val spUserC = User(id = 1L, name = "n", email = "e")
    spUserC.ema                  // User constructor receiver -> email
}

fun spProbeUserFn() {
    val spUser = sampleUser()
    spUser.ema                   // top-level fn return -> User.email
}

fun spProbeGreeterFor() {
    greeterFor("en").gre         // function return type Greeter -> greet
}
