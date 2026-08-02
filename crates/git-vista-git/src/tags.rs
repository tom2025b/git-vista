//! Reading the repository's tags with the metadata [`read_refs`] throws away
//! (M2.21b, #236): whether each tag is *lightweight* or *annotated*, the tag
//! object's own id, and — for annotated tags — the tagger line, the message,
//! and whether a signature block is present.
//!
//! # Why this is not a `git` subprocess
//!
//! Every other read in this crate goes through `gix`, and this one does too.
//! That is also the answer to "use the batched `cat-file` machinery rather
//! than a spawn per tag" (#221): the batched path exists so the *server* can
//! answer two object queries with one held-open `git cat-file --batch`
//! process. Here there is no process at all — `gix::open_opts` maps the object
//! database once and every tag object is decoded straight out of it. One open,
//! N in-process object reads, zero spawns; a `cat-file --batch` pipeline would
//! be strictly more expensive and would add a subprocess boundary that
//! [`read_refs`] does not have.
//!
//! # What is deliberately *not* decided here
//!
//! This module produces a [`TagRecord`] — raw, git-shaped facts. It does not
//! know about the wire DTO (`git-vista-protocol` is not a dependency of this
//! crate, on purpose), so the newtype validation, the message-length
//! refitting, and the signature *vocabulary* all live at the server's mapping
//! boundary. What this module owns is being **honest about absence**: a
//! lightweight tag has no tagger and no message, and that is `None` here —
//! never an empty string that reads downstream as a blank tagger.

use std::path::Path;

use gix::refs::Category;

use git_vista_core::model::Oid;

use crate::RepoError;

/// The most message bytes one tag contributes to a listing.
///
/// Matches `git_vista_protocol::plan::MAX_TAG_MESSAGE_LEN` (16 KiB) by
/// intent, not by import — this crate deliberately does not depend on the
/// protocol crate. The cap is what stops a repository with a hostile 100 MB
/// "annotation" from turning a tag listing into a 100 MB allocation, the same
/// bounded-read posture the server's `git_stdout_capped` takes. When it bites,
/// [`TagRecord::message_truncated`] says so; the server is what turns that
/// into visible text, because silently returning a prefix that reads like the
/// whole message is the failure mode this flag exists to prevent.
pub const MAX_TAG_MESSAGE_BYTES: usize = 16 * 1024;

/// One tag as git actually stores it — the raw material the server maps onto
/// the `TagDetail` wire DTO.
///
/// The `Option` fields are load-bearing: they are `None` for a lightweight tag
/// because a lightweight tag *has* no tag object, no tagger and no message —
/// there is nothing to render, as opposed to something empty to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRecord {
    /// The tag's short name (`v1.0.0`), i.e. `refs/tags/` stripped.
    pub name: String,
    /// `true` when `refs/tags/<name>` points at a real tag *object*;
    /// `false` when it points straight at the commit (a lightweight tag).
    pub annotated: bool,
    /// The commit the tag ultimately names, after peeling every tag object in
    /// the chain. Always a commit: see [`read_tags`] for what happens to a tag
    /// that peels to a tree or a blob.
    pub target: Oid,
    /// The tag object's own id — `Some` exactly when [`annotated`] is `true`.
    /// For a chain of tag objects this is the *outermost* one, i.e. what
    /// `refs/tags/<name>` literally contains, which is what recreating the
    /// deleted ref would need.
    ///
    /// [`annotated`]: Self::annotated
    pub tag_object: Option<Oid>,
    /// The raw `tagger` header value (`Ada Lovelace <ada@example.com>
    /// 1753300000 +0000`), exactly as git wrote it — display text, never
    /// re-parsed. `None` for a lightweight tag, and also `None` for the rare
    /// annotated tag whose object carries no `tagger` header at all (git
    /// itself always writes one; `git mktag` will accept an object without).
    pub tagger: Option<String>,
    /// The annotation body with any trailing newlines removed, and with the
    /// PGP signature block already split off by the object decoder (so a
    /// signed tag's message is the prose, not the armour). `None` for a
    /// lightweight tag *and* for an annotated tag whose message is empty or
    /// whitespace-only — in both cases there is genuinely nothing to show.
    pub message: Option<String>,
    /// Whether [`message`](Self::message) had to be cut at
    /// [`MAX_TAG_MESSAGE_BYTES`]. Established here, at the byte level, and
    /// never re-derived downstream from the decoded length.
    pub message_truncated: bool,
    /// Whether the tag object carries a PGP signature block.
    ///
    /// This is *presence*, established by the object parser splitting the
    /// `-----BEGIN PGP SIGNATURE-----` armour off the message — no `gpg` is
    /// run and no verdict is reached. M2.21c (#74) is what turns a present
    /// signature into valid/invalid/unknown-key.
    pub signed: bool,
}

