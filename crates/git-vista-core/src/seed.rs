//! Test-repo seed state: parsing the recorded snapshot and planning a reset.
//!
//! A repo opted in as a *test repo* (`gv --seed <path>`) carries a recorded
//! "seed" under `.git/git-vista/`: the exact local branches with their tips
//! (`seed-refs`), the checked-out branch (`seed-head`), and a bundle of the
//! objects those tips need (`seed.bundle`). "Reset Test Repo" restores that
//! state, discarding everything done since — including *deleting* branches
//! created after the seed, which is allowed nowhere else in git-vista.
//!
//! This module is the pure half: parsing the two text files and computing
//! exactly which refs to move and which to delete. The server does the I/O.

/// One recorded branch: its name and the full commit id it pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedRef {
    pub name: String,
    pub oid: String,
}

/// The recorded seed state: every local branch and the checked-out one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub head: String,
    pub refs: Vec<SeedRef>,
}

/// Parse the seed files: `refs` is `git for-each-ref refs/heads
/// --format='%(objectname) %(refname:short)'` output (one `<40-hex-oid>
/// <branch>` per line; ref names can't contain spaces... but only the FIRST
/// space is a separator anyway, so even an exotic name survives), `head` is
/// the `git symbolic-ref --short HEAD` output. Errors — not skips — on any
/// malformed line: a reset from a corrupt seed must refuse to run rather
/// than half-restore.
pub fn parse_seed(refs: &str, head: &str) -> Result<Seed, String> {
    let head = head.trim();
    if head.is_empty() {
        return Err("seed-head is empty".to_string());
    }
    let mut parsed = Vec::new();
    for line in refs.lines().filter(|l| !l.trim().is_empty()) {
        let Some((oid, name)) = line.trim().split_once(' ') else {
            return Err(format!("malformed seed-refs line: {line:?}"));
        };
        if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("malformed commit id in seed-refs: {oid:?}"));
        }
        if name.is_empty() {
            return Err(format!("empty branch name in seed-refs line: {line:?}"));
        }
        parsed.push(SeedRef {
            name: name.to_string(),
            oid: oid.to_lowercase(),
        });
    }
    if parsed.is_empty() {
        return Err("seed-refs lists no branches".to_string());
    }
    if !parsed.iter().any(|r| r.name == head) {
        return Err(format!(
            "seed-head ‘{head}’ isn't among the seeded branches"
        ));
    }
    Ok(Seed {
        head: head.to_string(),
        refs: parsed,
    })
}

/// What a reset must do to the branch refs, given the repo's *current*
/// `(name, oid)` branches: `update` every seeded branch that's missing or
/// moved, `delete` every branch the seed doesn't know. Both sorted by name so
/// the applied order (and any log of it) is deterministic.
#[derive(Debug, PartialEq, Eq)]
pub struct ResetPlan {
    pub update: Vec<SeedRef>,
    pub delete: Vec<String>,
}

pub fn reset_plan(seed: &Seed, current: &[(String, String)]) -> ResetPlan {
    let mut update: Vec<SeedRef> = seed
        .refs
        .iter()
        .filter(|s| !current.iter().any(|(n, o)| *n == s.name && *o == s.oid))
        .cloned()
        .collect();
    let mut delete: Vec<String> = current
        .iter()
        .filter(|(n, _)| !seed.refs.iter().any(|s| s.name == *n))
        .map(|(n, _)| n.clone())
        .collect();
    update.sort_by(|a, b| a.name.cmp(&b.name));
    delete.sort();
    ResetPlan { update, delete }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn seed() -> Seed {
        parse_seed(&format!("{A} main\n{B} feature/x\n"), "main\n").unwrap()
    }

    #[test]
    fn a_seed_parses_to_its_branches_and_head() {
        let s = seed();
        assert_eq!(s.head, "main");
        assert_eq!(s.refs.len(), 2);
        assert_eq!(
            s.refs[1],
            SeedRef {
                name: "feature/x".into(),
                oid: B.into()
            }
        );
    }

    #[test]
    fn corrupt_seeds_refuse_to_parse_rather_than_half_restore() {
        // Truncated oid, missing separator, empty file, head not seeded,
        // empty head: each is a hard error.
        assert!(parse_seed("abc main\n", "main").is_err());
        assert!(parse_seed(&format!("{A}main\n"), "main").is_err());
        assert!(parse_seed("", "main").is_err());
        assert!(parse_seed(&format!("{A} main\n"), "gone").is_err());
        assert!(parse_seed(&format!("{A} main\n"), "  ").is_err());
    }

    #[test]
    fn the_plan_moves_recreates_and_deletes_exactly_whats_needed() {
        // Current repo: main moved to C, feature/x deleted, extra/y created.
        let current = vec![
            ("main".to_string(), C.to_string()),
            ("extra/y".to_string(), C.to_string()),
        ];
        let plan = reset_plan(&seed(), &current);
        assert_eq!(
            plan.update,
            vec![
                SeedRef {
                    name: "feature/x".into(),
                    oid: B.into()
                },
                SeedRef {
                    name: "main".into(),
                    oid: A.into()
                },
            ]
        );
        assert_eq!(plan.delete, vec!["extra/y".to_string()]);
    }

    #[test]
    fn an_untouched_repo_yields_an_empty_plan() {
        let current = vec![
            ("feature/x".to_string(), B.to_string()),
            ("main".to_string(), A.to_string()),
        ];
        let plan = reset_plan(&seed(), &current);
        assert!(plan.update.is_empty() && plan.delete.is_empty());
    }
}
