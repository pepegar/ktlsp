package widgets

class Decorator(val label: String) {
    fun decorate(): String = "<$label>"
}
