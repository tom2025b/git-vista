//! Entry point for the Leptos frontend.
//!
//! Trunk compiles this crate for `wasm32-unknown-unknown` and serves the result.
//! The `cfg` split keeps a plain `cargo build --workspace` (host target) happy:
//! on native there's nothing to mount, so we emit a tiny stub binary.

// Pure layout/colour/demo logic — no UI deps, so it compiles (and is tested) on
// the host too. Only the host's non-test build leaves it unused, hence the
// targeted allows.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod camera;
// Branch colours live in the crate-wide `git_vista_core::color` "Color God" — the
// single source of truth shared with the layout engine — so there's no local
// colour module here; the render code imports from core directly.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod datetime;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod geometry;
// M1.11 (#64): the feature boundaries. NOT cfg-gated, on purpose — the feature *cores*
// are framework-free and must compile on the host target so `cargo test --workspace` runs
// their tests. Only the `signals.rs` wrappers inside are wasm-gated. Gating this line
// would silently delete the coverage M1.11's acceptance criterion 5 depends on.
//
// The blanket `allow(dead_code)` is scaffolding, not policy: the vocabulary lands in
// Task 1 and each later task consumes more of it. It narrows to the repo's usual
// `cfg_attr(not(any(target_arch = "wasm32", test)), …)` form once the wiring is done.
#[allow(dead_code)]
mod features;
// INV-15 (#66 M1.13b, #208): the per-repository hook-policy disclosure text.
// NOT wasm-gated, on purpose — the descriptor-to-wording mapping is pure, and
// the one decision that matters ("does this repository warn?") must be tested
// on the host. `picker.rs` renders what this returns.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod hook_policy_disclosure;
// #473: what the topbar says about a HEAD that resolves to nothing. NOT
// wasm-gated, for the same reason as the disclosure above — `mod app` is
// wasm-only, so a decision left inside the view could never be host-tested,
// and "renders nothing" is exactly the failure being fixed.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod head_notice;
// M1.04 (#57) / #392: the `#s=<token>` fragment parse, lifted out of the
// wasm-only `mod session` so it can be host-tested at all — and so startup and
// #392's `hashchange` listener share one parser rather than two that can
// silently disagree.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod bootstrap_fragment;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod icons;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod lod;
// #589: presentation and HTTP-failure policy for the listener profile.  Pure
// and host-tested; picker/api/preview_panel are wasm-only glue over its answers.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod listener_policy;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod text;
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
mod viewport;
// Hardcoded demo history. No longer used in the render path (the frontend now
// fetches real history from the backend), but kept for tests/fixtures.
#[cfg(test)]
mod graph;
// #340: a structural census over api.rs's offline write-guard, proving in
// the host test suite what `mod api` below being wasm32-gated otherwise
// leaves completely untested. Test-only, like `graph` above.
#[cfg(test)]
mod offline_guard_audit;
// A structural census over git-vista/src and git-vista-core/src: every
// declared pub fn must have a real (statement-shaped) call site somewhere in
// the crates/ tree, or be argued dead in EXEMPT. Same "wasm-gated code the
// host test suite can't link against" shape offline_guard_audit closes for
// api.rs's write guard, generalized to catch the #68d/#69c/#350 regression
// class (a pure-logic function shipped fully host-tested with zero real
// callers). Test-only, like graph/offline_guard_audit above.
#[cfg(test)]
mod reachability_census;
// #612 slice 4: an inventory over every #[cfg(target_arch = "wasm32")]-gated
// file above a size threshold, checking that some host test's include_str!
// reads it (or that the gap is argued in EXEMPT). Answers "which wasm-only
// modules has nobody pinned?" as a standing test instead of a manual reread
// of the tree. Test-only, like graph/offline_guard_audit/reachability_census
// above.
#[cfg(test)]
mod wasm_module_census;

// The frontend, split out of the former monolithic `app.rs`. Every one of these
// pulls in Leptos / web-sys (wasm-only deps), so — like `app` — they compile only
// for the wasm target; a host `cargo build --workspace` builds the native stub
// below and skips them entirely.
#[cfg(target_arch = "wasm32")]
mod activity;
#[cfg(target_arch = "wasm32")]
mod api;
#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod detail;
#[cfg(target_arch = "wasm32")]
mod dialogs;
#[cfg(target_arch = "wasm32")]
mod gestures;
// M1.13a (#66, ADR 0025): the persistent disclosure banner shown while
// SECURITY_MODEL.md:236's hook policy is `Allow` for this session.
#[cfg(target_arch = "wasm32")]
mod hook_policy_banner;
#[cfg(target_arch = "wasm32")]
mod menu;
// M2.22b (#242): the persistent offline banner shown while the browser
// reports no network — the UI face of M2.22a's connectivity signal.
#[cfg(target_arch = "wasm32")]
mod offline_banner;
// The repo picker + Visualize/Active mode screens (ADR 0006/0009).
#[cfg(target_arch = "wasm32")]
mod picker;
#[cfg(target_arch = "wasm32")]
mod prefs;
#[cfg(target_arch = "wasm32")]
mod print;
#[cfg(target_arch = "wasm32")]
mod render;
mod repomap;
// M1.04 (#57): the loopback session bootstrap flow — exchange the one-time
// `#s=<token>` fragment for a session cookie, and the blocking sign-in screen.
#[cfg(target_arch = "wasm32")]
mod session;
#[cfg(target_arch = "wasm32")]
mod state;
// M1.02 (#102): the blocking "Update Required" screen shown when the client's
// protocol version is incompatible with the server's.
#[cfg(target_arch = "wasm32")]
mod update_required;
#[cfg(target_arch = "wasm32")]
mod viewer;

#[cfg(target_arch = "wasm32")]
fn main() {
    // Surface Rust panics in the browser devtools console.
    console_error_panic_hook::set_once();
    leptos::mount_to_body(app::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!(
        "git-vista's frontend is a WebAssembly app built with Trunk.\n\
         Run it via the `gv` launcher, or `trunk serve` for frontend-only iteration."
    );
}
