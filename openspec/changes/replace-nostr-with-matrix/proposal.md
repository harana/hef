The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/replace-nostr-with-matrix/).

Status: Draft (2026-09-04)

## Why

Pulse's messaging, collaboration, and agent surface is built on Nostr. The
platform runs its own relay, speaks about fifty NIPs, and carries sixteen
protocol extensions that exist nowhere but the upstream Buzz implementation.
Decisions 43 and 44 committed to that: compatibility with Buzz as a wire
contract, satisfied by a native relay, with scope set to every NIP Buzz
implements.

That commitment is being withdrawn. The product is moving to Matrix — not as a
bridge behind a Nostr front, and not as a second protocol beside it, but as the
protocol the application is built on. This change removes Nostr from the
platform entirely and specifies what replaces it.

The case for the move is not that Nostr is badly implemented here. It is that
three of its properties are wrong for what Pulse is:

**A relay has no shared room state, so the platform has to invent it.** Nostr
events are independent signed records; a relay that wants a membership list, a
role, or a moderation decision has to hold that state outside the protocol and
defend it at every read. The `nostr-groups` and `nostr-relay-runtime`
capabilities are largely that defence — a p-gated subscription rule with six
enforcement chokepoints, relay-signed channel state, host-bound tenancy failing
closed. Matrix puts room state inside the protocol: state events keyed by
`(type, state_key)`, auth rules every server evaluates identically, power levels
as ordinary state, and state resolution as a specified algorithm. The work
becomes implementing a specification instead of designing one.

**Sixteen protocol extensions have exactly one implementation, and it is not
ours.** NIP-AA, AB, AE, AM, AO, AP, CW, DV, ER, GS, IA, MP, OA, PL, RS have no
second implementation, no independent test suite, and no standards process. The
porting discipline Decision 44 introduced — name the upstream source, transcribe
its tests, record every divergence — is the honest response to that, and it is
also an admission that the specification *is* the upstream source tree. Matrix
replaces the standard NIPs with a versioned specification that several
independent servers and clients implement, and turns the Harana-specific
extensions into custom event types in a reverse-DNS namespace, which is the
mechanism Matrix defines for exactly this.

**The client ecosystem is the product's leverage, and Matrix's is larger.** The
compatibility promise Decision 43 made was to stock Buzz clients and third-party
NIP-29 clients. Matrix's equivalent promise reaches Element and every
Matrix-spec client, and it comes with a maintained Apache-2.0 Rust client
library rather than a client surface Pulse writes and tests alone.

The cost is stated plainly and not minimised: this deletes roughly 77,600 lines
of working server code, rewrites four client applications, and gives up the
accumulated correctness of a relay that passes its conformance gate today. It is
a rebuild, not a refactor.

## What Changes

- **Every Nostr capability, crate, table, listener, and operator key is
  removed.** `add-nostr-nip-suite` and `add-nostr-push-gateway` are withdrawn
  unapplied — no `nostr-*` capability was ever promoted into `openspec/specs/`,
  so nothing is un-promoted. What *is* in the baseline and does need editing is
  the signed-event provenance family, the protocol-interoperability library
  inventory, and the six `buzz.*` operator keys. The `service/buzz` crate, the
  `buzz_push_gateway` module, the two WebSocket listener roles, the twenty-five
  `buzz_*` tables, and the upstream conformance gate all go.

- **The homeserver is native, on compio, inside the one product binary.** Pulse
  implements the Matrix Client-Server API itself rather than deploying Synapse or
  a Rust homeserver alongside. This is the same shape Decision 43 chose for the
  relay and it is chosen for the same reasons, plus two new ones: every
  embeddable Rust homeserver is more `tokio`-bound than the client library is,
  and
  Synapse is AGPLv3, which the dependency licence allowlist does not admit.
  Federation is specified but off by default — a deployment that turns it on
  opts into a trust boundary the enterprise default does not want.

- **Every Matrix crate the platform depends on is adopted, and none is forked.**
  `ruma`, `ruma-common`, `ruma-events`, `ruma-signatures`, `ruma-state-res`,
  `ruma-federation-api`, and `vodozemac` carry no async runtime, database driver,
  HTTP framework, or broker client in their transitive graphs, so they pass the
  existing mechanical adoption test unchanged and are the direct replacement for
  `buzz-core` and `buzz-sdk`. The client library is `harana/harana-matrix`,
  adopted on the same terms with its default features off.

