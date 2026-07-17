package com.example.probe

// KOTLINX-SERIALIZATION probe.
//
// Exercises kotlinx-serialization-json APIs + @Serializable fixture types. The headless harness
// requests completion at the END of each partial selector and asserts the expected member is
// offered, and runs goto-definition / find-references on serialization symbols. EVERY cursor-marker
// line is textually UNIQUE (distinct receiver names / member prefixes) so the substring-based
// find_line matches the intended line.
//
// NOTE: @Serializable / @SerialName and the reified encodeToString/decodeFromString extension
// functions live in kotlinx-serialization-CORE, which is NOT extracted under ~/.cache/ktlsp/extracted
// (only kotlinx-serialization-JSON + kotlinx-coroutines-core + okio + kotlin-stdlib are). Goto into
// those core symbols therefore cannot reach a sources jar; goto into JSON symbols (Json, JsonElement,
// member decodeFromString(deserializer, string)) should.

import com.example.fixture.User
import com.example.fixture.Address
import com.example.fixture.Role
import com.example.fixture.sampleUser
import com.example.fixture.encodeUser
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.int
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.encodeToJsonElement
import kotlinx.serialization.encodeToString as coreEncodeToString  // INTENTIONALLY UNUSED: unused-import target

// ---- completion on the Json companion (static-like) ------------------------------------------

fun probeJsonCompanionEncode() {
    Json.encodeToStr                               // companion member: encodeToString
}

fun probeJsonCompanionDecode() {
    Json.decodeFromStr                             // companion member: decodeFromString
}

fun probeJsonCompanionParse() {
    Json.parseToJsonEl                             // companion member: parseToJsonElement
}

// ---- completion inside the Json { } builder lambda (JsonBuilder receiver) ---------------------

val probeJsonInstance: Json = Json {
    prettyPr                                       // JsonBuilder member: prettyPrint
}

val probeJsonInstanceTwo: Json = Json {
    ignoreUnknownK                                 // JsonBuilder member: ignoreUnknownKeys
}

// ---- completion on a configured Json instance value -------------------------------------------

fun probeJsonInstanceEncode() {
    val cfg = Json { encodeDefaults = true }
    cfg.encodeToJsonEl                             // instance member/ext: encodeToJsonElement
}

fun probeJsonInstanceConfig() {
    val cfgTwo = Json { prettyPrint = true }
    cfgTwo.configura                               // instance property: configuration
}

// ---- member completion on a @Serializable fixture data class instance -------------------------

fun probeSerializableUserMember() {
    val userVal = sampleUser()
    userVal.ema                                    // User data-class property: email
}

fun probeSerializableUserRole() {
    val userRole = sampleUser()
    userRole.rol                                   // User data-class property: role
}

fun probeSerializableUserCopy() {
    val userCopy = sampleUser()
    userCopy.cop                                   // data-class generated member: copy
}

fun probeSerializableAddressMember() {
    val addr = Address(street = "s", city = "c", postalCode = "p")
    addr.postal                                    // Address data-class property: postalCode
}

// ---- @Serializable enum member completion -----------------------------------------------------

fun probeRoleEnumEntry() {
    Role.ADM                                       // enum entry: ADMIN
}

// ---- JsonElement hierarchy completion (library types) -----------------------------------------

fun probeJsonElementMember() {
    val el: JsonElement = JsonPrimitive(42)
    el.jsonObj                                     // JsonElement extension accessor: jsonObject
}

fun probeJsonPrimitiveMember() {
    val prim = JsonPrimitive("text")
    prim.conten                                    // JsonPrimitive property: content
}

fun probeBuildJsonObjectReceiver() {
    val obj = buildJsonObject {
        pu                                         // JsonObjectBuilder member: put
    }
    obj.toStr                                      // JsonObject -> toString (sanity)
}

// ---- chained-call inference through serialization ---------------------------------------------

fun probeEncodeToElementChain() {
    val cfg3 = Json { prettyPrint = true }
    // encodeToJsonElement returns JsonElement; .jsonObject -> JsonObject; member access on it.
    cfg3.encodeToJsonElement(sampleUser()).jsonOb  // chained: jsonObject accessor on JsonElement
}

fun probeJsonPrimitiveIntChain() {
    // JsonPrimitive(7).int -> Int; .toLo -> toLong on Int
    JsonPrimitive(7).int.toLo                      // chain: JsonPrimitive.int (Int) then toLong
}

// ---- nullability: Address? from User -> safe call / elvis ------------------------------------

fun probeNullableAddress() {
    val userNull = sampleUser()
    // address is Address? ; safe call yields String? ; elvis to String
    val cityVal: String = userNull.address?.city ?: "unknown"
    cityVal.upperc                                 // String member after elvis: uppercase
}

// ---- goto / references anchor lines ----------------------------------------------------------
// goto-definition into the JSON sources jar (on the Json type reference).
val gotoJsonAnchor: Json = Json.Default

// goto-definition on a @Serializable project type (User -> Model.kt) and used so the import isn't
// reported unused.
fun probeUserRef(u: User): String = encodeUser(u)

// find-references anchor: the @Serializable annotation usage + a User reference live here and in
// Model.kt / Main.kt / Storage.kt etc. (cross-file references to User).
@Serializable
data class ProbeWrapper(val owner: User, val note: String)
