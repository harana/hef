The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/replace-nostr-with-matrix/).

# Tasks

Nine phases in dependency order. The conformance and stock-client interop gates
run at every phase boundary against the surface claimed so far; a phase does not
close on a red gate, and no phase closes on a date.

Phase 1 is a gate. Nothing in Phase 2 onward starts until the dependency rule is
narrowed, the client library is in the graph under a named feature set, and the
three platform implementations satisfy its runtime, transport and store
interfaces. A failure there reopens the client decision as a reviewed change
rather than admitting a second async runtime to the product graph.

## 0. Record the reversal and write the specs

- [ ] 0.1 Add Decision 45 to `docs/design-review-decisions.md` — reverses
  Decisions 43 and 44, naming what stands from each and what falls, in the
  register's tension/decision/why/rejected-alternatives style.
- [ ] 0.2 Archive `add-nostr-nip-suite` and `add-nostr-push-gateway` under
  `openspec/changes/archive/2026-09-04-withdrawn-nostr-surface/`, each with a
  `WITHDRAWN.md` naming what carried forward and what did not.
- [ ] 0.3 Write the twelve capability spec deltas plus the `configuration`,
  `event-service`, `hef-logical-event-model`, `http-api`, and
  `system-architecture` modifications.
- [ ] 0.4 Run `openspec validate replace-nostr-with-matrix --strict` and fix
  every issue. Check separately for requirement text dangling after a scenario
  with no `### Requirement:` heading — validate does not catch that class. Note
  that strict validation only reads the **first line** of a requirement body for
  its `SHALL`/`MUST` check, so a hard wrap that pushes the keyword to line two
  fails.

## 1. The dependency rule, crate admission, and the client library

- [ ] 1.1 Narrow the async-runtime clause from a crate-name match to a resolved
  feature test. Read each package's resolved feature set from the dependency
  graph, classify a runtime family's features into a runtime-free allowlist
  (synchronisation primitives, channel and stream adapters, future-combinator
  macros) and everything else, and refuse an unclassified feature rather than
  admitting it. Apply the same rule in `cargo xtask adoption-test` and
  `cargo xtask runtime-inversion`. Implements `system-architecture` "The runtime
  ban is on starting a second runtime, not on a crate name".
- [ ] 1.2 Re-verify every by-name sanctioned carrier and introducer under the new
  rule. One that resolves only runtime-free features loses its named exception
  and passes on the mechanical property; one that resolves an executor, reactor
  or timer feature keeps a named exception recording why. The direct-dependency
  ban on workspace crates is unchanged and unconditional.
- [ ] 1.3 Admit the runtime-free protocol crates. Add `ruma` and its
  `ruma-common`, `ruma-events`, `ruma-signatures`, `ruma-state-res`, and
  `ruma-federation-api` members, plus `vodozemac`, to the workspace; confirm each
  passes `cargo xtask adoption-test` over its full transitive graph; record them
  in the `system-architecture` library inventory as pinned with
  `pinned-crates.toml` entries. Implements `system-architecture` "Adopted
  protocol crates are pinned wire-format dependencies".
- [ ] 1.4 Add `harana/harana-matrix` as a pinned git dependency with **default
  features off** and an explicitly named feature set that selects neither the
  built-in runtime backend, nor the bundled HTTP client, nor the bundled
  file-database backend, and neither the local-server nor the single-sign-on
  feature — both of which bind a TCP listener through an HTTP framework the
  policy refuses. Record the pin and the feature set in `pinned-crates.toml` and
  the library inventory. Implements `system-architecture` "An upstream crate
  carrying a runtime interface is adopted, not forked" and `matrix-client-runtime`
  "The pin, the feature set, and the upstream difference are recorded".

  The library git-pins its own protocol and cryptography dependencies, so
  `deny.toml`'s git allowlist gains those sources as well as the library's own —
  four entries, not one. Confirm the versions those pins resolve to are the ones
  task 1.3 admitted, rather than a second copy.
- [ ] 1.5 Establish that the chosen feature set compiles at all. The library's
  own continuous integration builds nothing with the runtime backend and the
  bundled HTTP client both off, so this configuration has never been compiled
  upstream. Budget a round of compile-error fixing, and contribute the fixes and
  a job that builds the configuration back to the library, so it stays compiling.
