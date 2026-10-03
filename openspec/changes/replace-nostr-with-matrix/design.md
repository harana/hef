The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/replace-nostr-with-matrix/).

# Design — Matrix as the base of the application

Contested choices, recorded as tension → decision → why → implied spec edits, in
the style of the platform decision register. Each of these was a real fork in the
road; the rejected side is written down so a later change meets the record rather
than reopening the debate from scratch.

---

## D1. The homeserver is native, on compio, inside the one product binary

**Tension.** Matrix is a client/server protocol and the library the clients are
built on is a client library. Something has to be the homeserver. Three shapes were available: deploy an
existing homeserver alongside the product, point each deployment at a customer's
own homeserver, or implement the Client-Server API natively.

**Decision.** Pulse implements the homeserver itself, as a capability module on
the platform's compio runtime, inside `harana-node`. Federation is specified and
off by default.

**Why.** The same three reasons that decided the native relay, plus two that are
specific to Matrix.

The platform reasons are unchanged. One homogeneous binary is Decision 31, and a
second server process reintroduces exactly the leader/reader operational split
that decision retired. Every stored event belongs on the HEJ/HEF tier under the
tenant's commit-authority lease (Decisions 32 and 44); a separate homeserver owns
its own database and its own durability story, and the platform's event tier
becomes a copy rather than the record. And the tenancy model is per-tenant
commit authority, which an off-the-shelf homeserver has no concept of.

The Matrix-specific reasons are these. First, **every embeddable Rust homeserver
is more `tokio`-bound than the client library is.** Continuwuity is `tokio`
multi-thread on axum and hyper with a forked RocksDB; Palpo is `tokio` on Salvo
and PostgreSQL; Conduit is beta and its own upstream points elsewhere; conduwuit
is archived read-only. None publishes its internal crates to crates.io with a
stability contract, and all of them hard-code an `axum::serve` on a
`TcpListener`. Embedding one means running a Tokio runtime inside the product
binary, which is the thing INV-RUNTIME exists to prevent. Second, **Synapse is
AGPLv3-or-later**, dual-licensed with a paid commercial licence. The dependency
licence allowlist admits MIT, Apache-2.0, the BSD pair, ISC, Unicode-3.0, and
Zlib. Bundling Synapse is not a licensing inconvenience to work around; it is
outside the allowlist.

**What this costs, stated plainly.** State resolution, the auth rules, and room
versions are subtle and Pulse will own all three. The mitigation is that they are
*specified* — unlike the sixteen Buzz-authored extensions, whose only
specification was an implementation — and that the conformance gate tests them
against an independent suite rather than against our own reading of them.

**Rejected: point each deployment at a customer homeserver.** It is the smallest
change and the fastest route to parity, and it cedes the substrate. Tenancy, the
event tier, retention, and the audit trail all become someone else's
implementation, and the enterprise deployments this product is sold into are
precisely the ones that will not accept a messaging substrate the vendor cannot
account for.

**Rejected: deploy continuwuity alongside.** Genuinely tempting — Apache-2.0, a
real spec-conformant server, and far less to build. Refused on the one-binary
rule, on the Tokio and RocksDB operational surface it adds, and on an unverified
premise: MSC4186 support is mature in Synapse and needs confirming elsewhere. If
D1 is ever reopened, this is the alternative to reopen it with.

**Implied spec edits.** `matrix-homeserver-runtime` owns the listener, the
Client-Server API surface, and the lease-governed write path. `http-api` gains
`node_matrix_homeserver` and loses the two Buzz listener roles.
`system-architecture` records the homeserver as a capability module rather than a
deployment unit.

---

## D2. The client library is adopted, not forked, because it already asks for the runtime, transport and storage

**Tension.** The mechanical adoption test refuses the upstream Matrix client SDK
on three of its four clauses — an async runtime, an HTTP client, and a database
driver. Under Decision 44's rule a refusal means porting the behaviour natively,
which would mean writing a Matrix client from scratch alongside the homeserver.
Refusing that, the obvious next move is a fork retargeted onto the platform's
runtime.

