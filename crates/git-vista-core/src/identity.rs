//! Stable, opaque, path-independent identity for repositories and worktrees.
//!
//! This module is the answer to a specific hazard: the current server addresses
//! "the repository" by a filesystem path held in process-global state, and a
//! browser tab that has been open for a while can ask the server to act on a
//! repository whose state has moved on underneath it. Paths are the wrong API
//! identity — they leak the server's filesystem, they aren't stable when a repo
//! is moved, and they don't distinguish the *shared repository* from one of its
//! *worktrees*. This module introduces value types that fix that, entirely in
//! pure code so they compile for both the native backend and the wasm frontend
//! and (de)serialize across the JSON boundary unchanged.
//!
//! The types:
//!
//! - [`RepositoryId`] — an opaque handle for a *shared repository* (its common
//!   git directory). Every worktree of one clone shares one `RepositoryId`.
//! - [`WorktreeId`] — an opaque handle for one *worktree* (its own git dir).
//!   The main working tree and each linked worktree get distinct `WorktreeId`s,
//!   even though they share a `RepositoryId`.
//! - [`ObjectId`] — a git object hash, *validated* (algorithm + hex) on
//!   construction, unlike the loose [`crate::model::Oid`] string used on the
//!   graph-drawing hot path.
//! - [`RepositoryGeneration`] — an opaque token summarising the observable state
//!   of a worktree (HEAD, refs, index, working tree). It advances whenever that
//!   observable state changes, so a stale client can be detected by comparing
//!   the generation it last saw against the current one.
//!
//! Why UUIDs, and why *derived* rather than random: `RepositoryId` and
//! `WorktreeId` are name-based (RFC 4122 v5) UUIDs computed from the repository's
//! canonicalised git directory. That makes them **stable** — the same repository
//! yields the same id across server restarts with no persisted table — while
//! staying **opaque and path-independent** to clients: the id is a 128-bit hash,
//! the path can't be recovered from it, and nothing outside this module ever
//! learns the path. Derivation is deterministic and randomness-free, which is
//! what lets this crate stay pure and wasm-compatible. Only the native backend
//! knows the paths and calls the derivation constructors; the frontend only ever
//! receives the finished ids as opaque strings.
//!
//! See `docs/adr/0001-repository-generation.md` for the generation algorithm and
//! the design decisions recorded here.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Namespace for [`RepositoryId`] derivation (v5). A fixed, arbitrary constant:
/// its only job is to keep repository ids in a different space from worktree and
/// generation hashes so the same input bytes can never collide across kinds.
const REPOSITORY_NAMESPACE: Uuid = Uuid::from_bytes([
    0x67, 0x69, 0x74, 0x2d, 0x76, 0x69, 0x73, 0x74, 0x61, 0x2d, 0x72, 0x65, 0x70, 0x6f, 0x69, 0x64,
]);

/// Namespace for [`WorktreeId`] derivation (v5). Distinct from
/// [`REPOSITORY_NAMESPACE`] so a repository and one of its worktrees can never
/// hash to the same id even if their identity strings coincided.
const WORKTREE_NAMESPACE: Uuid = Uuid::from_bytes([
    0x67, 0x69, 0x74, 0x2d, 0x76, 0x69, 0x73, 0x74, 0x61, 0x2d, 0x77, 0x6b, 0x74, 0x72, 0x65, 0x65,
]);

/// Namespace for the [`RepositoryGeneration`] digest (v5). Keeps generation
/// hashes in their own space from the id spaces above.
const GENERATION_NAMESPACE: Uuid = Uuid::from_bytes([
    0x67, 0x69, 0x74, 0x2d, 0x76, 0x69, 0x73, 0x74, 0x61, 0x2d, 0x67, 0x65, 0x6e, 0x65, 0x72, 0x61,
]);

/// An opaque, stable handle for a **shared repository** — the thing a set of
/// worktrees have in common (git's *common directory*). The API addresses
/// repositories by this id, never by a filesystem path.
///
/// Derived (v5) from the canonicalised common directory, so it is stable across
/// restarts and moves-that-preserve-the-directory, yet opaque: a client cannot
/// recover the path from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryId(Uuid);

