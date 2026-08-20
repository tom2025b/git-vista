//! INV-16 half of `argv_boundary`'s tripwire (#144): `sandbox_argv`'s three
//! argv shapes, exhaustively over every `Tier`/`HookMode`. Split out of
//! `argv_boundary.rs` because it proves a different thing than the source
//! scan above it — the *value* a chokepoint produces, not which files may
//! call a chokepoint — even though both prove no argv smuggling is possible.
//!
//! **This file is scanned too, and is not exempt.** The parent's spawn-site
//! scan walks every `.rs` file under `src/`, including this one, and its
//! by-name exemption from the literal-`git` check names only
//! `src/argv_boundary.rs` — not this path. Nothing here constructs a
//! `Command`, and it should stay that way; never spell the bare pattern
//! (`Command` immediately followed by `::new(`) in a comment here even in
//! passing, or a prose mention reads as a new, unreviewed spawn site.

use std::path::{Path, PathBuf};

/// INV-16, the value half asserted **exhaustively over the tier enum**: every
/// argv `sandbox_argv` can produce is one of exactly three shapes, and in the
/// sandboxed shapes the tail is `-- git` with `git` named exactly once.
///
/// `sandbox::argv` already pins each shape against its reviewed constants. What
/// this adds — and the reason it lives in the tripwire file rather than beside
/// those — is coverage that cannot silently miss a case, plus the negative
/// space:
///
///  * The tiers are walked through an exhaustive `match` (`next_tier`), so a
///    new `Tier` variant is a **compile error here** rather than an untested
///    shape. `sandbox::argv` iterates hand-written arrays, which a fourth tier
///    would slip past without a word.
///  * Both hook modes are crossed with every tier. `HookMode::Blocked` is the
///    state a host that failed INV-13 drops to, and the `Unsandboxed`/`Blocked`
///    corner is the one that already shipped broken once (a bare `["git"]` that
///    ran hooks a policy said were blocked).
///  * `git` must appear **exactly once** in a sandboxed argv, as the last
///    entry, and the element before `-- git` must be a reviewed policy flag or
///    its value. Together those say nothing can be appended between the
///    reviewed prefix and the program, and no second program name can ride
///    along — neither of which follows from checking the tail alone.
#[test]
fn sandbox_argv_is_one_of_inv16s_three_shapes_in_every_tier() {
    use crate::sandbox::{sandbox_argv, HookMode, Policy, Tier, DEFAULT_GIT_PORTS};

    /// The tier walk. Exhaustive on purpose: adding a `Tier` variant will not
    /// compile until it is given an arm, and linking it into the chain is what
    /// puts it under every assertion below.
    fn next_tier(tier: Tier) -> Option<Tier> {
        match tier {
            Tier::Strict => Some(Tier::Network),
            Tier::Network => Some(Tier::Unsandboxed),
            Tier::Unsandboxed => None,
        }
    }

    const SHIM: &str = "/opt/gv/gv-sandbox";
    const HOOK_DIR: &str = "/var/lib/gv/no-hooks";

    let policy = |tier: Tier, hook_mode: HookMode| Policy {
        tier,
        shim: PathBuf::from(SHIM),
        // Fake but absolute, like `sandbox::argv`'s: these assertions are about
        // shape, and pinning to wherever bwrap really lives would make them
        // pass or fail for reasons unrelated to the chokepoint.
        bwrap: (tier == Tier::Strict).then(|| PathBuf::from("/usr/bin/bwrap")),
        rw_trees: vec![PathBuf::from("/srv/repos/r")],
        ro_trees: vec![PathBuf::from("/usr"), PathBuf::from("/home/tom")],
        secret_excludes: vec![PathBuf::from("/home/tom/.ssh")],
        // #188 is out of scope for this test (it pins the three INV-16
        // argv shapes across every tier/hook-mode combination, not any one
        // flag's contents) — empty in every tier here on purpose.
        ro_carveouts: Vec::new(),
        net_ports: if tier == Tier::Network {
            DEFAULT_GIT_PORTS.to_vec()
        } else {
            Vec::new()
        },
        hook_mode,
    };

    let mut tiers = Vec::new();
    let mut cursor = Some(Tier::Strict);
    while let Some(tier) = cursor {
        assert!(!tiers.contains(&tier), "the tier walk loops: {tier:?}");
        tiers.push(tier);
        cursor = next_tier(tier);
    }
    assert_eq!(
        tiers.len(),
        3,
        "the tier census drifted — {} tiers walked, three shapes documented in \
         `sandbox_argv`'s INV-16 comment. Either a tier was added without a \
         shape, or the walk lost one.",
        tiers.len()
    );

    for tier in tiers {
        for hook_mode in [
            HookMode::Run,
            HookMode::Blocked {
                empty_dir: PathBuf::from(HOOK_DIR),
            },
        ] {
            let blocked = matches!(hook_mode, HookMode::Blocked { .. });
            let argv: Vec<String> = sandbox_argv(&policy(tier, hook_mode))
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let what = format!("{tier:?}/blocked={blocked}");

            if tier == Tier::Unsandboxed {
                // Shapes 1 and 2. `git` leads here — there is no launcher in
                // front of it — and nothing may follow but the hook
                // suppression, which is the only argument this tier is allowed
                // to add.
                let expected: Vec<String> = if blocked {
                    vec![
                        "git".into(),
                        "-c".into(),
                        format!("core.hooksPath={HOOK_DIR}"),
                    ]
                } else {
                    vec!["git".into()]
                };
                assert_eq!(
                    argv, expected,
                    "{what}: the unsandboxed argv is not one of INV-16's two \
                     unsandboxed shapes"
                );
                continue;
            }

            // Shape 3: a reviewed prefix, then `-- git`.
            assert!(
                argv.len() > 2,
                "{what}: a sandboxed argv is a prefix *and* `-- git`, got {argv:?}"
            );
            assert_eq!(
                &argv[argv.len() - 2..],
                ["--".to_string(), "git".to_string()],
                "{what}: the launcher argv must end in `-- git`"
            );
            assert_eq!(
                argv.iter().filter(|s| *s == "git").count(),
                1,
                "{what}: `git` appears more than once — the program name must be \
                 the single last entry, or a reviewer cannot tell which one runs"
            );
            assert!(
                Path::new(&argv[0]).is_absolute(),
                "{what}: the launcher must be an absolute path ({}); a bare name \
                 resolves against the inherited PATH",
                argv[0]
            );
            assert_eq!(
                argv.iter().filter(|s| *s == SHIM).count(),
                1,
                "{what}: the shim must appear exactly once in the argv"
            );

            // Nothing sits between the reviewed policy flags and `-- git`. The
            // last prefix element is either the net decision or, in the network
            // tier, the final `--net-port` value.
            let terminator = &argv[argv.len() - 3];
            assert!(
                terminator == "--net-deny"
                    || terminator == "--net-allow"
                    || terminator.parse::<u16>().is_ok(),
                "{what}: `{terminator}` was appended after the reviewed policy \
                 flags and before `-- git`; every argument the shim reads must \
                 sit inside the reviewed prefix"
            );

            // Exactly one hook decision reaches the shim. Neither is silence.
            let hook_flags = argv
                .iter()
                .filter(|s| *s == "--hooks-run" || *s == "--hooks-blocked")
                .count();
            assert_eq!(
                hook_flags, 1,
                "{what}: expected exactly one hook decision in the argv, found \
                 {hook_flags}"
            );
            assert_eq!(
                argv.iter().any(|s| s == "--hooks-blocked"),
                blocked,
                "{what}: the argv's hook decision contradicts the policy's"
            );

            // And no interpreter anywhere in it — the same rule
            // `launcher_sites_name_no_shell` applies to the source, applied to
            // the value, because a path that arrived from a policy field is not
            // covered by a source scan.
            for bad in ["sh", "bash", "/bin/sh", "/bin/bash", "zsh", "env", "-c"] {
                assert!(
                    !argv.iter().any(|s| s == bad),
                    "{what}: `{bad}` must never appear in a launcher argv"
                );
            }
        }
    }
}