**Decision.** Neither. `harana/harana-matrix` already carries the three
interfaces the platform needs, so it is **adopted** — built with default
features off, with the platform implementing the runtime, the transport, and the
four stores. There is no retargeting work, and therefore no fork to maintain.

**Why this is not the same decision as forking.** A fork is a tree the platform
owns and must rebase; an adoption is a dependency the platform configures. The
distinction is load-bearing here because the library's own design is what makes
it possible:

- `crates/matrix-sdk-common/src/runtime/mod.rs:110` defines
  `AsyncRuntime: Debug + SendOutsideWasm + SyncOutsideWasm + 'static` with three
  methods — `spawn`, `spawn_blocking`, `sleep` — installed once with
  `set_runtime()`. The feature that supplies the built-in backend says what it is
  for in as many words: *"Turn this off to build without any built-in runtime,
  and install your own instead."*
- The bundled HTTP client is `optional = true` behind its own transport feature,
  so the transport is an interface rather than a stored concrete type.
- The bundled file-database backend is `optional = true` and off unless asked
  for.

So the platform's work is three implementations against stated interfaces, not a
retargeting of somebody else's internals. That is a different kind of ongoing
cost: a version bump re-runs the checks and the conformance suites, and it does
not re-run a rebase.

**Rejected: fork and retarget.** What this change proposed before the library was
available. It was a sound plan against the upstream project — whose wasm support
means most of the abstraction already exists — and it is simply unnecessary
against a library that exposes the abstraction as public API. A fork would also
have owned a standing difference that shrinks only if upstream accepts it.

**Rejected: write a native Matrix client.** Consistent with Decision 44's letter
and wrong on its arithmetic. The client surface is the timeline, the send queue,
the event cache, sliding-sync driving, authentication, and the whole
end-to-end-encryption state machine — tens of thousands of lines where a subtle
error is a silent confidentiality failure, and which is already free of any
runtime coupling. Re-deriving it to satisfy a rule whose purpose is keeping a
second runtime out would be the rule defeating its own intent.

**The two costs this buys, stated rather than glossed.** First, **the thread
models do not compose**. The library's interfaces require futures that are safe
to move between threads; the platform's completion-based runtime produces futures
that are not, and its spawn is thread-local. Every timer and every request
therefore crosses a bridge, and where the platform runs more than one reactor,
which reactor a piece of library work lands on has to be decided rather than
inherited from whichever thread polled the caller. The library's own
documentation shows the bridge and gives no undertaking about where it polls, so
this is ours to define. It is the largest unmodelled risk in Phase 1, because
everything else there has a stated contract to satisfy and this does not.

Second, **the configuration is available but unexercised**. Nothing in the
library's own continuous integration builds it with the runtime backend and the
bundled HTTP client both off — every no-default-features job turns one of them
back on. The cfg gating looks carefully done and there is a test stub that only
makes sense if someone tried it, but a first build should expect a round of
compile errors. The response is to fix them and contribute back a job that builds
this configuration, so it stays compiling rather than decaying between pins.

**Implied spec edits.** `matrix-client-runtime` states the three implementations,
the thread-model bridge, and the feature-set discipline. `system-architecture` gains "An upstream crate
carrying a runtime interface is adopted, not forked".

---

## D3. The runtime ban is on starting a second runtime, not on a crate name

**Tension.** With every runtime feature off, the client library still names the
runtime crate and two of its companions as unconditional dependencies, for
synchronisation primitives, stream adapters, and future-combinator macros. None
of those starts an executor; all of them run under any executor. But the
adoption test matches crate **families** — the companion crates count as the
runtime crate — so the library fails a gate for a reason that has nothing to do
with running a second runtime.