- **The client library asks for its runtime, transport and storage, so the
  platform supplies all three.** It defines an `AsyncRuntime` interface of three
  methods — spawn, spawn-blocking, sleep — installed once at startup, with the
  built-in backend behind a feature whose own documentation says to turn it off
  and install your own. Its bundled HTTP client and its bundled file-database
  backend are both optional and stay off. So the platform's work is three
  implementations against stated interfaces, not a retargeting of somebody else's
  internals, and there is no fork to rebase on every upstream release.

- **The runtime ban is narrowed to what it was always about.** With every runtime
  feature off, the client library still names the runtime crate and two companions
  for synchronisation primitives, stream adapters, and future-combinator macros —
  none of which starts an executor. The adoption test matches crate families, so
  it would refuse the library for a reason unrelated to running a second runtime.
  The async-runtime clause therefore becomes a test of a crate's **resolved
  feature set** against a runtime-free allowlist. This narrows the rule rather
  than widening it: today's by-name sanctioned carriers are re-verified under the
  feature test rather than trusted, and the ban on a crate the platform writes
  naming a runtime crate directly is unchanged and unconditional.

- **The bundled storage backend stays off, and the four stores are implemented
  over the platform store.** The state store, encryption key store, event cache
  store, and media store — roughly 122 methods — are implemented over the `store`
  crate and the HEJ/HEF event tier, validated by the library's own conformance
  suites rather than by fresh tests. That keeps the storage rule the corpus
  already states: event bodies on the event tier, relational state limited to
  rebuildable derived projections.

- **Storage keeps the shape Decision 44 set, with one scheme added.** Every
  stored Matrix event is committed to HEJ/HEF under the tenant's
  commit-authority lease, with the signed-event provenance family carrying the
  event's own hashes and signatures so an archived event re-verifies offline
  years later. The provenance family's signature-scheme registry gains Ed25519
  alongside the BIP-340 entry it has today; the rest of the family is unchanged,
  because it was specified protocol-neutrally.

- **The sixteen Buzz-authored extensions are rebuilt as custom Matrix event
  types** under a `com.harana.*` reverse-DNS namespace — state events where the
  extension was replaceable, account data where it was per-user, and ordinary
  room events otherwise. Agent identity and owner attestation, agent memory,
  personas and teams, turn metrics, session observability, reminders, encrypted
  push leases, cross-device read state, and git collaboration all keep their
  product behaviour and change their wire encoding.

- **Sync is simplified sliding sync (MSC4186), and that is a homeserver
  requirement, not a client preference.** The client library's high-level stack —
  `RoomListService`, `EncryptionSyncService`, `Timeline` — runs MSC4186 only; the
  MSC3575 proxy variant no longer exists in the SDK. The native homeserver serves
  both MSC4186 and `/sync` v2, because third-party clients still use the latter.

- **The clients are rewritten in one release train, with no dual-protocol
  window.** Desktop, mobile, the Leptos browser client, and the legacy React
  client all lose their Nostr transport, kind registries, and hand-written NIP-44
  cryptography. There is no bridge and no compatibility shim: between the removal
  landing and the interop gate going green, the product has no messaging. The
  phase gate is the interop suite, not a date.

## Capabilities

### New Capabilities

- `matrix-protocol-core` — the event model every other Matrix capability builds
  on: event identity and content hashes, Ed25519 signatures, room versions, the
  room DAG, auth rules, state resolution, redactions, and the namespacing rule
  for custom event types.
- `matrix-homeserver-runtime` — the homeserver as a capability module on the
  platform runtime: listener lifecycle, host-bound tenancy, the Client-Server API
  surface, simplified sliding sync, cross-node fan-out over the peer transport,
  ingest admission, and the event-tier write path under the commit-authority
  lease.
- `matrix-identity-and-auth` — Matrix user identifiers, server discovery,
  next-generation authentication, device and session management, and the
  custodial signing keys that bind a Matrix identity to a platform identity.
- `matrix-rooms-and-spaces` — rooms and the spaces that organise them,
  membership and the join rules that admit it, power levels, moderation, policy
  lists, and room upgrades.
- `matrix-messaging-and-threads` — messages, threads, replies, edits, reactions,
  read receipts, typing, and presence.
- `matrix-encryption` — end-to-end encryption: Olm and Megolm sessions, device
  verification, cross-signing, key backup, and secret storage.
- `matrix-media-repository` — uploading, storing, and serving files and their
  derived renditions on the platform object store, behind authenticated media
  endpoints.
- `matrix-push-and-notifications` — push rules, pushers, and the push gateway
  that wakes a mobile device without disclosing what the message said.