- [ ] 1.6 Implement the library's runtime interface over compio and install it
  during node startup on a path that cannot be skipped, before any library call
  that spawns work or sleeps — the library installs a default lazily on first use
  and refuses a later installation, so a late install is a startup failure rather
  than a fallback. Spawn a detached future, run a blocking closure on the
  platform's blocking-worker pool rather than inline, and supply a timer.
  Implements `matrix-client-runtime` "The platform supplies the client's runtime,
  and no second one is compiled in".
- [ ] 1.7 Build the thread-model bridge once, and state which reactor library
  work lands on. The library's interfaces require futures that are safe to move
  between threads; compio's are not, and its spawn is thread-local. Define the
  binding explicitly rather than letting it fall out of which thread polled the
  caller, and measure the added latency per timer and per request against the
  platform's own budget. Implements `matrix-client-runtime` "The runtime bridge
  states which reactor work lands on".

  This is the largest unmodelled risk in the phase: everything else has a stated
  contract to satisfy, and this does not.
- [ ] 1.8 Implement the library's transport interface over the platform's own
  HTTP client, covering the Client-Server API, media, and authentication, and
  compile out any library feature that cannot be served through it — recording
  each omission in the library inventory. Implements `matrix-client-runtime` "The
  platform supplies the client's HTTP transport".

  The interface is fully buffered — an owned request body in, an owned response
  body out, with no streaming body type — so it composes with the platform
  client's owned-buffer I/O without an adapter. The consequence to carry into
  Phase 4 is that a client-side media upload is buffered whole in memory, which
  bounds how large an upload a client can make and is a different limit from the
  homeserver's streaming inbound path.
- [ ] 1.9 Record the divergence register rows for the client library's
  differences from its upstream project, with their upstream status, and wire the
  check that every diverging module has a row and every cited path exists.
  Implements `matrix-interop-conformance` "The divergence register covers the
  client library and is checked against the tree".

## 2. Protocol core and the homeserver substrate

- [ ] 2.1 Implement event identity, canonical serialisation, signing, and
  verification through the adopted crates, with no second implementation.
  Implements `matrix-protocol-core` "One implementation of event identity,
  signing, and verification".
- [ ] 2.2 Implement the room graph, the room versions the platform creates and
  joins, the authorization rules, and state resolution. Implements
  `matrix-protocol-core` "Rooms are event graphs resolved by a stated room
  version" and "Authorization is the room's own rules, evaluated before
  acceptance".
- [ ] 2.3 Implement redaction per the room version's algorithm, and keep
  irrecoverable erasure as a separate audited event-tier operation. Implements
  `matrix-protocol-core` "Redaction removes content and keeps the record".
- [ ] 2.4 Add the Harana event-type registry and the namespacing rule, with
  ingest refusing an unregistered type under the prefix. Implements
  `matrix-protocol-core` "Harana event types live in one reserved namespace" and
  `matrix-workspace-extensions` "Every Harana extension type is registered,
  versioned, and namespaced".
- [ ] 2.5 Bind the `node_matrix_homeserver` listener role, declare it in
  `ports.rs`, and retire `node_buzz_relay` and `node_buzz_pair_relay`. Implements
  `http-api` "Public HTTPS TLS refuses; peer transport rides libfabric".
- [ ] 2.6 Add the streaming request-body path with the per-route ceiling enforced
  at the transport on bytes received. Implements `http-api` "Streaming request
  bodies with the size ceiling enforced at the transport".
- [ ] 2.7 Implement host-derived tenancy binding that fails closed, and the
  cross-tenant isolation of rooms, events, accounts, and media. Implements
  `matrix-homeserver-runtime` "Tenancy binds from the request host and fails
  closed".
- [ ] 2.8 Implement the event-tier write path under the tenant's
  commit-authority lease, with forwarding from non-lease nodes and no event
  content in the relational tier. Implements `matrix-homeserver-runtime`
  "Accepted events commit through the event tier under the tenant's lease".
- [ ] 2.9 Extend the signed-event provenance family's scheme registry with
  Ed25519, including the byte-length checks, and record which signature was
  verified at ingest. Implements `hef-logical-event-model` "Signed-event
  provenance column family" and `event-service` "Signed protocol events commit
  through the event tier".
