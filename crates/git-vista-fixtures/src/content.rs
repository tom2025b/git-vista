//! Repositories built to be *hostile to read* — huge files, binary blobs, and
//! paths chosen to walk out of the repository.
//!
//! Nothing here is broken in git's eyes. Every one of these repositories is
//! perfectly valid, and that is the point: they break the *reader*, not the
//! repository. A tool that assumes a file fits in memory, that a patch can be
//! sent whole, or that a path in a commit is a path on disk, fails here while
//! git itself is untroubled.

use crate::git;
use crate::seeded::{seeded, Fixture};
use std::path::Path;

/// Roughly the size of the pathological text fixture's first version.
pub const BIG_TEXT_BYTES: usize = 50 * 1024 * 1024;

/// How much [`pathological_content`]'s second version appends — comfortably
/// past both patch caps.
pub const BIG_TEXT_APPEND: usize = 8 * 1024 * 1024;

/// A string that appears only inside the binary blob.
///
/// If it ever shows up in a patch, binary bytes reached the wire. That is the
/// whole assertion `pathological_content` exists to support, so the sentinel is
/// exported rather than hidden: the test needs to name it.
pub const BINARY_SENTINEL: &str = "GV-BINARY-SENTINEL-PAYLOAD";

/// Write a file of exactly `len` bytes: `header`, then numbered rows.
///
/// Each row carries its own starting offset, so a longer file is a
/// byte-identical *prefix extension* of a shorter one — which is what makes an
/// "append" diff cheap for git to compute. No shell helper is involved:
/// `yes`/`dd`/`head` are banned by the argv boundary, and every child these
/// fixtures spawn is literally `git`.
pub fn write_rows(path: &Path, header: &str, len: usize, tag: &str) {
    use std::io::Write;

    let mut w = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    w.write_all(header.as_bytes()).unwrap();
    let mut written = header.len();
    while written < len {
        let row = format!("{written:012} {tag} bounded-read fixture row\n");
        let take = row.len().min(len - written);
        w.write_all(&row.as_bytes()[..take]).unwrap();
        written += take;
    }
    w.flush().unwrap();
}

/// The size a file actually reached on disk.
///
/// Used to assert a fixture really is as large as it claims. A silently short
/// write would make every "this is too big to send" test pass for the wrong
/// reason.
pub fn on_disk_len(path: &Path) -> usize {
    std::fs::metadata(path).unwrap().len() as usize
}

/// `len` bytes of binary content: NUL-delimited [`BINARY_SENTINEL`] runs.
///
/// The leading NUL is inside the first 8000 bytes, which is what makes both
/// git and git-vista's own sniff call this binary. A file that is merely
/// *large* would not do — the two are handled by different code paths.
pub fn binary_blob(tag: &str, len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len);
    bytes.push(0u8);
    bytes.extend_from_slice(tag.as_bytes());
    while bytes.len() < len {
        bytes.push(0u8);
        bytes.extend_from_slice(BINARY_SENTINEL.as_bytes());
    }
    bytes.truncate(len);
    bytes
}

