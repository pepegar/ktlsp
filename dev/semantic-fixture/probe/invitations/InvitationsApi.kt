package probe.invitations

import probe.invitations.routing.call
import probe.invitations.routing.independentVersionedRoutes
import probe.invitations.routing.ok
import probe.invitations.routing.post
import probe.invitations.routing.route
import probe.invitations.routing.Route

interface CreateInvitationUseCase {
    fun execute()
}

class InvitationsApi
    (
    private val createInvitationUseCase: CreateInvitationUseCase,
) {
    fun Route.setup() {
        createInvitationUseCase.execute()
        independentVersionedRoutes {
            allVersionsRoutes {
                route("/invitations") {
                    post {
                        ok {
                            val resolvedCall = call
                        }
                    }
                }
            }
        }
    }

    fun Route.completionProbe() {
        independentVersionedRoutes {
            allVersionsRoutes {
                route("/invitations") {
                    post {
                        ok {
                            ca
                        }
                    }
                }
            }
        }
    }
}