- [ ] 2.10 Implement cross-node fan-out as advisory peer notifications with
  event-tier recovery, and presence, typing, and the replay-protection set as
  unlogged store rows. Implements `matrix-homeserver-runtime` "Cross-node
  fan-out is advisory, and the event tier is the truth".

## 3. Identity, rooms, and the message surface

- [ ] 3.1 Derive Matrix identities from platform identities, refuse open
  registration, and revoke devices and signing on deprovisioning. Implements
  `matrix-identity-and-auth` "A Matrix identity is derived from a platform
  identity, never registered separately".
- [ ] 3.2 Serve the server-discovery documents from the configured server name.
  Implements `matrix-identity-and-auth` "Server discovery is explicit and does
  not depend on the bind address".
- [ ] 3.3 Establish Matrix devices from authenticated platform sessions, with
  device listing and revocation tied to the platform session. Implements
  `matrix-identity-and-auth` "Sign-in issues a device, and the platform session
  governs it".
- [ ] 3.4 Hold signing keys custodially in the platform's secret handling, with
  every custodial signing recorded in the audit trail and unattributable signing
  refused. Implements `matrix-identity-and-auth` "Signing keys are custodial,
  held by the platform, and their use is audited".
- [ ] 3.5 Register application services with explicit namespaces as the only
  privileged extension path. Implements `matrix-identity-and-auth` "Application
  services are the only privileged extension path".
- [ ] 3.6 Implement rooms with membership, join rules, power levels, and
  moderation as room state, with derived projections that are never the
  authority. Implements `matrix-rooms-and-spaces` "A room's membership and
  permissions are room state, not a side table" and "Joining is governed by join
  rules, and invitations are explicit events".
- [ ] 3.7 Implement spaces as the workspace hierarchy, with listings that
  disclose nothing a member could not join. Implements `matrix-rooms-and-spaces`
  "Spaces carry the workspace structure that a relay address used to carry".
- [ ] 3.8 Implement moderation through power levels and policy lists, with
  audit-trail records that survive redaction. Implements
  `matrix-rooms-and-spaces` "Moderation is power levels plus policy lists, and
  both are auditable".
- [ ] 3.9 Implement room upgrades with tombstone and predecessor references, and
  no event copying. Implements `matrix-rooms-and-spaces` "A room upgrade
  preserves continuity for members and for the archive".
- [ ] 3.10 Implement messages, replacements, and annotations on the protocol's
  own relations. Implements `matrix-messaging-and-threads` "Messages, edits, and
  reactions use the protocol's own relations".
- [ ] 3.11 Implement threads with server-side summaries updated in the same
  commit as the event that changes them. Implements
  `matrix-messaging-and-threads` "Threads are first-class and their summaries are
  derived, not stored by clients".
- [ ] 3.12 Implement read receipts, read markers, private receipts, and
  server-computed unread counts. Implements `matrix-messaging-and-threads` "Read
  state is per-member, cross-device, and private by default".
- [ ] 3.13 Implement typing and presence as ephemeral state that does not
  survive a restart as fact and is readable in bulk. Implements
  `matrix-messaging-and-threads` "Typing and presence are ephemeral and never
  durable".
- [ ] 3.14 Implement both sync surfaces and the capability advertisement that
  lets a client choose between them. Implements `matrix-homeserver-runtime` "Both
  sync surfaces are served, and the capability document says so".

## 4. Encryption, storage, media, and push

- [ ] 4.1 Implement the four store traits — state, crypto, event cache, media —
  over the platform's relational store, event tier, and object store, and run the
  upstream conformance suite for each against them. Implements
  `matrix-client-runtime` "The four stores are implemented over the platform's
  own storage" and `matrix-encryption` "The key store's durability obligations
  are stated and tested".
- [ ] 4.2 Enable encryption by default on created rooms, refuse downgrade, and
  make server-side readers participate as devices rather than bypass the room.
  Implements `matrix-encryption` "Encryption is on by default and cannot be
  silently turned off".
- [ ] 4.3 Cross-sign each new device at sign-in so a first device is usable
  without a second, and record interactive verification outcomes durably.
  Implements `matrix-encryption` "Devices are cross-signed, and verification is
  available without a second device".
