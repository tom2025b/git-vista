# Git-Vista Documentation

This directory contains current operator references, architectural decisions,
product design material, and dated engineering records. They are not all the
same kind of truth. Use this page to choose the right source before relying on a
claim.

## Where to start

| You want to… | Read |
| --- | --- |
| Install and run Git-Vista | [Project README](../README.md) |
| Understand the supported host and browser floors | [Supported Versions](SUPPORTED_VERSIONS.md) |
| Connect from an iPad or another machine | [Remote Linux Architecture](REMOTE_ARCHITECTURE.md) |
| Understand the security boundary | [Security Model](SECURITY_MODEL.md) |
| Learn the sandbox without kernel jargon | [The Sandbox, Explained](SANDBOX_EXPLAINED.md) |
| See what blocks a release | [Release Gates](RELEASE_GATES.md) |
| Review dependency exceptions | [Dependency Exceptions](DEPENDENCY_EXCEPTIONS.md) |
| Review native/kernel-facing dependencies | [Native Dependencies](NATIVE_DEPENDENCIES.md) |
| Run the declared mutation checks | [Invariant Receipts](INVARIANT_RECEIPTS.md) |
| Find an architectural decision | [ADR Index](adr/README.md) |

## Authority and freshness

When sources conflict, use this order:

1. **Code, tests, and executable CI checks.** These describe what the current
   commit actually accepts.
2. **Accepted Architecture Decision Records.** ADRs explain why a boundary or
   behavior exists and what alternatives were rejected.
3. **Current reference documents.** The operator and policy documents listed
   above consolidate behavior but can lag a recent implementation change.
4. **Roadmaps, designs, investigations, reviews, handoffs, and work notes.**
   These are evidence about a point in time or a proposed direction, not an
   unconditional statement of current behavior.

Do not turn a proposal into a product claim merely because it has a polished
document or PDF. Verify it in current code, an accepted ADR, or an enforced
test.

## Current operator and policy references

- [Supported Versions](SUPPORTED_VERSIONS.md) — Git, Safari, and Linux kernel
  floors, including how each floor is measured or enforced.
- [Remote Linux Architecture](REMOTE_ARCHITECTURE.md) — loopback service, SSH
  forwarding, session bootstrap, LAN-view separation, and long-running
  operations.
- [Security Model](SECURITY_MODEL.md) — trust boundaries, operating modes,
  command execution, credentials, request integrity, storage, and release
  criteria.
- [Release Gates](RELEASE_GATES.md) — required checks and the evidence each gate
  is intended to supply.
- [Dependency Exceptions](DEPENDENCY_EXCEPTIONS.md) — synchronized,
  owner-assigned, expiring RustSec exceptions.
- [Native Dependencies](NATIVE_DEPENDENCIES.md) — narrow register for crates
  that touch the kernel ABI directly.
- [Invariant Receipts](INVARIANT_RECEIPTS.md) — declared mutations and named
  tests for high-value Git-Vista rules.

## Architecture and product design

- [ADR Index](adr/README.md) — numbered decisions, their status, and links to
  the full records.
- [iPad Design](IPAD_DESIGN.md) — touch, Pencil, keyboard, viewport, and
  responsive interaction requirements.
- [Performance Budgets](PERFORMANCE_BUDGETS.md) — measurable latency and scale
  targets.
- [Feature Matrix](FEATURE_MATRIX.md) — product comparison and capability
  framing; verify current implementation claims against code and ADRs.
- [V2 Architecture](V2_ARCHITECTURE.md), [Future Vision](FUTURE_VISION.md), and
  [Git Client Roadmap](GIT_CLIENT_ROADMAP.md) — direction and proposed work,
  not a release manifest.

## Engineering records

- `investigations/` contains dated acceptance audits and focused technical
  investigations.
- `sandbox/` contains sandbox contracts, censuses, and detailed design work.
- `reviews/` contains review snapshots and findings from particular commits.
- `handoffs/` and `work-notes/` preserve coordination context. They may be
  obsolete after the work they describe lands.
- `pdf/` and adjacent `.pdf` files are rendered companions. The Markdown source
  is the editable version unless a document explicitly says otherwise.

These records are intentionally preserved. Rewriting them to sound current
would erase the evidence of what was known and decided at the time.

## Documentation maintenance

When behavior changes:

1. Change the code and its enforcing test or gate.
2. Add or amend an ADR when the decision is expensive to reverse.
3. Update the smallest current reference document that users will consult.
4. Keep future work labeled as future work.
5. Preserve dated records; add a follow-up or superseding link instead of
   silently rewriting history.
6. Check all commands from a clean Linux checkout before merging.

The root README should remain a usable entry point, not a milestone ledger.
Detailed evidence belongs here or in an ADR, with the README linking to it.