**Decision.** Narrow the rule to what it was always about. The async-runtime
clause is decided by a crate's **resolved feature set** rather than its name: a
runtime crate whose resolved features are a subset of a recorded runtime-free
allowlist is admitted, and any other feature refuses. The allowlist is an
allowlist, so a feature the rule has not classified refuses rather than passes.

**Why this is a correction, not a concession.** The invariant's own statement of
intent already draws this line. The runtime-inversion check's module
documentation says the runtime crate "is still allowed to sit *underneath* a
couple of pinned, third-party building blocks ... where it only supplies plumbing
types and never starts a runtime of its own", and that "what must never happen is
one of *our* crates depending on it directly, which is the first step toward
someone spawning a second runtime beside compio". The rule was already
"no second runtime". It was implemented as "no crate by that name", plus a
by-name allowlist for the cases where that was too strong.

The feature test replaces a by-name allowlist with a mechanical property, and it
is **stricter** in the direction that matters. Today's sanctioned carriers are
trusted because they are named; under the feature test each is re-verified, and
any that turns out to resolve an executor or timer feature keeps a named
exception that records why rather than inheriting one. The direct-dependency ban
on crates the platform writes is unchanged and unconditional: our own code has
our own runtime and never needs another.

**What the rule does not claim.** It proves no second executor, reactor or timer
driver is compiled into the binary. It does not prove a dependency makes good use
of the platform's runtime — a crate could satisfy the interface by running
blocking work inline and stalling the reactor. That is a review question, and
`matrix-client-runtime` states it as a requirement rather than leaving the check
to imply it.

**Rejected: change the library instead, making the synchronisation dependencies
optional too.** Tractable — the platform controls the library — and refused
because it treats a correct dependency as a defect. Those primitives are
runtime-agnostic by construction; substituting equivalents from another crate
family would add a maintenance burden and a divergence from the upstream project
purely to satisfy a name match. Fixing the rule is the smaller and more honest
change, and it benefits every future dependency rather than this one.

**Accepted cost.** The check gets more complex: it reads resolved features from
the dependency graph rather than package names alone, and it needs a classified
allowlist per runtime family that someone maintains. A new feature in a new
version blocks a bump until it is classified. That is the intended behaviour, but
it is friction on an upgrade path, and it should not be discovered as a surprise.

**Implied spec edits.** `system-architecture` gains "The runtime ban is on
starting a second runtime, not on a crate name". The adoption test and the
runtime-inversion check both read resolved features; the by-name carrier and
introducer allowlists are re-derived under the new rule rather than carried
forward unexamined.

---

## D4. The bundled storage backend is switched off, and the four stores are implemented over the platform store

**Tension.** The library's default persistence is a file database behind a
blocking pool. That is a database driver the adoption test refuses, and a third
durable store beside the platform's event tier and its relational store.

**Decision.** Leave it switched off — it is optional and off unless asked for —
and implement the library's four store interfaces over the platform's own `store`
crate, event tier, and object store.

**Why.** One decision removes three problems: the database-driver clause, a
blocking pool bound to a runtime the platform does not have, and a third answer
to a storage question the corpus has already answered twice. The interfaces are
designed to be implemented — asynchronous CRUD with an associated error type,
each with an object-safe erasure layer — and the library ships in-memory
implementations of all four as production code plus shared conformance suites a
custom backend can be run against.

It also lands the existing storage rule without a special case: event bodies go
to the event tier, and relational state is limited to derived projections
rebuildable from durable coverage.

**Accepted cost.** Roughly 122 trait methods across the four is real work, and
the encryption key store carries obligations ordinary CRUD does not — losing an
inbound group session makes messages permanently unreadable, and reusing an
outbound session's message index breaks the cipher's guarantees. The mitigation
is the library's own conformance suites, run against the platform backend as the
acceptance check rather than reimplemented, so the contract tested is the
library's rather than our reading of it.

**Implied spec edits.** `matrix-client-runtime` owns the four implementations.
`matrix-encryption` states the durability obligations the key store carries.

---