- [ ] 4.4 Implement mandatory key backup with the recovery secret in the
  platform's secret handling, and back a room key up before its message is
  acknowledged. Implements `matrix-encryption` "Key backup is mandatory, and
  losing a device does not lose history".
- [ ] 4.5 Confirm no hand-written cryptography remains in any client — the
  mobile client's own encryption, key-derivation, and key-agreement modules are
  deleted, not ported. Implements `matrix-encryption` "One implementation of the
  encryption primitives, and it is the adopted one".
- [ ] 4.6 Implement authenticated media upload and download on the object store,
  with membership-based authorization and content-determined type admission.
  Implements `matrix-media-repository` "Media endpoints are authenticated, and an
  unauthenticated fetch is not served" and "Uploads are bounded at the transport
  and stored on the object store".
- [ ] 4.7 Implement derived renditions with an allocation ceiling, regenerable
  from the blob and never rewriting the event. Implements
  `matrix-media-repository` "Renditions are derived, regenerable, and never
  rewrite the event".
- [ ] 4.8 Implement media retention following the room, with erasure reaching
  the blob and every rendition. Implements `matrix-media-repository` "Media
  retention follows the room, and erasure reaches the bytes".
- [ ] 4.9 Implement identifier-only push with client-side resolution, refusing
  content-bearing registrations. Implements `matrix-push-and-notifications` "A
  push carries an identifier, never content".
- [ ] 4.10 Implement server-side notification rules with per-member changes that
  apply to every device, and muted rooms that reach no provider. Implements
  `matrix-push-and-notifications` "Notification rules decide what is worth a
  push, and a member can change them".
- [ ] 4.11 Implement platform-side push revocation independent of the device,
  and drop pushes that cannot be attributed to a live device and session.
  Implements `matrix-push-and-notifications` "A push registration is revocable by
  the platform without the device".
- [ ] 4.12 Build the push gateway as a platform component with provider
  credentials from the secret handling, delivery metrics, and unregistration of
  permanently-invalid tokens. Implements `matrix-push-and-notifications` "The
  push gateway is a platform component, and its failures are visible".

## 5. Workspace extensions and git collaboration

- [ ] 5.1 Implement agent identity and owner attestation as verifiable room
  state, with agent access bounded by the owner's entitlements. Implements
  `matrix-workspace-extensions` "An agent has its own identity and a verifiable
  attestation to its owner".
- [ ] 5.2 Implement agent memory as encrypted room events scoped by membership,
  with scope boundaries that an index lookup cannot cross. Implements
  `matrix-workspace-extensions` "Agent memory is encrypted, scoped, and readable
  only by its own participants".
- [ ] 5.3 Implement personas and teams as state events with explicit sharing
  scopes that a reference does not confer. Implements
  `matrix-workspace-extensions` "Personas and teams are room state, and their
  sharing rules are explicit".
- [ ] 5.4 Implement turn metrics carrying no message content, and live session
  observability that does not outlive the session. Implements
  `matrix-workspace-extensions` "Turn metrics and session observability are
  ephemeral or retained by policy, never silently durable".
- [ ] 5.5 Implement reminders and extended read positions as per-member account
  data, invisible to the room and never firing early. Implements
  `matrix-workspace-extensions` "Reminders and cross-device read positions are
  per-member state that follows the member".
- [ ] 5.6 Implement repository, patch, and issue events with derived projections
  and ingest refusal for an unknown repository. Implements
  `matrix-git-collaboration` "Repositories, patches, and issues are room state
  and room events".
- [ ] 5.7 Implement commit signing with the member's workspace identity, showing
  an unverifiable signature as unverified. Implements
  `matrix-git-collaboration` "Commits are signed with the member's workspace
  identity".
- [ ] 5.8 Move repository objects to the object store and bound the git
  transport as bytes arrive. Implements `matrix-git-collaboration` "Repository
  contents live on the object store, not a node's filesystem".
- [ ] 5.9 Implement projects as state events referencing repositories and their
  discussion room, with deletion that does not remove the repositories.
  Implements `matrix-git-collaboration` "Projects gather repositories and bind
  them to where they are discussed".

## 6. Client migration

