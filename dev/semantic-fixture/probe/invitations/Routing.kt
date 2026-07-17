package probe.invitations.routing

class ApplicationCall
class ControllerCall
class PipelineContext<T, C>
class Unrelated
class Route

val <C : ApplicationCall> PipelineContext<Unit, C>.call: C get() = TODO()
val Unrelated.call: Int get() = 1

typealias RoutingHandler = suspend PipelineContext<Unit, ApplicationCall>.(Unit) -> Unit

fun <R> Route.post(body: suspend PipelineContext<Unit, ApplicationCall>.(R) -> Unit) {}
fun Route.post(body: RoutingHandler) {}

class VersionedRouteBuilder {
    fun allVersionsRoutes(body: Route.() -> Unit) {}
}

fun Route.independentVersionedRoutes(body: VersionedRouteBuilder.() -> Unit) {}
fun Route.route(path: String, body: Route.() -> Unit) {}

fun <C : ApplicationCall> PipelineContext<Unit, C>.ok(
    controllerFunction: suspend ControllerCall.() -> Unit,
) {}