impl RepositoryId {
    /// Derive the id for the shared repository whose canonical common directory
    /// is `canonical_common_dir`.
    ///
    /// The caller (the native backend) is responsible for canonicalising the
    /// path first — symlinks resolved, no trailing separator — so that two
    /// spellings of the same directory yield one id. This function is pure: it
    /// hashes the bytes it is given and never touches the filesystem.
    pub fn from_common_dir(canonical_common_dir: &str) -> Self {
        Self(Uuid::new_v5(
            &REPOSITORY_NAMESPACE,
            canonical_common_dir.as_bytes(),
        ))
    }

    /// The underlying UUID, for callers that need the raw 128-bit value.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for RepositoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for RepositoryId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// An opaque, stable handle for a single **worktree** — one checked-out working
/// tree with its own git directory. Distinct from [`RepositoryId`]: the main
/// working tree and each linked worktree of one clone share a `RepositoryId` but
/// each carry their own `WorktreeId`.
///
/// Derived (v5) from the canonicalised git directory of the worktree, so it is
/// stable and opaque on the same terms as [`RepositoryId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorktreeId(Uuid);

impl WorktreeId {
    /// Derive the id for the worktree whose canonical git directory is
    /// `canonical_git_dir`.
    ///
    /// For the main working tree this is the repository's git dir; for a linked
    /// worktree it is `…/.git/worktrees/<name>`. Because those differ, each
    /// worktree gets a distinct id while all share one [`RepositoryId`] (their
    /// common dir). Pure: hashes the given bytes, never reads the filesystem.
    pub fn from_git_dir(canonical_git_dir: &str) -> Self {
        Self(Uuid::new_v5(
            &WORKTREE_NAMESPACE,
            canonical_git_dir.as_bytes(),
        ))
    }

    /// The underlying UUID, for callers that need the raw 128-bit value.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for WorktreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for WorktreeId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// The opaque address the API uses to select what to act on: a repository and,
/// within it, a specific worktree. This is the ID-based replacement for "a
/// filesystem path in process-global state".
///
/// It is deliberately *not* a snapshot — it carries no [`RepositoryGeneration`].
/// It names *which* worktree; the generation names *which state* of that worktree
/// and is checked separately at the point of a mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepositoryHandle {
    /// The shared repository.
    pub repository: RepositoryId,
    /// The worktree within it that the request addresses.
    pub worktree: WorktreeId,
}

impl RepositoryHandle {
    /// Bundle a repository and one of its worktrees into a selector.
    pub fn new(repository: RepositoryId, worktree: WorktreeId) -> Self {
        Self {
            repository,
            worktree,
        }
    }
}

/// The hash algorithm behind an [`ObjectId`]. Git repositories are SHA-1 today
/// and SHA-256 under the object-format transition; we validate both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    /// 160-bit SHA-1: 40 lowercase hex characters.
    Sha1,
    /// 256-bit SHA-256: 64 lowercase hex characters.
    Sha256,
}

impl HashAlgorithm {
    /// The number of hex characters an id of this algorithm has.
    pub const fn hex_len(self) -> usize {
        match self {
            HashAlgorithm::Sha1 => 40,
            HashAlgorithm::Sha256 => 64,
        }
    }

    /// Classify by hex length, or `None` if `len` matches no known algorithm.
    fn from_hex_len(len: usize) -> Option<Self> {
        match len {
            40 => Some(HashAlgorithm::Sha1),
            64 => Some(HashAlgorithm::Sha256),
            _ => None,
        }
    }
}

/// Why an [`ObjectId`] string was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectIdError {
    /// The length matches no supported hash algorithm.
    BadLength(usize),
    /// A character outside `[0-9a-f]` was found (uppercase hex is rejected: git
    /// emits lowercase, so normalising here would hide a caller passing the
    /// wrong thing).
    NonHexCharacter,
}

