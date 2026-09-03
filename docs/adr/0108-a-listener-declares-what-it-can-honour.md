# ADR 0108 — A listener declares what it can honour, and a refusal outranks reassurance

- **Status:** Accepted — implemented and tested
- **Date:** 2026-09-02
- **Issue:** #589
- **Extends:** [ADR 0005](0005-lan-view-profile.md), [ADR 0002](0002-versioned-api-contract.md)
- **Supersedes / superseded by:** —

## Context

Git Vista deliberately builds two different HTTP routers. The loopback listener
gets the complete route table. ADR 0005's LAN listener gets a structurally
read-only table: its write, repository-selection, plan, preview, and
write-outcome routes are not gated at runtime; they are never registered.

That boundary is correct. The live defect was on the other side of it:

```text
listener          POST /api/select
127.0.0.1         401 — route exists; caller is unauthenticated
LAN address       405 — route does not exist; static fallback answered
```

The browser nevertheless rendered every repository as an actionable button.
A click opened the next screen, `POST /api/select` received an ordinary 405,
and the user ended up looking at a not-connected state. The console was empty
because `fetch` rejects only for transport failures; an HTTP 405 is a fulfilled
response.

The existing `SessionInfo.via_lan` flag did not solve the contract problem. It
describes how a session was established. The route builder's `full_routes`
value describes what the listener can honour. They happen to correspond in the
two current constructors, but deriving a capability from network topology or a
session fact gives the client a second source of truth. It also leaves the
decision in `picker.rs`, which is compiled only for wasm32 and executed by no
test runner in this repository.

A second live reproduction exposed the same false-confidence shape outside the
picker. A 405 from `POST /api/plan` entered the preview panel's generic failure
arm, followed by:

> the operation … is unchanged and still available

The response had established the opposite for this listener. A capability
refusal may not be followed by reassurance that outruns it.

## Decision

### 1. The listener declares a closed route-capability profile

Every listener response carries:

```text
X-Git-Vista-Listener-Profile: full
```

or:

```text
X-Git-Vista-Listener-Profile: read-only
```

`ListenerProfile` and `LISTENER_PROFILE_HEADER` live in the pure
`git-vista-protocol` crate. Unknown or missing values never default to `full`.
The server derives the value with
`ListenerProfile::from_write_routes(full_routes)` inside the same function
whose `if full_routes` branch registers the complete table. It does not inspect
Host, peer address, or `SessionState::via_lan` to reconstruct the answer.

The header is applied both to the API router and to the assembled app. The
second placement matters: an absent LAN write route falls through to the static
service, so its 405 is outside the API router and must still carry the
listener's declaration.

This is protocol v11. A v10 client ignores the declaration and can reproduce
the live-looking-button defect against a v11 server. That is semantic contract
skew even though the catalog JSON itself did not change, so the compatibility
window moves whole instead of allowing the old client through negotiation.

### 2. Catalog data and the declaration become one picker input

`fetch_catalog` reads the profile header before consuming the catalog body and
returns one `RepositoryCatalog { repositories, listener_profile }`. The picker
therefore cannot render repository data first and decide its action state later
from an unrelated session store. If the declaration is absent or unknown, the
catalog fetch fails visibly and no repository control is constructed.

The pure, host-tested `listener_policy` module maps the profile to one of two
closed answers:

```text
full       -> RepositorySelection::Offered
read-only  -> RepositorySelection::Unavailable { visible notice }
```

The wasm-only view arranges that answer:

- a full-profile row is an enabled button;
- a read-only row is disabled and visibly says “Read-only LAN view — open the
  loopback link to switch repositories”;
- the action-shaped repository map is not offered for a read-only profile;
- Delete, Clone URL, and Rescan remain absent there, now from the declaration
  rather than from `via_lan` as a proxy.

The catalog still lists every repository on the LAN view. Read-only means the
information is useful but switching the server-side session selection is not an
available action.

### 3. A 405 is classified at both write funnels

Declaration is the honest surface, not a substitute for handling the answer.
The shared retrying write transport inspects every completed response before
returning it to an endpoint. Status 405 is converted to a visible refusal that
names the route, the profile declared on that response, and the fact that the
operation is unavailable there. A `read-only` declaration carries the
loopback remedy. A `full` or missing/unknown declaration reports the mismatch
and asks for a reload without falsely claiming that switching listeners will
fix it.