/// Read every `refs/tags/*` entry as a [`TagRecord`], sorted by name.
///
/// Sorted order is part of the contract — a listing that reordered itself
/// between reads because the loose/packed ref split changed would make every
/// downstream diff and test flaky for no reason — but this function does not
/// re-sort to get it. `gix`'s ref platform already documents its iterators as
/// yielding refs "sorted by their name" (`gix-ref`'s `overlay_iter`: loose
/// refs come off a sorted directory walk precisely so they can be merged with
/// the already-sorted `packed-refs`), and because every name here shares the
/// `refs/tags/` prefix, full-name order and short-name order are the same
/// order. An explicit `sort_by` here was tried and removed: no mutation could
/// kill it, because the property held with it gone — code a test cannot fail
/// on is code that has stopped being a check. What guards the contract instead
/// is `many_loose_tags_come_back_sorted_whatever_order_the_ref_store_walks_them`,
/// which pins the *observable* order, so a `gix` upgrade that changed the
/// iterator's guarantee fails the build rather than quietly reordering an API.
///
/// # Refs this skips, and why
///
/// * **An unreadable ref** — logged to stderr and skipped, exactly as
///   [`read_refs`](crate::read_refs) does. A single corrupt ref must not fail
///   the whole listing.
/// * **A tag that does not peel to a commit** — a tag on a tree or a blob is
///   legal git and vanishingly rare. It is skipped with a stderr line because
///   the wire DTO's `target` is defined as *the tagged commit*; putting a blob
///   id there would be a quieter lie than leaving the tag out and saying so.
/// * **A tag whose object cannot be found or decoded** — skipped and logged.
///   Reporting it as lightweight would be wrong (the ref does point at a tag
///   object; we just cannot read it) and inventing an annotated record with
///   everything `None` would be indistinguishable from a real one.
///
/// A ref-store open/list failure is a hard [`RepoError`], not an empty list —
/// same call as issue #16 made for [`read_refs`](crate::read_refs): "no tags"
/// and "could not ask" must not look alike.
pub fn read_tags(path: &Path) -> Result<Vec<TagRecord>, RepoError> {
    let repo =
        gix::open_opts(path, gix::open::Options::isolated()).map_err(|e| RepoError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let platform = repo
        .references()
        .map_err(|e| RepoError::Walk(format!("opening the ref store: {e}")))?;
    // `prefixed` still costs a full iteration underneath, but it keeps the
    // category check below from being the only thing standing between a
    // branch and a tag record.
    let tags = platform
        .prefixed("refs/tags/")
        .map_err(|e| RepoError::Walk(format!("listing tags: {e}")))?;

    let mut records = Vec::new();
    for reference in tags {
        let mut reference = match reference {
            Ok(r) => r,
            Err(e) => {
                eprintln!("git-vista: skipping an unreadable ref while reading tags: {e}");
                continue;
            }
        };
        // Belt and braces: `prefixed` already narrowed to `refs/tags/`, but the
        // short name has to come from the category split anyway, and taking
        // both from one call means the name can never disagree with the kind.
        let Some((Category::Tag, short)) = reference.name().category_and_short_name() else {
            continue;
        };
        let name = short.to_string();

        // The object the ref *literally* points at, symbolic refs followed but
        // tag objects NOT peeled. This is the lightweight/annotated question.
        let direct = match reference.follow_to_object() {
            Ok(id) => id.detach(),
            Err(e) => {
                eprintln!("git-vista: tag {name:?} does not resolve to an object ({e}); skipped");
                continue;
            }
        };
        // …and the commit it ultimately names, tag chain peeled.
        let peeled = match reference.peel_to_id() {
            Ok(id) => id.detach(),
            Err(e) => {
                eprintln!("git-vista: tag {name:?} will not peel ({e}); skipped");
                continue;
            }
        };

        let direct_object = match repo.find_object(direct) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("git-vista: tag {name:?} object {direct} is unreadable ({e}); skipped");
                continue;
            }
        };

        // The peeled end of the chain must be a commit for `target` to mean
        // what the DTO says it means. `peel_to_id` stops at the first non-tag
        // object, which for `git tag v1 <blob>` is the blob.
        let peeled_kind = if direct_object.kind == gix::object::Kind::Tag {
            match repo.find_object(peeled) {
                Ok(o) => o.kind,
                Err(e) => {
                    eprintln!(
                        "git-vista: tag {name:?} peeled target {peeled} is unreadable ({e}); \
                         skipped"
                    );
                    continue;
                }
            }
        } else {
            direct_object.kind
        };
        if peeled_kind != gix::object::Kind::Commit {
            eprintln!(
                "git-vista: tag {name:?} names a {peeled_kind}, not a commit; \
                 not in the tag listing"
            );
            continue;
        }

        let record = if direct_object.kind == gix::object::Kind::Tag {
            let decoded = match direct_object.try_to_tag_ref() {
                Ok(t) => t,
                Err(e) => {
                    eprintln!(
                        "git-vista: tag {name:?} object {direct} will not decode ({e}); skipped"
                    );
                    continue;
                }
            };
            let (message, message_truncated) = annotation_message(decoded.message);
            TagRecord {
                name,
                annotated: true,
                target: Oid(peeled.to_string()),
                tag_object: Some(Oid(direct.to_string())),
                tagger: decoded.tagger.map(|t| t.to_string()),
                message,
                message_truncated,
                signed: decoded.pgp_signature.is_some(),
            }
        } else {
            TagRecord {
                name,
                annotated: false,
                target: Oid(peeled.to_string()),
                tag_object: None,
                tagger: None,
                message: None,
                message_truncated: false,
                signed: false,
            }
        };
        records.push(record);
    }

    Ok(records)
}