## D5. Sync is simplified sliding sync, and the homeserver serves `/sync` v2 as well

**Tension.** Matrix has two sync mechanisms. The client SDK's high-level stack —
`RoomListService`, `EncryptionSyncService`, `Timeline` — runs simplified sliding
sync (MSC4186) only; the older MSC3575 sliding-sync-proxy variant has been
removed from the SDK entirely. But `/sync` v2 is what the wider client ecosystem
still uses, and MSC4186 is an unstable feature that servers advertise through
`unstable_features`.

**Decision.** The native homeserver implements both. MSC4186 is what Pulse's own
clients use and is therefore a homeserver requirement rather than a client
preference; `/sync` v2 is what makes the third-party client promise real.

**Why the second one is not optional.** The whole case for Matrix over Nostr
rests partly on the client ecosystem. A homeserver that only speaks an unstable
sync extension serves Pulse's clients and nothing else, which would leave the
product with a bespoke protocol again — this time with more specification to
implement, which is the worst of both.

**Implied spec edits.** `matrix-homeserver-runtime` requires both sync surfaces
and the capability advertisement that lets a client choose.
`matrix-interop-conformance` drives a stock client over `/sync` v2 as a distinct
interop leg.

---

## D6. The Buzz-authored extensions become custom event types, not a second protocol

**Tension.** Sixteen NIPs exist only in the upstream implementation: agent
identity and owner attestation, agent memory, personas and teams, turn metrics,
session observability, encrypted push leases, event reminders, cross-device read
state, channel windows, git object signing, multi-repository projects, DM
visibility. They carry real product behaviour — the agent surface alone spans 94
of 301 files in one client feature directory — and Matrix has no equivalent for
any of them.

**Decision.** Rebuild each on Matrix's own extension mechanism: custom event
types in a `com.harana.*` reverse-DNS namespace. State events where the extension
was a replaceable event, account data where it was per-user preference, ordinary
room events otherwise. The product behaviour is preserved; the wire encoding
changes.

**Why this is not a workaround.** Custom event types under a reverse-DNS prefix
are the mechanism the Matrix specification defines for exactly this, and the
mapping is closer than it looks. A Nostr replaceable event keyed by
`(kind, pubkey, d-tag)` is a Matrix state event keyed by `(type, state_key)` —
both are last-writer-wins with a natural key, except that Matrix resolves
conflicts by a specified algorithm instead of a timestamp comparison. A
per-author preference event is account data. An ephemeral event is an EDU.

**What genuinely does not map, and what replaces it.** Nostr's arbitrary-kind
extensibility needs no server agreement; a Matrix custom event type needs the
homeserver to admit it and, for state events, needs auth rules that permit it.
Since Pulse writes the homeserver, that is a requirement to state rather than an
obstacle. Key-as-identity portability is genuinely lost: a Nostr identity is a
keypair the user carries between relays, whereas an MXID is bound to a
homeserver. For an enterprise deployment with custodial keys and provisioned
identities — which is what Decision 44 already established this product to be —
that loss costs nothing the deployment was using.

**Rejected: drop the extensions and take stock Matrix behaviour.** It would
shrink this change substantially. It is refused because the agent surface is
product, not protocol garnish, and because the alternative to specifying these is
not "no extensions" but "extensions invented per client".

**Implied spec edits.** `matrix-workspace-extensions` owns the namespace and the
agent, memory, persona, metrics, reminder, and read-state types.
`matrix-git-collaboration` owns the repository, patch, issue, and project types.

---

## D7. Removal is one release train, and the messaging surface is down in between

**Tension.** Roughly 77,600 lines of working server code and four client
applications depend on the Nostr surface. Removing it before Matrix reaches
parity means an outage of the messaging surface. Keeping both alive means two
event models in every client, and a conformance gate, divergence register, and
NIP coverage matrix that all have to keep passing against a protocol being
discarded.