- [ ] 6.1 Rewrite the desktop client's transport, event model, and identity
  surface onto the adopted client library. Delete `shared/api/relayClient.ts` and the relay
  modules around it, `shared/constants/kinds.ts`, `shared/lib/nostrUtils.ts`, and
  the Tauri-side protocol crates' kind registry, event, filter, and verification
  modules. Remove the `nostr-tools` dependency, noting it is currently declared
  as a development dependency but imported from `src/`.
- [ ] 6.2 Rewrite the mobile client's transport onto the adopted client library.
  Delete
  `lib/shared/relay/` and `lib/shared/crypto/`, and remove the `nostr` and
  `pointycastle` dependencies; the hand-written encryption there is deleted, not
  ported.
- [ ] 6.3 Rewrite the browser client's protocol module onto the adopted client
  library's browser target. Delete `src/nostr/` and the schnorr signer, and remove the
  relay-URL derivation and build-time relay flag. Note this workspace is covered
  by neither the repository hook configuration nor the package workspace, so it
  needs its own check step.
- [ ] 6.4 Remove the legacy browser client's protocol modules and its repository
  and invite callers.
- [ ] 6.5 Replace community-is-a-relay-address with homeserver-and-space
  identity in all three live clients, including the persisted-state migration
  each already carries one of.
- [ ] 6.6 Wire push registration in the mobile client — there is none today, so
  this is new work rather than a change: the platform's iOS target requests badge
  permission only and neither target has a messaging service.

## 7. Removal and the gates

- [ ] 7.1 Delete the server protocol surface: `crates/service/buzz`,
  `crates/service/notifications/src/buzz_push_gateway`,
  `tools/buzz-backend-kubernetes`, the relay and pair-relay modules in
  `crates/api` and `crates/app`, and the relay-fixture xtask.
- [ ] 7.2 Drop the twenty-five `buzz_*` tables and their migration sets, and
  remove the relay's rows from the platform secret store. Rewrite the one
  cross-capability reader — the knowledge search module's raw SQL against the
  group membership and channel tables — against the new membership projection.
- [ ] 7.3 Remove the protocol dependencies: the `nostr` crate, the three
  `buzz-*` git dependencies, their `deny.toml` git-allowlist entry, and their
  `pinned-crates.toml` records. Keep the pins that survive on other callers.
- [ ] 7.4 Remove the operator keys and their generated documentation at the
  registry rather than in the generated file, and remove the relay entries from
  the environment example and the local compose stack, including the cache
  server that no longer serves anything.
- [x] 7.5 Remove the retired gates: the upstream relay conformance script and
  its pinned clone of the upstream source, the coverage matrix, the relay legs of
  the local conformance script, and the relay recipes and release train in the
  task runner. Implements `matrix-interop-conformance` "A phase closes on a green
  gate, and the retired protocol's gates go with it".
- [ ] 7.6 Update the capability ownership map: remove the three Nostr rows and
  add the twelve Matrix capabilities with their owning crates and modules.
- [ ] 7.7 Retire the divergence register's Nostr-port rows, leaving only the
  client library's own, and confirm the check still passes against the reduced
  register. Implements `matrix-interop-conformance` "The divergence register
  covers the client library and is checked against the tree".
- [ ] 7.8 Update the architecture documents and the interface analysis documents
  that describe the retired protocol, and remove the mockup that frames the
  product on it.
- [x] 7.9 Stand up the independent conformance suite in its own workspace with
  its own lockfile, outside the product graph. Implements
  `matrix-interop-conformance` "Protocol conformance is proven by an independent
  suite, not by our own tests".
- [x] 7.10 Stand up the stock-client interop gate with a leg for each sync
  surface. Implements `matrix-interop-conformance` "A stock third-party client is
  driven against a live deployment in the release gate".

## 8. Verification

- [ ] 8.1 Run `cargo xtask adoption-test`, `cargo xtask runtime-inversion`,
  `cargo xtask capability-map`, and `cargo xtask divergence-register`, and fix
  every failure.
- [ ] 8.2 Run the conformance suite and the stock-client interop gate green
  against the full claimed surface.
- [ ] 8.3 Confirm no reference to the retired protocol remains outside the
  archive: no crate, table, route, operator key, dependency, script, task-runner
  recipe, or generated document.
- [ ] 8.4 Run `openspec validate replace-nostr-with-matrix --strict`.