Three read-shaped POSTs (`/api/plan`, `/api/preview`, and explicit diff) do not
use that retrying transport. They already converge on `user_facing_error`, so
the same pure classifier is applied there. A route-less fallback response has
no API request id; that case is logged explicitly instead of preserving the
empty console that hid #589.

Other HTTP statuses retain the server's structured message. In particular, 401
is still authentication, 403 is still an authorization or repository-mode
refusal, and 404 is not silently rewritten as a listener-profile claim.

### 4. Preview request failure copy makes no availability claim

A successfully computed `PreviewOutcome` remains advisory and may keep the
existing explanation that the preview does not gate execution. A failed
request is a different state. Pending copy now says only that the client is
checking whether the listener can provide a preview. Failed copy reports the
failure reason and stops.

The copy comes from the host-tested policy module. Its `/api/plan` 405 test
assembles the exact rendered failure line and asserts both that it says
`unavailable` and that neither “still available” nor “ready either way” can
appear. The DOM module contains no second sentence that can contradict it.

## Why both halves are required

The declaration and the 405 classifier close different failures:

```text
profile declaration -> prevents a control that can only fail from looking live
405 classification   -> makes any stale, missed, or future control fail visibly
```

Declaration alone fixes the picker but leaves every other omitted route able to
fail as an ordinary, easily flattened response. Classification alone improves
the click's aftermath while preserving a button whose only possible outcome is
refusal. Neither is an honest interface by itself.

## Alternatives considered

### Declare the profile only

Rejected. It prevents this picker click but provides no defence for another
control accidentally wired to any of the many routes under `full_routes`.
The next omission would again depend on every caller remembering that HTTP
errors do not reject `fetch`.

### Handle 405 only

Rejected. The user would receive a better message only after pressing a button
that the client already knew could never work. A control is a claim that an
action is available; error handling cannot make that initial claim honest.

### Continue deriving capability from `SessionInfo.via_lan`

Rejected. Transport provenance and route capability are different facts. The
router already owns the authoritative switch, and duplicating its meaning in a
session mapping lets the declaration and the table drift. It also leaves the
picker decision hidden behind `cfg(target_arch = "wasm32")`.

### Put `profile` inside the catalog JSON or add a capabilities endpoint

Rejected in favour of the response header. A listener profile applies to the
whole route table, not to repository records, and an extra endpoint creates
another request that must finish before the catalog is safe to render. The
header makes the answer atomic with the catalog response and is also present on
the absent-route fallback that most needs to explain itself.

### Send a list of every route or every UI control

Rejected. A route list makes transport names the frontend's feature model and
requires a hand-maintained mapping from URLs to controls. A list of controls
reverses the coupling and makes the server know DOM concepts. The two closed
profiles match the router's one structural branch; the 405 defence covers
future omissions without duplicating that route list.

### Register `/api/select` (or `/api/plan`) on the LAN router and refuse inside

Rejected. ADR 0005's security property is structural absence: a later mode-gate
regression cannot reopen a handler that was never put in the router. The LAN
router is right and remains unchanged.

### Disable rows without visible copy, or explain with a tooltip

Rejected. A disabled unexplained row is another apparent no-op, and a tooltip
is not a disclosure on touch or assistive technology. The remedy is visible
inside every disabled row.

### Treat 405 as an expired session

Rejected. Loopback's 401 and LAN's 405 are measured, different answers. Turning
the latter into sign-out discards the correct machine fact and confidently
invents an authentication failure.

## Verification

The decision lives outside wasm-only modules. Native tests execute the profile
mapping, picker policy, 405 classifier, and preview failure copy; wasm checking
then verifies that the DOM and `gloo-net` glue consume those answers.

