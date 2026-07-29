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

/// The register's columns, by index into `line.split('|')`.
///
/// A leading `|` makes `split` yield an empty first element and a trailing `|`
/// an empty last one, so a six-column row is **eight** elements, not six, and
/// the crate name lands at 1 — not 0. Getting this wrong is not a cosmetic
/// slip: the earlier `cells[2]/[3]/[4]` reading checked *version, reason,
/// owner* while believing it checked *reason, owner, alternative*, so a row
/// with an empty "Reviewed alternative" and an empty "Review date" passed and
/// the gate was a no-op for exactly the two columns that carry the review.
const COL_CRATE: usize = 1;
const COL_VERSION: usize = 2;
const COL_REASON: usize = 3;
const COL_OWNER: usize = 4;
const COL_ALTERNATIVE: usize = 5;
const COL_REVIEW_DATE: usize = 6;
/// Six columns plus the empty elements the leading and trailing `|` produce.
const ROW_CELLS: usize = 8;

#[test]
fn the_register_names_an_owner_and_a_reason_for_each_row() {
    let mut rows = 0usize;
    for line in REGISTER.lines().filter(|l| l.starts_with("| `")) {
        rows += 1;
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        assert!(
            cells.len() >= ROW_CELLS,
            "malformed register row (want {ROW_CELLS} cells, got {}): {line}",
            cells.len()
        );
        for (col, what) in [
            (COL_CRATE, "crate name"),
            (COL_VERSION, "version"),
            (COL_REASON, "reason it is unavoidable"),
            (COL_OWNER, "owner"),
            (COL_ALTERNATIVE, "reviewed alternative"),
            (COL_REVIEW_DATE, "review date"),
        ] {
            assert!(!cells[col].is_empty(), "row has no {what}: {line}");
        }
    }
    assert!(
        rows > 0,
        "the register parsed zero rows — the row format changed and this gate is now \
         checking nothing at all"
    );
}

/// The column indices above are asserted against the register's own header, so
/// a reordered or inserted column fails here rather than silently shifting what
/// every other assertion checks.
#[test]
fn the_column_indices_match_the_registers_header() {
    let header = REGISTER
        .lines()
        .find(|l| l.starts_with("| Crate |"))
        .expect("the register must have a `| Crate |` header row");
    let cells: Vec<&str> = header.split('|').map(str::trim).collect();
    assert_eq!(cells.len(), ROW_CELLS, "header column count changed: {header}");
    assert_eq!(cells[COL_CRATE], "Crate");
    assert_eq!(cells[COL_VERSION], "Version");
    assert!(cells[COL_REASON].starts_with("Why it is unavoidable"));
    assert_eq!(cells[COL_OWNER], "Owner");
    assert!(cells[COL_ALTERNATIVE].starts_with("Reviewed alternative"));
    assert_eq!(cells[COL_REVIEW_DATE], "Review date");
}
