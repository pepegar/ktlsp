package probe.cleaner

class AttributeValue {
    var s: String = ""
    var n: String = ""
}

fun probeApply(value: String) {
    AttributeValue().apply {
        this.s
    }
}