| Invariant | Executable check |
|---|---|
| `full_routes` and the response declaration use one closed mapping | `listener::tests::the_route_switch_and_header_declaration_are_one_mapping` |
| Unknown declarations fail closed | `listener::tests::an_unknown_or_weakened_declaration_never_defaults_to_full` |
| Real loopback/LAN route tables declare `full`/`read-only` | `tests::the_loopback_router_still_has_write_routes_registered`; `tests::the_lan_router_has_no_write_routes` |
| The measured 401/405 pair and the header cannot disagree | `tests::select_route_presence_and_listener_declaration_cannot_disagree` |
| A read-only picker policy is disabled and carries visible remedy text | `listener_policy::tests::a_read_only_profile_never_offers_repository_selection_and_says_why`; `listener_policy::tests::the_picker_wires_the_policy_to_both_action_and_explanation` |
| Only 405 becomes this capability refusal, and both POST funnels ask that classifier | `listener_policy::tests::only_method_not_allowed_becomes_a_listener_capability_refusal`; `listener_policy::tests::both_wasm_post_funnels_consult_the_host_tested_405_classifier` |
| `/api/plan` 405 cannot be followed by availability reassurance in either policy or DOM glue | `listener_policy::tests::a_plan_405_can_never_be_followed_by_an_availability_promise`; `listener_policy::tests::the_failed_preview_arm_cannot_append_reassurance_behind_the_policy` |

Local verification before mutation proof:

- `cargo test -p git-vista`: **810 passed, 2 ignored** in the real
  `git-vista-ui` binary; the 0-test `lib.rs` target is reported separately and
  was not used as evidence.
- `cargo test -p git-vista-protocol`: **229 passed** across unit/integration
  targets, with one ignored doctest.
- `cargo test -p git-vista-server`: **1,138 passed, 6 ignored** across its
  binary/integration targets; the three named router/profile tests pass
  against real Axum routers.
- `cargo check -p git-vista --target wasm32-unknown-unknown` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` and the corresponding
  wasm32 frontend clippy pass.
- `trunk build` passes and produces the real wasm bundle.
- `cargo fmt --all -- --check` and `git diff --check` pass.

### Mutation matrix

`failure-atlas mutation_check` ran against committed HEAD `600ee784`. Every
unmutated baseline was green, every edit applied exactly once inside the
atlas's contained clone, and every mutated leg reached and failed an assertion;
no compiler failure is counted as a catch. The atlas reported the source tree
as dirty solely because of the user's pre-existing untracked `.grok/`
directory. It cloned the recorded HEAD, and no tracked implementation or test
change was pending.

| Invariant | Remove mechanism | Weaken / misroute | Result |
|---|---|---|---|
| Read-only profile does not offer repository selection and carries the remedy | map `ReadOnly` to `Offered` | replace the row's `disabled=!can_select` with `disabled=false` | caught/caught — records 155–156; the policy assertion and wasm-seam assertion fail independently |
| A 405 becomes a visible listener refusal, while other statuses retain their meaning | make the classifier always return false | widen it to every status ≥400 | caught/caught — records 157–158; 405 disappears in the first and ordinary statuses are misclassified in the second |
| `/api/plan` 405 never produces availability reassurance | append “still available” in the host-tested failed-preview formatter | append the same promise directly in the wasm failed arm | caught/caught — records 159–160; the rendered-line assertion and failed-arm seam assertion each fail |
| Router profile declaration matches the route set | map `full_routes == false` to `Full` | make only the assembled app stamp `Full` over its read-only API router | caught/caught — records 161–162; the closed mapping and live 405/header assertions fail respectively |

The assertion that detects the original silent path is the 405 classifier test:
removing classification makes it receive `None` rather than a listener-specific
refusal. The independent picker assertions still pass in that mutation, which
is why both halves are pinned. Overall failure-atlas result: **8 caught, 0
survived, 0 baseline/build failures**.

## Consequences

- The LAN picker remains useful as a catalog but no longer pretends it can
  switch the selected repository.
- An unexpected 405 from any ordinary write or direct plan/preview POST becomes
  visible and actionable instead of looking like navigation or lost auth.
- A `/api/plan` refusal can no longer share a panel with words claiming the
  operation remains available.
- Adding another listener profile is a closed-contract change: the protocol
  enum, router mapping, client policy, tests, and compatibility decision must be
  revisited together.
- The wasm-only modules keep DOM/fetch glue; the decisions that can go wrong are
  exercised by the host test runner.

**Signed:** max · 2026-09-02
