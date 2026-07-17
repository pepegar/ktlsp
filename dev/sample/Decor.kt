package widgets

class AutoImportTarget

class Decorator(val label: String) {
    class Badge

    fun decorate(): String = "<$label>"
}