/// A commit no device could render whole: ~50 MiB of text plus a NUL-bearing
/// binary blob. Returns the fixture and the pathological commit's oid.
///
/// ## What is wrong
///
/// Nothing, to git. The repository is valid and every command works. What is
/// wrong is the *size*: a naive "show me this commit's diff" produces a patch
/// tens of megabytes long, and a naive "show me this file" produces a response
/// no browser will survive.
///
/// ## What git put on disk
///
/// Two commits. The first adds `zbig.txt` at [`BIG_TEXT_BYTES`] and `bin.dat`
/// at 64 KiB; the second appends [`BIG_TEXT_APPEND`] to the text and rewrites
/// the blob at 96 KiB.
///
/// Two deliberate choices sit behind that shape. `bin.dat` sorts before
/// `zbig.txt`, so git's patch leads with the binary section — otherwise the
/// panel cap would cut away the very `Binary files … differ` line the tests are
/// about. And the text change is an **append**, so git trims the identical
/// 50 MiB prefix in one pass: the fixture stays a fixture rather than a
/// minutes-long diff, while still producing a patch far past both caps.
///
/// ## Why it matters
///
/// Two failures hide here, and they are different failures. A response that is
/// merely *huge* is a performance problem. A response that carries the binary
/// blob's bytes is a **correctness and safety** problem — which is why
/// [`BINARY_SENTINEL`] exists: a test can assert those exact bytes never
/// appear, rather than only that the patch was small enough.
pub fn pathological_content() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let (dir, repo) = seeded();

    write_rows(&repo.join("zbig.txt"), "ZBIG\n", BIG_TEXT_BYTES, "alpha");
    std::fs::write(repo.join("bin.dat"), binary_blob("one", 64 * 1024)).unwrap();
    assert_eq!(on_disk_len(&repo.join("zbig.txt")), BIG_TEXT_BYTES);
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "add pathological content"]);

    write_rows(
        &repo.join("zbig.txt"),
        "ZBIG\n",
        BIG_TEXT_BYTES + BIG_TEXT_APPEND,
        "alpha",
    );
    assert_eq!(
        on_disk_len(&repo.join("zbig.txt")),
        BIG_TEXT_BYTES + BIG_TEXT_APPEND,
        "the fixture must really be ~50 MiB, not a silently short write"
    );
    std::fs::write(repo.join("bin.dat"), binary_blob("two", 96 * 1024)).unwrap();
    git::run(&repo, &["add", "-A"]);
    git::run(
        &repo,
        &["commit", "-q", "-m", "modify pathological content"],
    );

    let id = git::out(&repo, &["rev-parse", "HEAD"]);
    (dir, repo, id)
}

/// A repository shaped to exercise the malicious-path battery.
///
/// ## What is wrong
///
/// Again, nothing — the danger is in what a *caller* may ask for. The
/// repository holds a root file, a subdirectory (so a tree-vs-blob path
/// exists), and a **committed symlink**.
///
/// ## What git put on disk
///
/// `secret.txt` at the root, `sub/file.txt`, and `sub/link.txt` — a symlink
/// object whose *content* is the string `file.txt`. That is what a symlink is
/// in git: a blob holding a path, with a mode of `120000`. It is not a pointer
/// git follows.
///
/// ## Why it matters
///
/// Two distinct traversals have to be refused, and they fail differently.
/// `<rev>:../secret.txt` asks git to resolve a path above the worktree root:
/// git's own boundary check refuses it, independent of the tree object.
/// `sub/link.txt` asks for a symlink: the honest answer is the *blob's*
/// content, the literal string `file.txt`. A reader that resolved it against
/// the filesystem would be following a path chosen by whoever wrote the commit
/// — which is how a repository becomes a way to read `/etc/passwd`.
pub fn path_battery() -> Fixture {
    let (dir, repo) = seeded();
    std::fs::write(repo.join("secret.txt"), "root-secret\n").unwrap();
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/file.txt"), "sub-file\n").unwrap();
    std::os::unix::fs::symlink("file.txt", repo.join("sub/link.txt")).unwrap();
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "path battery fixture"]);
    (dir, repo)
}