impl fmt::Display for ObjectIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObjectIdError::BadLength(n) => write!(
                f,
                "object id has {n} hex characters; expected 40 (SHA-1) or 64 (SHA-256)"
            ),
            ObjectIdError::NonHexCharacter => {
                write!(f, "object id contains a non-lowercase-hex character")
            }
        }
    }
}

impl std::error::Error for ObjectIdError {}

/// A git object id (commit, tree, blob, or tag), **validated on construction**:
/// its length must match a supported [`HashAlgorithm`] and every character must
/// be lowercase hex. Use this wherever an id is identity (generation inputs,
/// operation preconditions) rather than a value flowing to the renderer, where
/// the looser [`crate::model::Oid`] string is enough.
///
/// Serialises as its hex string, and *validates on deserialisation* — a
/// malformed id from the wire is a hard error, not a value that flows onward.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId {
    hex: String,
    algorithm: HashAlgorithm,
}

impl ObjectId {
    /// Parse and validate a hex object id. Returns [`ObjectIdError`] if the
    /// length matches no algorithm or a non-lowercase-hex character is present.
    pub fn parse(hex: &str) -> Result<Self, ObjectIdError> {
        let algorithm =
            HashAlgorithm::from_hex_len(hex.len()).ok_or(ObjectIdError::BadLength(hex.len()))?;
        if !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ObjectIdError::NonHexCharacter);
        }
        Ok(Self {
            hex: hex.to_string(),
            algorithm,
        })
    }

    /// The full hex string.
    pub fn as_str(&self) -> &str {
        &self.hex
    }

    /// Which hash algorithm this id belongs to (inferred from its length).
    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }

    /// The conventional 7-character abbreviation (or the whole id if shorter).
    pub fn short(&self) -> &str {
        &self.hex[..self.hex.len().min(7)]
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex)
    }
}

impl FromStr for ObjectId {
    type Err = ObjectIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for ObjectId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.hex)
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        ObjectId::parse(&hex).map_err(serde::de::Error::custom)
    }
}

/// An opaque token summarising the observable state of a worktree at a moment in
/// time. Two reads of the same unchanged worktree produce equal generations;
/// any change to HEAD, a ref, the index, or the working tree produces a
/// different one. That is exactly what a stale-tab check needs: the client
/// records the generation it reviewed, and a mutation is allowed only while the
/// current generation still equals it.
///
/// It is an **equality token, not a sequence number**: a *content digest*, not a
/// monotonic counter. `==` means "same state" and `!=` means "state changed", and
/// that is the *only* defined comparison. The inner `u64` is a hash — it can move
/// up, down, or back to a prior value when state is reverted — so callers must
/// never infer newer/older, sort by it, or treat a larger value as "ahead". That
/// contract is enforced here by *not* deriving `PartialOrd`/`Ord`: the type
/// supports equality and hashing (so it can key a map) but not ordering. See the
/// ADR (`docs/adr/0001-repository-generation.md`) for why equality suffices, why a
/// counter was rejected, and how the algorithm may be versioned later. Build one
/// with [`GenerationInputs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryGeneration(u64);

impl RepositoryGeneration {
    /// Wrap a raw digest value. Prefer [`GenerationInputs::generation`]; this is
    /// for round-tripping a value already computed (e.g. read back from a
    /// client's request).
    pub fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// The raw digest value.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RepositoryGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// The observable inputs that define a [`RepositoryGeneration`], collected in a
/// canonical form so the digest is independent of the order the caller supplies
/// them in.
///
/// The generation advances when any of these changes:
///
/// - **HEAD** — its symbolic target (which branch is checked out, or detached)
///   and the commit it resolves to.
/// - **Every ref** — each branch, tag, and remote-tracking ref, and its target.
/// - **The index** — a digest of the staging area.
/// - **The working tree** — a digest of tracked modifications and untracked
///   files.
///
/// The native backend fills these from a repository read; this type and its
/// hashing are pure. Fields are keyed and the key/value pairs sorted before
/// hashing, so supplying refs in any order yields the same generation, while a
/// ref rename or retarget changes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerationInputs {
    fields: Vec<(String, String)>,
}