- `matrix-workspace-extensions` — the Harana-specific behaviour that has no
  Matrix equivalent, carried as custom event types in one namespace: agent
  identity and attestation, agent memory, personas and teams, turn metrics,
  session observability, reminders, and cross-device read state.
- `matrix-git-collaboration` — repositories, patches, and issues carried as room
  events, commit signing with workspace keys, and projects that gather
  repositories under one heading.
- `matrix-client-runtime` — the adopted client library and the three things the
  platform hands it: the runtime its background work runs on, the HTTP client its
  requests go out over, and the four stores its state is kept in.
- `matrix-interop-conformance` — the crate-adoption test and its feature-based
  runtime rule, the divergence register, and the release-blocking conformance and
  stock-client interop gates.

### Modified Capabilities

- `configuration` — removes the six `buzz.*` operator keys and
  `network.buzz_relay_bind`, and adds the homeserver's own listener bind, server
  name, federation switch, and signing-key reference.
- `event-service` — restates the signed-protocol-event commit path without the
  Decision 43 reference, and admits Ed25519-signed Matrix events on the same
  verify-before-append precondition.
- `hef-logical-event-model` — adds Ed25519 to the provenance family's
  signature-scheme registry and removes the BIP-340 parenthetical, leaving the
  column family otherwise unchanged.
- `http-api` — replaces the `node_buzz_relay` and `node_buzz_pair_relay` listener
  roles with `node_matrix_homeserver`, keeps WebSocket restricted to that role,
  and carries forward the streaming request-body path with its transport-enforced
  size ceiling.
- `system-architecture` — replaces the protocol-interoperability library
  inventory: the four Nostr entries and the eight-row refusal table give way to
  the adopted `ruma` and `vodozemac` family and the adopted client library with
  the feature set it is built under. Adds the feature-based runtime rule and the
  rule that a crate offering pluggable interfaces is adopted rather than forked.

### Withdrawn Changes

`add-nostr-nip-suite` and `add-nostr-push-gateway` — archived under
`openspec/changes/archive/2026-09-04-withdrawn-nostr-surface/`, each with a
`WITHDRAWN.md` naming what carried forward and what did not.

## Impact

- **Decision register.** Adds Decision 45, which reverses Decisions 43 and 44
  rather than extending them. What stands from both: INV-RUNTIME and its
  mechanical enforcement, the adoption test unchanged in form and applied to a
  new crate family, the porting discipline and its divergence register applied to
  the client library, the storage rule that event bodies live on the event tier,
  the split
  of what Redis used to carry, and the removal of per-identity rate limiting.
  What falls: Buzz wire compatibility as the compatibility target, the NIP
  coverage matrix as the scope instrument, the native-relay decision, and the
  upstream conformance gate that clones `harana-oss/buzz`.

- **Scale, stated honestly.** This is larger than `add-nostr-nip-suite` was,
  because it carries a removal as well as a build. The server side is roughly
  77,600 lines deleted across 383 files, twenty-five tables dropped, and two
  listener roles retired. The client side is four applications: 475 files in
  desktop touch the Nostr surface, 128 in mobile, and mobile's NIP-44
  implementation is hand-written cryptography rather than a library call. Twelve
  capabilities are new.

- **No messaging between removal and parity.** The single release train is a
  deliberate choice against a dual-protocol window, and its cost is an outage of
  the messaging surface for the duration. The mitigation is phasing with the
  interop gate green at every phase boundary, not a shorter schedule.

- **A retained go/no-go, much reduced.** Decision 43's compio WebSocket-upgrade
  gate stands resolved green and is not re-litigated. What replaces the transport
  go/no-go this change originally carried is smaller and different in kind: the
  client library states its runtime, transport, and store interfaces, so Phase 1
  proves the three platform implementations satisfy them — a real gate, but one
  with a stated contract to satisfy rather than an unknown amount of somebody
  else's crate to rewrite. The dependency-policy work is its own gate: the
  feature-based runtime rule has to be implemented and every existing by-name
  carrier re-verified under it before the library enters the graph.

- **Push privacy changes posture, and it is a decision not an accident.** The
  withdrawn push-gateway design carried one fixed phrase and no content. Matrix's
  push gateway is content-bearing by default. `matrix-push-and-notifications`
  keeps the privacy posture by specifying `event_id_only` push and client-side
  resolution, which costs a round trip on every notification.

- **Not covered.** Federation between Pulse deployments and the public Matrix
  network is specified but not enabled, and no bridge to any other protocol —
  Nostr included — is in scope. Application services are named as the extension
  mechanism the agent surface uses, not specified as a public integration
  surface.