/// Turn a decoded tag object's raw message bytes into the
/// (`message`, `message_truncated`) pair a [`TagRecord`] carries.
///
/// Three things happen here and nothing else: the bytes are capped at
/// [`MAX_TAG_MESSAGE_BYTES`] *on a UTF-8 character boundary* (so a cut through
/// a multi-byte character can never produce a replacement char that was not in
/// the repository), trailing newlines — git always writes at least one — are
/// removed, and an entirely blank result becomes `None`.
///
/// Split out as a free function so the cap and the blank-message rule are
/// testable without building a repository for each case.
fn annotation_message(raw: &[u8]) -> (Option<String>, bool) {
    let truncated = raw.len() > MAX_TAG_MESSAGE_BYTES;
    let kept = if truncated {
        // Walk back to the nearest boundary at or below the cap. `floor_char_boundary`
        // is still unstable, so this does it by hand: a UTF-8 continuation byte is
        // `0b10xxxxxx`, and there are never more than three of them in a row.
        let mut end = MAX_TAG_MESSAGE_BYTES;
        while end > 0 && (raw[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
        &raw[..end]
    } else {
        raw
    };
    let text = String::from_utf8_lossy(kept);
    let trimmed = text.trim_end_matches(['\n', '\r']).trim();
    if trimmed.is_empty() {
        (None, truncated)
    } else {
        (Some(trimmed.to_string()), truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::tests::{commit, git, git_out};
    use std::path::PathBuf;

    /// A repository with one commit and nothing tagged yet.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        commit(p, "A root", 1);
        commit(p, "B second", 2);
        dir
    }

    /// Create an annotated tag with a pinned tagger identity and timestamp, so
    /// the tagger line the test asserts on is byte-stable.
    fn annotate(dir: &PathBuf, name: &str, message: &str, at: &str) {
        let status = std::process::Command::new("git")
            .args(["tag", "-a", "-m", message, name, at])
            .current_dir(dir)
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_DATE", "@1753300000 +0000")
            .status()
            .expect("git should be runnable");
        assert!(status.success(), "git tag -a {name} failed");
    }

    #[test]
    fn a_repository_with_no_tags_reads_as_an_empty_list() {
        let dir = repo();
        assert_eq!(read_tags(dir.path()).unwrap(), Vec::new());
    }

    #[test]
    fn opening_a_non_repository_errors_rather_than_reading_no_tags() {
        let dir = tempfile::tempdir().unwrap();
        // Not "no tags" — "could not ask".
        assert!(matches!(
            read_tags(dir.path()).unwrap_err(),
            RepoError::Open { .. }
        ));
    }

    /// The load-bearing distinction. Both tags exist in one repository so the
    /// test cannot pass by reading a single tag correctly and generalising.
    #[test]
    fn lightweight_and_annotated_tags_are_told_apart_with_their_metadata() {
        let dir = repo();
        let p = dir.path();
        let root = git_out(p, &["rev-parse", "HEAD~1"]);
        let tip = git_out(p, &["rev-parse", "HEAD"]);
        git(p, &["tag", "tip-marker", &tip]);
        annotate(&p.to_path_buf(), "v1.0", "one\n\nrelease notes", &root);

        let tags = read_tags(p).unwrap();
        assert_eq!(
            tags.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["tip-marker", "v1.0"],
            "sorted by name, not by ref-store enumeration order"
        );

        let light = &tags[0];
        assert!(!light.annotated);
        assert_eq!(light.target, Oid(tip.clone()));
        // Absence, modelled as absence — not as empty strings.
        assert_eq!(light.tag_object, None);
        assert_eq!(light.tagger, None);
        assert_eq!(light.message, None);
        assert!(!light.signed);

        let annotated = &tags[1];
        assert!(annotated.annotated);
        // `target` is the *peeled commit*, and the tag object is a different
        // object — the whole point of keeping both.
        assert_eq!(annotated.target, Oid(root.clone()));
        let tag_object_id = git_out(p, &["rev-parse", "refs/tags/v1.0"]);
        assert_ne!(tag_object_id, root, "an annotated tag has its own object");
        assert_eq!(annotated.tag_object, Some(Oid(tag_object_id)));
        assert_eq!(annotated.message.as_deref(), Some("one\n\nrelease notes"));
        assert!(!annotated.signed);
        // The tagger line is compared against git's own rendering of it, read
        // back out of the object — not against a string this test composed.
        let tagger_line = git_out(p, &["cat-file", "-p", "refs/tags/v1.0"])
            .lines()
            .find_map(|l| l.strip_prefix("tagger ").map(str::to_string))
            .expect("git wrote a tagger header");
        assert_eq!(annotated.tagger.as_deref(), Some(tagger_line.as_str()));
    }

    /// A tag object pointing at another tag object: `target` must be the
    /// commit at the end of the chain, while `tag_object` stays the outermost
    /// object — the one `refs/tags/<name>` literally holds.
    #[test]
    fn a_tag_of_a_tag_peels_to_the_commit_but_keeps_the_outer_object() {
        let dir = repo();
        let p = dir.path();
        let tip = git_out(p, &["rev-parse", "HEAD"]);
        annotate(&p.to_path_buf(), "inner", "inner tag", &tip);
        annotate(&p.to_path_buf(), "outer", "outer tag", "refs/tags/inner");

        let tags = read_tags(p).unwrap();
        let outer = tags.iter().find(|t| t.name == "outer").unwrap();
        assert!(outer.annotated);
        assert_eq!(outer.target, Oid(tip));
        assert_eq!(
            outer.tag_object,
            Some(Oid(git_out(p, &["rev-parse", "refs/tags/outer"])))
        );
    }

    /// A tag on a blob is legal git; it is left out of the listing rather than
    /// given a blob id in a field defined as the tagged commit.
    #[test]
    fn a_tag_that_does_not_name_a_commit_is_left_out() {
        let dir = repo();
        let p = dir.path();
        std::fs::write(p.join("f.txt"), b"contents").unwrap();
        let blob = git_out(p, &["hash-object", "-w", "f.txt"]);
        git(p, &["tag", "blob-tag", &blob]);
        git(p, &["tag", "commit-tag", "HEAD"]);

        let tags = read_tags(p).unwrap();
        assert_eq!(
            tags.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["commit-tag"],
            "the blob tag is skipped, and the commit tag still comes through"
        );
    }

    /// Branches must never leak into a tag listing, even though they live in
    /// the same ref store.
    #[test]
    fn branches_are_not_tags() {
        let dir = repo();
        let p = dir.path();
        git(p, &["branch", "feature"]);
        git(p, &["tag", "v9"]);
        let tags = read_tags(p).unwrap();
        assert_eq!(
            tags.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["v9"]
        );
    }

    /// The sort is a contract, and this is the test that can actually fail if
    /// it is removed.
    ///
    /// The two-tag case above cannot: with only `tip-marker` and `v1.0` in the
    /// store, *any* enumeration order has a decent chance of already being
    /// alphabetical, and deleting `records.sort_by(..)` left it green. Loose
    /// refs are enumerated in directory order — on ext4 that is filename-hash
    /// order, neither creation order nor alphabetical — so this plants twelve
    /// loose tags whose creation order is deliberately the reverse of their
    /// sorted order and asserts the exact sorted sequence. Twelve entries make
    /// an accidentally-sorted enumeration a 1-in-12! coincidence.
    #[test]
    fn many_loose_tags_come_back_sorted_whatever_order_the_ref_store_walks_them() {
        let dir = repo();
        let p = dir.path();
        let mut expected: Vec<String> = (0..12).map(|i| format!("v{i:02}-tag")).collect();
        // Created newest-name-first, so creation order is the reverse of sorted.
        for name in expected.iter().rev() {
            git(p, &["tag", name, "HEAD"]);
        }
        assert!(
            p.join(".git/refs/tags").join(&expected[0]).exists(),
            "these must stay loose — packed-refs is written sorted, which would \
             make this test pass without the sort"
        );
        expected.sort();
        assert_eq!(
            read_tags(p)
                .unwrap()
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>(),
            expected
        );
    }

    /// Packed refs take a different path through the ref store than loose
    /// ones; a tag must read identically either way.
    #[test]
    fn a_packed_tag_reads_the_same_as_a_loose_one() {
        let dir = repo();
        let p = dir.path();
        annotate(&p.to_path_buf(), "v2.0", "packed annotation", "HEAD");
        git(p, &["tag", "v2.0-light", "HEAD"]);
        let loose = read_tags(p).unwrap();
        git(p, &["pack-refs", "--all"]);
        assert!(
            !p.join(".git/refs/tags/v2.0").exists(),
            "pack-refs should have removed the loose tag file"
        );
        assert_eq!(read_tags(p).unwrap(), loose);
    }

    #[test]
    fn an_annotated_tag_with_a_blank_message_reports_no_message() {
        // Not a repository test: `git tag -a` refuses an empty message, so the
        // only way to reach this shape is a hand-built object. The rule itself
        // is pure, so it is proved on the pure function.
        assert_eq!(annotation_message(b"\n\n"), (None, false));
        assert_eq!(annotation_message(b"   \n"), (None, false));
        assert_eq!(annotation_message(b""), (None, false));
    }

    /// `(None, true)` is a real answer, not a theoretical one, and the two
    /// halves of it disagree: nothing was retained *and* bytes were dropped.
    ///
    /// This is the pair `handlers::tags::fit_message` exists to handle (#236
    /// review). The mapping there used to read
    /// `record.message.as_deref().and_then(|m| fit_message(m, ..))`, which
    /// short-circuits on the `None` and never looks at the flag — so a tag
    /// carrying arbitrarily much content past the cap went out on the wire as
    /// `message: null`, byte-identical to a tag with no annotation at all.
    ///
    /// Both legs are here on purpose. The pure leg pins the contract; the
    /// repository leg is the one that makes it a *reachable* contract rather
    /// than a shape only a unit test can construct — and it needs no
    /// `mktag`, only `git tag -a --cleanup=verbatim -F`, which preserves
    /// leading whitespace that the default `strip` cleanup would remove.
    #[test]
    fn an_annotation_that_is_all_whitespace_up_to_the_cap_reports_nothing_kept_but_cut() {
        // The pure rule.
        let mut raw = vec![b' '; MAX_TAG_MESSAGE_BYTES + 1];
        raw.extend_from_slice(b"CONTENT PAST THE CUTOFF\n");
        assert_eq!(
            annotation_message(&raw),
            (None, true),
            "nothing survived the cap, but the cut is still a fact"
        );

        // …and the same shape read back out of a real repository.
        let dir = repo();
        let p = dir.path();
        let body = " ".repeat(MAX_TAG_MESSAGE_BYTES + 1) + "CONTENT PAST THE CUTOFF\n";
        let message_file = p.join("annotation.txt");
        std::fs::write(&message_file, &body).expect("write the annotation body");
        let status = std::process::Command::new("git")
            .args(["tag", "-a", "--cleanup=verbatim", "-F"])
            .arg(&message_file)
            .args(["v-whitespace", "HEAD"])
            .current_dir(p)
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_DATE", "@1753300000 +0000")
            .status()
            .expect("git should be runnable");
        assert!(status.success(), "git tag -a --cleanup=verbatim failed");
        // The fixture is only interesting if git really kept the whitespace —
        // a cleanup mode that stripped it would make this test vacuous.
        let stored = git_out(p, &["cat-file", "tag", "v-whitespace"]);
        assert!(
            stored.contains("CONTENT PAST THE CUTOFF"),
            "the payload past the cap must be in the object"
        );

        let tags = read_tags(p).unwrap();
        let tag = tags.iter().find(|t| t.name == "v-whitespace").unwrap();
        assert!(tag.annotated);
        assert_eq!(
            tag.message, None,
            "nothing in the first 16 KiB was worth keeping"
        );
        assert!(
            tag.message_truncated,
            "…but the record must still carry the fact that content was cut, \
             or the server has nothing left to be honest with"
        );
    }

    #[test]
    fn a_message_is_capped_on_a_character_boundary_and_says_it_was_cut() {
        // Under the cap: kept whole, not flagged.
        let (msg, truncated) = annotation_message(b"short\n");
        assert_eq!(msg.as_deref(), Some("short"));
        assert!(!truncated);

        // Exactly at the cap: still not truncated — an off-by-one here would
        // flag every maximal message as cut.
        let exact = vec![b'x'; MAX_TAG_MESSAGE_BYTES];
        let (msg, truncated) = annotation_message(&exact);
        assert_eq!(msg.unwrap().len(), MAX_TAG_MESSAGE_BYTES);
        assert!(!truncated);

        // One byte over: flagged, and cut to the cap.
        let over = vec![b'x'; MAX_TAG_MESSAGE_BYTES + 1];
        let (msg, truncated) = annotation_message(&over);
        assert_eq!(msg.unwrap().len(), MAX_TAG_MESSAGE_BYTES);
        assert!(truncated);

        // A multi-byte character straddling the cap is dropped whole rather
        // than cut into a replacement character that was never in the object.
        let mut straddle = vec![b'x'; MAX_TAG_MESSAGE_BYTES - 1];
        straddle.extend_from_slice("é".as_bytes()); // 2 bytes: one in, one out
        straddle.extend_from_slice(b"tail");
        let (msg, truncated) = annotation_message(&straddle);
        let msg = msg.unwrap();
        assert!(truncated);
        assert_eq!(msg.len(), MAX_TAG_MESSAGE_BYTES - 1);
        assert!(
            !msg.contains('\u{FFFD}'),
            "a boundary-safe cut cannot invent a replacement character"
        );
    }

    /// A signed tag's signature is *detected*, and its armour is not left in
    /// the message. No gpg runs — this is the object parser splitting the
    /// block off, which is all M2.21b claims to do.
    #[test]
    fn a_signature_block_is_detected_and_kept_out_of_the_message() {
        let dir = repo();
        let p = dir.path();
        let tip = git_out(p, &["rev-parse", "HEAD"]);
        // Hand-built so the test needs no gpg key: `git mktag` writes exactly
        // the bytes given, including a signature block real gpg would produce.
        let body = format!(
            "object {tip}\n\
             type commit\n\
             tag v-signed\n\
             tagger Ada Lovelace <ada@example.com> 1753300000 +0000\n\
             \n\
             signed release\n\
             -----BEGIN PGP SIGNATURE-----\n\
             \n\
             not-real-armour\n\
             -----END PGP SIGNATURE-----\n"
        );
        let mut child = std::process::Command::new("git")
            .args(["mktag"])
            .current_dir(p)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("git should be runnable");
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(body.as_bytes())
                .unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git mktag failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        git(p, &["update-ref", "refs/tags/v-signed", &oid]);

        let tags = read_tags(p).unwrap();
        let signed = tags.iter().find(|t| t.name == "v-signed").unwrap();
        assert!(signed.signed, "the signature block must be detected");
        assert_eq!(signed.message.as_deref(), Some("signed release"));
        assert!(
            !signed.message.as_deref().unwrap().contains("PGP SIGNATURE"),
            "the armour belongs to the signature, not the message"
        );
    }
}