impl GenerationInputs {
    /// An empty set of inputs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record HEAD: its symbolic target name (e.g. `refs/heads/main`, or `None`
    /// when detached) and the object it resolves to (`None` for an unborn HEAD).
    pub fn head(
        &mut self,
        symbolic_target: Option<&str>,
        resolved: Option<&ObjectId>,
    ) -> &mut Self {
        self.field(
            "head",
            format!(
                "{}\u{0}{}",
                symbolic_target.unwrap_or(""),
                resolved.map(ObjectId::as_str).unwrap_or("")
            ),
        )
    }

    /// Record one ref by its full name (e.g. `refs/heads/main`) and its target.
    pub fn reference(&mut self, full_name: &str, target: &ObjectId) -> &mut Self {
        self.field(format!("ref:{full_name}"), target.as_str().to_string())
    }

    /// Record a digest of the index/staging area.
    pub fn index(&mut self, digest: &str) -> &mut Self {
        self.field("index", digest.to_string())
    }

    /// Record a digest of the working-tree state (tracked changes + untracked).
    pub fn worktree(&mut self, digest: &str) -> &mut Self {
        self.field("worktree", digest.to_string())
    }

    /// Low-level: record an arbitrary keyed field. Keys must be unique within a
    /// build (later writes to the same key overwrite the earlier value) so that
    /// the digest is well-defined; the typed methods above namespace their keys
    /// to guarantee this.
    pub fn field(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let key = key.into();
        let value = value.into();
        match self.fields.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.fields.push((key, value)),
        }
        self
    }

    /// Compute the generation from the inputs recorded so far.
    ///
    /// The encoding is unambiguous and order-independent: fields are sorted by
    /// key, then each `(key, value)` is written length-prefixed so no two
    /// distinct input sets can produce the same byte stream. The byte stream is
    /// hashed (v5) and the leading 64 bits are taken as the generation.
    pub fn generation(&self) -> RepositoryGeneration {
        let mut fields = self.fields.clone();
        fields.sort();

        let mut bytes = Vec::new();
        for (key, value) in &fields {
            bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
            bytes.extend_from_slice(key.as_bytes());
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }

        let digest = Uuid::new_v5(&GENERATION_NAMESPACE, &bytes);
        let head = <[u8; 8]>::try_from(&digest.as_bytes()[..8]).expect("uuid is 16 bytes");
        RepositoryGeneration(u64::from_be_bytes(head))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid 40-char SHA-1 and 64-char SHA-256 for reuse.
    const SHA1_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA1_B: &str = "89abcdef0123456789abcdef0123456789abcdef";
    const SHA256_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn oid(hex: &str) -> ObjectId {
        ObjectId::parse(hex).expect("valid oid in test")
    }

    // --- RepositoryId / WorktreeId ------------------------------------------

    #[test]
    fn repository_id_is_stable_for_the_same_path() {
        let a = RepositoryId::from_common_dir("/srv/repos/app/.git");
        let b = RepositoryId::from_common_dir("/srv/repos/app/.git");
        assert_eq!(a, b, "same common dir must yield the same id across calls");
    }

    #[test]
    fn repository_id_differs_by_path() {
        let a = RepositoryId::from_common_dir("/srv/repos/app/.git");
        let b = RepositoryId::from_common_dir("/srv/repos/other/.git");
        assert_ne!(a, b);
    }

    #[test]
    fn worktree_and_repository_identity_are_distinct() {
        // A repository and a worktree derived from the *same* directory string
        // must not collide: the two id spaces are separated by namespace.
        let path = "/srv/repos/app/.git";
        let repo = RepositoryId::from_common_dir(path);
        let worktree = WorktreeId::from_git_dir(path);
        assert_ne!(
            repo.as_uuid(),
            worktree.as_uuid(),
            "repo and worktree ids must not collide even from identical input"
        );
    }

    #[test]
    fn worktrees_of_one_repo_share_repo_id_but_differ() {
        // Two linked worktrees: same common dir, different git dirs.
        let common = "/srv/repos/app/.git";
        let repo_a = RepositoryId::from_common_dir(common);
        let repo_b = RepositoryId::from_common_dir(common);
        let wt_main = WorktreeId::from_git_dir("/srv/repos/app/.git");
        let wt_linked = WorktreeId::from_git_dir("/srv/repos/app/.git/worktrees/feature");
        assert_eq!(repo_a, repo_b, "worktrees share one repository id");
        assert_ne!(wt_main, wt_linked, "each worktree has its own id");
    }

    // --- Path independence / serde ------------------------------------------

    #[test]
    fn ids_roundtrip_through_json_as_opaque_strings() {
        let repo = RepositoryId::from_common_dir("/srv/repos/app/.git");
        let json = serde_json::to_string(&repo).unwrap();
        // Serialises as a bare, opaque string — no path anywhere in it.
        assert!(json.starts_with('"') && json.ends_with('"'));
        assert!(
            !json.contains("srv") && !json.contains("repos"),
            "the id must not leak the path it was derived from: {json}"
        );
        let back: RepositoryId = serde_json::from_str(&json).unwrap();
        assert_eq!(repo, back);
    }

    #[test]
    fn worktree_id_roundtrips_through_json() {
        let wt = WorktreeId::from_git_dir("/srv/repos/app/.git");
        let json = serde_json::to_string(&wt).unwrap();
        let back: WorktreeId = serde_json::from_str(&json).unwrap();
        assert_eq!(wt, back);
    }

    #[test]
    fn ids_roundtrip_through_display_and_fromstr() {
        let repo = RepositoryId::from_common_dir("/srv/repos/app/.git");
        assert_eq!(repo.to_string().parse::<RepositoryId>().unwrap(), repo);
        let wt = WorktreeId::from_git_dir("/srv/repos/app/.git");
        assert_eq!(wt.to_string().parse::<WorktreeId>().unwrap(), wt);
    }

    #[test]
    fn repository_handle_roundtrips_through_json() {
        let handle = RepositoryHandle::new(
            RepositoryId::from_common_dir("/srv/repos/app/.git"),
            WorktreeId::from_git_dir("/srv/repos/app/.git"),
        );
        let json = serde_json::to_string(&handle).unwrap();
        let back: RepositoryHandle = serde_json::from_str(&json).unwrap();
        assert_eq!(handle, back);
    }

    // --- ObjectId ------------------------------------------------------------

    #[test]
    fn object_id_accepts_sha1_and_sha256() {
        assert_eq!(oid(SHA1_A).algorithm(), HashAlgorithm::Sha1);
        assert_eq!(oid(SHA256_A).algorithm(), HashAlgorithm::Sha256);
    }

    #[test]
    fn hash_algorithm_hex_len_matches_ids() {
        assert_eq!(HashAlgorithm::Sha1.hex_len(), SHA1_A.len());
        assert_eq!(HashAlgorithm::Sha256.hex_len(), SHA256_A.len());
    }

    #[test]
    fn object_id_rejects_bad_length() {
        assert_eq!(ObjectId::parse("abc"), Err(ObjectIdError::BadLength(3)));
        assert_eq!(ObjectId::parse(""), Err(ObjectIdError::BadLength(0)));
    }

    #[test]
    fn object_id_rejects_non_lowercase_hex() {
        // Right length (40), but an uppercase letter and a non-hex letter.
        let upper = "0123456789ABCDEF0123456789abcdef01234567";
        assert_eq!(ObjectId::parse(upper), Err(ObjectIdError::NonHexCharacter));
        let zed = "0123456789abcdefz123456789abcdef01234567";
        assert_eq!(ObjectId::parse(zed), Err(ObjectIdError::NonHexCharacter));
    }

    #[test]
    fn object_id_short_is_seven_chars() {
        assert_eq!(oid(SHA1_A).short(), "0123456");
    }

    #[test]
    fn object_id_roundtrips_through_json() {
        let id = oid(SHA1_A);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{SHA1_A}\""));
        let back: ObjectId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn object_id_deserialization_validates() {
        // A malformed id on the wire is a hard error, not a silently-kept value.
        let bad = serde_json::from_str::<ObjectId>("\"nope\"");
        assert!(bad.is_err());
    }

    // --- RepositoryGeneration ------------------------------------------------

    fn head_ref_gen(head: &str, refs: &[(&str, &str)]) -> RepositoryGeneration {
        let mut inputs = GenerationInputs::new();
        inputs.head(Some("refs/heads/main"), Some(&oid(head)));
        for (name, target) in refs {
            inputs.reference(name, &oid(target));
        }
        inputs.index("clean").worktree("clean");
        inputs.generation()
    }

    #[test]
    fn generation_is_stable_for_the_same_state() {
        let a = head_ref_gen(SHA1_A, &[("refs/heads/main", SHA1_A)]);
        let b = head_ref_gen(SHA1_A, &[("refs/heads/main", SHA1_A)]);
        assert_eq!(a, b, "identical observable state → identical generation");
    }

    #[test]
    fn generation_is_independent_of_ref_order() {
        let a = head_ref_gen(
            SHA1_A,
            &[("refs/heads/main", SHA1_A), ("refs/tags/v1", SHA1_B)],
        );
        let b = head_ref_gen(
            SHA1_A,
            &[("refs/tags/v1", SHA1_B), ("refs/heads/main", SHA1_A)],
        );
        assert_eq!(a, b, "ref order must not affect the generation");
    }

    #[test]
    fn generation_advances_when_head_moves() {
        let before = head_ref_gen(SHA1_A, &[("refs/heads/main", SHA1_A)]);
        let after = head_ref_gen(SHA1_B, &[("refs/heads/main", SHA1_B)]);
        assert_ne!(before, after, "moving HEAD must advance the generation");
    }

    #[test]
    fn generation_advances_when_a_ref_is_added() {
        let before = head_ref_gen(SHA1_A, &[("refs/heads/main", SHA1_A)]);
        let after = head_ref_gen(
            SHA1_A,
            &[("refs/heads/main", SHA1_A), ("refs/heads/feature", SHA1_B)],
        );
        assert_ne!(before, after, "a new ref must advance the generation");
    }

    #[test]
    fn generation_advances_when_index_changes() {
        let mut clean = GenerationInputs::new();
        clean.index("clean").worktree("clean");
        let mut staged = GenerationInputs::new();
        staged.index("one-staged-file").worktree("clean");
        assert_ne!(
            clean.generation(),
            staged.generation(),
            "staging must advance the generation"
        );
    }

    #[test]
    fn generation_advances_when_worktree_changes() {
        let mut clean = GenerationInputs::new();
        clean.index("clean").worktree("clean");
        let mut dirty = GenerationInputs::new();
        dirty.index("clean").worktree("one-modified-file");
        assert_ne!(clean.generation(), dirty.generation());
    }

    #[test]
    fn field_overwrites_rather_than_duplicates() {
        // Writing the same key twice keeps the last value, so the digest is
        // well-defined even if a caller records HEAD twice.
        let mut a = GenerationInputs::new();
        a.field("head", "first").field("head", "second");
        let mut b = GenerationInputs::new();
        b.field("head", "second");
        assert_eq!(a.generation(), b.generation());
    }

    #[test]
    fn distinct_fields_do_not_alias_via_concatenation() {
        // Length-prefixing means ("ab","c") and ("a","bc") can't collide.
        let mut a = GenerationInputs::new();
        a.field("ab", "c");
        let mut b = GenerationInputs::new();
        b.field("a", "bc");
        assert_ne!(a.generation(), b.generation());
    }

    #[test]
    fn generation_roundtrips_through_json_and_raw() {
        let gen = head_ref_gen(SHA1_A, &[("refs/heads/main", SHA1_A)]);
        let json = serde_json::to_string(&gen).unwrap();
        let back: RepositoryGeneration = serde_json::from_str(&json).unwrap();
        assert_eq!(gen, back);
        assert_eq!(RepositoryGeneration::from_raw(gen.as_u64()), gen);
    }
}
