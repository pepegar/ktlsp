package com.example.probe

import com.example.fixture.User
import com.example.fixture.sampleUser

// Minimal isolation probe: ONLY a plain project-local member-completion line, no library imports,
// no Json{} lambdas, no aliased imports. If completion works here but not in SerializationProbe.kt,
// the regression is caused by something specific in SerializationProbe.kt.
fun smProbeUserMember() {
    val smUser = sampleUser()
    smUser.ema
}

fun smProbeUserRef(u: User): String = u.name
