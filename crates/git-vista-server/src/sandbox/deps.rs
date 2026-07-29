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

/// `None` if the row is complete, otherwise what is missing.
///
/// Split out from the test that runs it over the real register so the gate can
/// be pointed at a deliberately bad row and shown to *reject* it. A gate that
/// has only ever been run against input it accepts has not been tested; it has
/// been observed agreeing.
fn row_defect(line: &str) -> Option<String> {
    let cells: Vec<&str> = line.split('|').map(str::trim).collect();
    if cells.len() < ROW_CELLS {
        return Some(format!(
            "malformed row: want {ROW_CELLS} cells, got {}",
            cells.len()
        ));
    }
    for (col, what) in [
        (COL_CRATE, "crate name"),
        (COL_VERSION, "version"),
        (COL_REASON, "reason it is unavoidable"),
        (COL_OWNER, "owner"),
        (COL_ALTERNATIVE, "reviewed alternative"),
        (COL_REVIEW_DATE, "review date"),
    ] {
        if cells[col].is_empty() {
            return Some(format!("row has no {what}"));
        }
    }
    None
}

#[test]
fn the_register_names_an_owner_and_a_reason_for_each_row() {
    let mut rows = 0usize;
    for line in REGISTER.lines().filter(|l| l.starts_with("| `")) {
        rows += 1;
        assert!(
            row_defect(line).is_none(),
            "{}: {line}",
            row_defect(line).unwrap()
        );
    }
    assert!(
        rows > 0,
        "the register parsed zero rows — the row format changed and this gate is now \
         checking nothing at all"
    );
}

/// Proof the gate bites. Every row here is one a *correct* gate must reject,
/// and the last two are the exact rows the previous off-by-one let through: it
/// read cells 2/3/4 (version, reason, owner) while believing it read reason,
/// owner and alternative, so the two columns that carry the actual review —
/// the alternative considered and the date it was considered — were never
/// checked at all. Both of those rows passed the old gate.
#[test]
fn the_gate_rejects_incomplete_rows() {
    let bad = [
        ("| `x` | 0.1 | why | who | alt |", "too few cells"),
        ("| `x` |  | why | who | alt | 2026-01-01 |", "no version"),
        ("| `x` | 0.1 |  | who | alt | 2026-01-01 |", "no reason"),
        ("| `x` | 0.1 | why |  | alt | 2026-01-01 |", "no owner"),
        (
            "| `x` | 0.1 | why | who |  | 2026-01-01 |",
            "no reviewed alternative — passed the old off-by-one gate",
        ),
        (
            "| `x` | 0.1 | why | who | alt |  |",
            "no review date — passed the old off-by-one gate",
        ),
    ];
    for (row, why) in bad {
        assert!(
            row_defect(row).is_some(),
            "the gate accepted a row that {why}: {row}"
        );
    }
}

/// The other half of the same proof: a complete row must be accepted, so the
/// test above cannot be passing because `row_defect` rejects everything.
#[test]
fn the_gate_accepts_a_complete_row() {
    let good = "| `x` | 0.1 | why it is unavoidable | Tom | the alternative, and why not | 2026-01-01 |";
    assert_eq!(row_defect(good), None, "a complete row must pass");
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