**Decision.** One release train. The Nostr surface is removed in this change and
Matrix replaces it; there is no dual-protocol window, no bridge, and no
compatibility shim. The gate for each phase is the interop suite, not a date.

**Why.** A dual-protocol window buys availability with permanent complexity in
the place the complexity hurts most — the clients, where the desktop application
already has 475 files touching the Nostr surface and a 128-constant kind
registry. Carrying both would mean maintaining two transports, two identity
models, and two encryption stacks in each of four applications, and keeping the
upstream Buzz conformance gate green throughout, which means keeping the pinned
`harana-oss/buzz` clone in CI for the whole transition.

**Accepted cost.** The messaging surface is unavailable between the removal
landing and the interop gate going green. This is the plainest cost in the change
and it is not mitigated by anything except phasing and the gate. A deployment
that cannot accept it should not take this release train.

**Rejected: Matrix first, delete after parity.** The safer schedule and the one
most projects would choose. Refused on the client arithmetic above, and on a
second point: a removal deferred to a follow-up change is a removal that competes
with feature work forever.

**Implied spec edits.** The tasks are phased with the interop gate at every
boundary. `matrix-interop-conformance` states that the gate, not a date, closes a
phase.

---

## D8. Push keeps the disclosure posture the withdrawn design had, and pays a round trip for it

**Tension.** Matrix's push gateway is content-bearing by default: the homeserver
pushes event content to a gateway, which forwards it to APNs or FCM. The
withdrawn `add-nostr-push-gateway` design was built the opposite way — an opaque,
revocable capability instead of a device token, and one fixed phrase with no
content in every push.

**Decision.** Keep the disclosure posture. Pulse's pushers are configured
`event_id_only`, so a push carries an event identifier and a room identifier and
nothing else; the client resolves the content itself over an authenticated
connection.

**Why.** The posture was not incidental to the withdrawn design, it was its
point: in a deployment where message content is tenant data under an audit
regime, handing plaintext to Apple or Google to relay is a disclosure the
deployment did not agree to. Matrix supports `event_id_only` precisely for
encrypted rooms, so this is a supported configuration rather than a deviation.

**Accepted cost.** Every notification costs a round trip before it can be
rendered, and a device that is offline when the push lands shows a generic
notification until it can resolve the event. That is the same trade the withdrawn
design made.

**Open sub-tension.** The revocable-capability idea — a push credential the
platform can revoke without the device's cooperation — has no direct Matrix
equivalent; the pusher registration is the nearest thing. Whether pusher
deletion is a sufficient revocation story for the deployments that asked for the
original design is not settled here.

**Implied spec edits.** `matrix-push-and-notifications` requires `event_id_only`
pushers and client-side resolution, and states the revocation path.

---

## D9. Federation is specified and off by default

**Tension.** Federation is the property that most distinguishes Matrix from a
relay — and it is a trust boundary. An enterprise deployment holding tenant
revenue data does not want its rooms reachable from the public Matrix network by
default, and federation brings state resolution across servers, remote server
signing keys, and an ACL surface with it.

**Decision.** The federation API is specified and implemented, and the switch
defaults to off. A deployment that turns it on opts into the trust boundary
explicitly.

**Why not simply omit it.** Two reasons. Omitting it would make room versions and
the auth rules look like implementation detail rather than the wire contract they
are, and a homeserver written as though it will never federate tends to take
shortcuts in exactly the places state resolution later needs. And multi-node
Pulse deployments that want rooms shared across separately-administered
installations are a foreseeable ask; specifying federation now and defaulting it
off is cheaper than retrofitting it.

**Rejected: federate by default.** It is the Matrix-native answer and it is wrong
for this product's deployment model, for the same reason Decision 44 removed
per-identity rate limiting: the anonymous, adversarial surface those defaults
assume is not the surface this deployment has.

**Implied spec edits.** `matrix-protocol-core` states room versions and auth
rules as wire contract regardless of the switch.
`matrix-homeserver-runtime` states the federation listener and its default.
`configuration` carries the switch.