/// A repository where **each of the four diff modes sees a different change**.
/// Returns the fixture plus the first and second commits' oids.
///
/// ## What is wrong
///
/// Nothing is broken; what is missing, in most fixtures, is *discrimination*.
/// A test that asks for "the diff" and gets a non-empty patch has proved
/// almost nothing — it has not proved which two things were compared.
///
/// ## What git put on disk
///
/// `v.txt` moves through four values, each parked in a different place:
///
/// ```text
///   one    commit 1 (branch `base`)
///   two    commit 2 (branch `main`, HEAD)
///   three  staged in the index, not committed
///   four   in the working tree, not staged
/// ```
///
/// ## Why it matters
///
/// Worktree-vs-index must see `three`→`four`, index-vs-commit `two`→`three`,
/// and commit-vs-commit `one`→`two`. Four modes, four distinguishable answers:
/// a mode that silently ran the wrong argv shows up as the **wrong pair**
/// rather than as a pass. Getting this wrong is not exotic — showing the user a
/// staged change when they asked what they had edited is the most ordinary
/// mistake a diff view can make, and the hardest to notice.
pub fn four_mode() -> (tempfile::TempDir, std::path::PathBuf, String, String) {
    let (dir, repo) = seeded();

    std::fs::write(repo.join("v.txt"), "one\n").unwrap();
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "v = one"]);
    let c1 = git::out(&repo, &["rev-parse", "HEAD"]);
    git::run(&repo, &["branch", "base"]);

    std::fs::write(repo.join("v.txt"), "two\n").unwrap();
    git::run(&repo, &["add", "-A"]);
    git::run(&repo, &["commit", "-q", "-m", "v = two"]);
    let c2 = git::out(&repo, &["rev-parse", "HEAD"]);

    // Staged but uncommitted.
    std::fs::write(repo.join("v.txt"), "three\n").unwrap();
    git::run(&repo, &["add", "-A"]);

    // Working tree, on top of the staged value and not added.
    std::fs::write(repo.join("v.txt"), "four\n").unwrap();

    (dir, repo, c1, c2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four values must really be in four different places, or every test
    /// built on this fixture proves less than it claims.
    #[test]
    fn four_mode_parks_a_distinct_value_at_each_layer() {
        let (_d, repo, c1, c2) = four_mode();

        assert_eq!(git::out(&repo, &["show", &format!("{c1}:v.txt")]), "one");
        assert_eq!(git::out(&repo, &["show", &format!("{c2}:v.txt")]), "two");
        assert_eq!(git::out(&repo, &["show", ":v.txt"]), "three");
        assert_eq!(
            std::fs::read_to_string(repo.join("v.txt")).unwrap(),
            "four\n"
        );
    }

    #[test]
    fn binary_blob_is_binary_to_git_and_carries_the_sentinel() {
        let bytes = binary_blob("one", 4096);
        assert_eq!(bytes.len(), 4096);
        assert!(bytes[..8000.min(bytes.len())].contains(&0u8));
        assert!(String::from_utf8_lossy(&bytes).contains(BINARY_SENTINEL));
    }

    /// `write_rows` must hit the length exactly — a short write would make a
    /// "too big to send" test pass because the file was never big.
    #[test]
    fn write_rows_writes_exactly_the_length_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rows.txt");
        for len in [64usize, 1024, 65_536] {
            write_rows(&path, "HEAD\n", len, "tag");
            assert_eq!(on_disk_len(&path), len);
        }
    }

    /// A longer file must be a byte-identical prefix extension of a shorter
    /// one — that is what keeps the pathological append cheap for git.
    #[test]
    fn a_longer_row_file_extends_a_shorter_one_byte_identically() {
        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("short.txt");
        let long = dir.path().join("long.txt");
        write_rows(&short, "ZBIG\n", 8192, "alpha");
        write_rows(&long, "ZBIG\n", 16_384, "alpha");
        let a = std::fs::read(&short).unwrap();
        let b = std::fs::read(&long).unwrap();
        assert_eq!(a, b[..a.len()]);
    }

    /// The symlink must be committed as a symlink (mode 120000) whose content
    /// is the target path — not followed and stored as the file's bytes.
    #[test]
    fn the_path_battery_commits_a_symlink_as_a_path_holding_blob() {
        let (_d, repo) = path_battery();
        let entry = git::out(&repo, &["ls-files", "-s", "--", "sub/link.txt"]);
        assert!(entry.starts_with("120000 "), "expected a symlink: {entry}");
        assert_eq!(git::out(&repo, &["show", "HEAD:sub/link.txt"]), "file.txt");
    }
}
