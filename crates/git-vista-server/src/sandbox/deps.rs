//! F10: `docs/DEPENDENCY_EXCEPTIONS.md` gates RustSec *advisory ids*, not new
//! crates — confirmed against the file, `.cargo/audit.toml` and `ci.yml`.
//! A new kernel-API dependency would otherwise sail in with no owner, no
//! justification and no review. This test is the source-of-truth half; the CI
//! step in the `audit` job is the enforcement half.

const REGISTER: &str = include_str!("../../../../docs/NATIVE_DEPENDENCIES.md");
const MANIFEST: &str = include_str!("../../Cargo.toml");

/// Crates that touch the kernel ABI directly. Adding one to `Cargo.toml`
/// without a row in the register fails here, in the `core` test job, before CI
/// ever gets to the audit job.
const KERNEL_API_CRATES: &[&str] = &[
    "libc",
    "seccompiler",
    "nix",
    "rustix",
    "landlock",
    "libseccomp",
];

#[test]
fn every_kernel_api_dependency_has_a_register_row() {
    for krate in KERNEL_API_CRATES {
        let declared = MANIFEST.lines().any(|l| {
            l.trim_start().starts_with(&format!("{krate} "))
                || l.trim_start().starts_with(&format!("{krate}="))
        });
        if !declared {
            continue;
        }
        assert!(
            REGISTER.contains(&format!("| `{krate}` |")),
            "{krate} is a direct dependency of git-vista-server but has no row in \
             docs/NATIVE_DEPENDENCIES.md. Kernel-ABI crates are reviewed, not assumed (F10)."
        );
    }
}

#[test]
fn the_register_names_an_owner_and_a_reason_for_each_row() {
    for line in REGISTER.lines().filter(|l| l.starts_with("| `")) {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        assert!(cells.len() >= 6, "malformed register row: {line}");
        assert!(!cells[2].is_empty(), "row has no reason: {line}");
        assert!(!cells[3].is_empty(), "row has no owner: {line}");
        assert!(
            !cells[4].is_empty(),
            "row names no reviewed alternative: {line}"
        );
    }
}
