//! The durable operation journal and recovery references (M1.09, #62).
//!
//! M1.08 gave every mutation identity, a lifecycle, and a replayable result —
//! but only for the life of the process. This module is what survives a
//! restart:
//!
//! 1. **A SQLite journal** (one file, process-wide, at
//!    [`crate::state::operations_db_path`]) holding every [`OperationStatus`]
//!    this server has admitted, keyed by the client's own
//!    [`IdempotencyKey`]. [`persist`] writes a row on admission and again on
//!    the terminal transition; [`recover`] reads them all back at startup,
//!    closes out anything left non-terminal (a process that died mid-operation
//!    left no answer, and no answer ever comes now — see below), and hands the
//!    result to [`crate::operations::rehydrate`] so `GET /api/operations/{id}`
//!    and idempotency replay both keep working across a restart.
//! 2. **Recovery refs** — [`write_recovery_ref`] pins the pre-operation tip a
//!    [`RecoveryStrategy`] names, as a ref under `refs/git-vista/recovery/`,
//!    **never** `refs/heads/` or `refs/tags/`: the namespace prefix is what
//!    makes "never overwrite a user ref" true by construction rather than by
//!    care. A recovery ref outlives the SQLite row that describes it — restart
//!    the server, and the *pointer* into the object graph is still there even
//!    though the row was closed out as interrupted.
//!
//! ## Why closing out a non-terminal record is correct, not just convenient
//!
//! A `Running` record names a `tokio::spawn`ed task. That task belongs to the
//! process that spawned it; a restart doesn't suspend and resume it, it erases
//! it. There is no way to know, from the row alone, whether the git command it
//! was running landed or not — the record was `Running`, not `Succeeded`,
//! *because* the process died before finding out. Recovery therefore does not
//! guess: it marks the record `Failed` with a message that says exactly that,
//! and leaves the real answer to the staleness gate (ADR 0018) the next time
//! the client acts — the same posture ADR 0019 already takes for a mutation
//! guard's holder dying mid-hold.
//!
//! ## Redaction
//!
//! [`crate::operations`] logs failures with `eprintln!`, and a [`GitOperation`]
//! can carry free text a user typed — a commit message. Every log line here
//! goes through [`redact_operation`], which keeps only the operation's kind
//! (`commit_on_head`, `push_branch`, …) and never the fields. The database row
//! itself is not redacted — persisting operation intent verbatim is the whole
//! point of the journal — only what reaches the server's own stderr.

use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};

use rusqlite::{params, Connection};

use git_vista_protocol::{
    CommitOid, GenerationToken, GitOperation, IdempotencyKey, OperationHash, OperationId,
    OperationStage, OperationState, OperationStatus, RecoveryStrategy, RepositoryToken,
    UnixSeconds, WorktreeToken,
};

/// The schema's `PRAGMA user_version`. Bump this — and add a migration arm to
/// [`open_at`]'s match plus a `migrate_v{n}_to_v{n+1}` function, not a silent
/// edit to [`migrate_fresh`] alone — the day a column changes shape.
///
/// The convention this file follows, now that there is more than one version:
/// a brand-new database (version 0) gets [`migrate_fresh`]'s *current-shape*
/// schema directly, in one step; an existing database reporting an older
/// version runs the matching incremental migration in place, against its real
/// rows. [`open_at`] still refuses (`UnknownSchemaVersion`) any version it has
/// no arm for, so a downgrade or a corrupted `user_version` fails loud.
///
/// "Loud" is relative and worth stating: [`recover`] treats every
/// [`DurableError`] as best-effort-failed and starts with an empty history, so
/// a version this build has no arm for costs the user their whole browsable
/// journal with no error reachable from the browser. That is exactly why
/// bumping this constant without adding the matching arm is not a style
/// choice — see [`migrate_v1_to_v2`].
const SCHEMA_VERSION: i32 = 2;

/// The namespace every recovery ref lives under. Never `refs/heads/` or
/// `refs/tags/`, so "never overwrites a user ref" holds by construction: no
/// user-chosen name can ever resolve into this prefix, because git refs are
/// namespaced by their full path and this path is fixed and app-owned.
const RECOVERY_REF_PREFIX: &str = "refs/git-vista/recovery";

/// `refs/git-vista/recovery/<operation.as_str()>` — the one place this
/// namespacing is spelled out, shared by [`write_recovery_ref`] (the writer)
/// and, since M3.25 (#78), `crate::recovery_center`'s live classification
/// (the reader), so the two can never drift onto different ref names for the
/// same operation.
pub(crate) fn recovery_ref_name(operation: &OperationId) -> String {
    format!("{RECOVERY_REF_PREFIX}/{}", operation.as_str())
}

/// Why the durable layer couldn't do what was asked. Every caller treats every
/// variant the same way: log it and carry on with an in-memory-only operation.
/// The journal is a safety net, not a dependency the git operation it describes
/// can be made to wait on indefinitely.
#[derive(Debug)]
pub(crate) enum DurableError {
    /// The database exists but was written by a schema version this build
    /// doesn't know. Refused rather than guessed at — a mismatched column
    /// shape read as the wrong type is worse than a journal that starts empty.
    UnknownSchemaVersion(i32),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    /// The `spawn_blocking` task running a query panicked — it never returned
    /// a `Result` at all, so this is the one variant built from a
    /// `tokio::task::JoinError` rather than a rusqlite/io error.
    ///
    /// It exists because of [`list_operations`], the one read path here that
    /// does *not* swallow a failure the way [`persist`]/[`recover`] do: those
    /// are best-effort because the git operation they describe already ran
    /// regardless of the journal's health, whereas a history read has nothing
    /// else to fall back on — silently answering "no rows" would read as
    /// "nothing happened" to a caller who cannot tell that from "we couldn't
    /// check".
    TaskPanicked(String),
}

impl std::fmt::Display for DurableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DurableError::UnknownSchemaVersion(v) => write!(
                f,
                "operations.sqlite3 has schema version {v}, this build knows version {SCHEMA_VERSION}"
            ),
            DurableError::Sqlite(e) => write!(f, "{e}"),
            DurableError::Io(e) => write!(f, "{e}"),
            DurableError::TaskPanicked(e) => write!(f, "the journal task panicked: {e}"),
        }
    }
}

impl From<rusqlite::Error> for DurableError {
    fn from(e: rusqlite::Error) -> Self {
        DurableError::Sqlite(e)
    }
}

// ---------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------

static DB: OnceLock<StdMutex<Connection>> = OnceLock::new();

/// Serializes the *opening* of [`DB`], closing the race [`db`]'s old comment
/// dismissed as harmless. See [`db`] for why it wasn't.
static DB_INIT: StdMutex<()> = StdMutex::new(());

/// Open (creating and migrating if needed) the journal at `path`. Split from
/// the process-wide [`DB`] singleton so a test can point this at a throwaway
/// file instead of the real one.
fn open_at(path: &Path) -> Result<Connection, DurableError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(DurableError::Io)?;
    }
    let conn = Connection::open(path)?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 => migrate_fresh(&conn)?,
        // M3.25 (#78): a v1 journal already exists on disk (this server has
        // shipped since M1.09) and must gain `recovers_operation` and the
        // Recovery Center's two read indexes without losing a row. An
        // explicit arm here — not a silent fallthrough to
        // `UnknownSchemaVersion` — is load-bearing: without it, bumping
        // `SCHEMA_VERSION` refuses to open every already-installed journal,
        // permanently, with no shell available to fix it.
        1 => migrate_v1_to_v2(&conn)?,
        v if v == SCHEMA_VERSION => {}
        v => return Err(DurableError::UnknownSchemaVersion(v)),
    }
    Ok(conn)
}

/// Create the schema from nothing, at its current (v2) shape, for a database
/// that has never been opened before (`PRAGMA user_version` reads 0). A fresh
/// database has no existing rows to preserve and no earlier shape to step
/// through, so it gets `recovers_operation` and both indexes directly in the
/// initial `CREATE TABLE` rather than being created at v1 and then migrated —
/// there is nothing an `ALTER TABLE` would need to add a column to yet.
fn migrate_fresh(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE operations (
            id                 TEXT PRIMARY KEY,
            idempotency_key    TEXT NOT NULL UNIQUE,
            state              TEXT NOT NULL,
            stage              TEXT NOT NULL,
            operation_json     TEXT NOT NULL,
            operation_hash     TEXT NOT NULL,
            repository         TEXT NOT NULL,
            worktree           TEXT NOT NULL,
            accepted_at        INTEGER NOT NULL,
            ended_at           INTEGER,
            status             INTEGER,
            message            TEXT,
            generation         TEXT,
            recovery_json      TEXT,
            recovers_operation TEXT
        );
        CREATE INDEX idx_operations_history ON operations(accepted_at, id);
        CREATE INDEX idx_operations_recovers ON operations(recovers_operation)
            WHERE recovers_operation IS NOT NULL;",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Migrate an existing schema-version-1 database — every `operations.sqlite3`
/// written before the Recovery Center (M3.25, #78) — up to version 2 in
/// place, preserving every existing row.
///
/// `ALTER TABLE ... ADD COLUMN recovers_operation TEXT` backfills every
/// existing row's new column with `NULL`, which is the honest value: none of
/// those operations could have been the executed recovery of another, since
/// nothing could record it. `idx_operations_history` is the keyset index
/// `crate::recovery_center`'s paginated read walks; `idx_operations_recovers`
/// is the partial index behind "was this operation ever recovered", which is
/// a read-time lookup rather than a mutable flag on the original row.
///
/// Run as **one transaction** together with the `user_version` bump, not as
/// separate autocommitted statements: `execute_batch` alone commits each
/// statement independently, so a process death between the `ALTER` and the
/// pragma write would leave the database at version 1 with the column already
/// present. The next startup would re-enter this same arm, re-run the
/// `ALTER`, hit SQLite's "duplicate column name", and fail permanently —
/// `open_at` returning `Err` on every future start, with no shell-accessible
/// way for the user to intervene. The transaction makes the migration
/// all-or-nothing so that state cannot arise.
///
/// [`migrate_fresh`] above is deliberately *not* given the same wrapping: a
/// half-created, still-empty database is recreated cleanly on the next start,
/// where a v1 database holds a user's real operation history.
fn migrate_v1_to_v2(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "ALTER TABLE operations ADD COLUMN recovers_operation TEXT;
         CREATE INDEX idx_operations_history ON operations(accepted_at, id);
         CREATE INDEX idx_operations_recovers ON operations(recovers_operation)
             WHERE recovers_operation IS NOT NULL;",
    )?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

/// The process-wide connection, opened on first use. A `std` mutex, taken only
/// inside `spawn_blocking` closures — the same "never across an await"
/// discipline [`crate::coordinator`] and [`crate::operations`] follow, just
/// with `rusqlite::Connection` (which is `Send`, not `Sync`) in the slot.
///
/// **Root cause of #158.** This used to read `DB.get().is_none()` and, if so,
/// call `open_at` unconditionally — on the stated assumption that "opening
/// twice on a race is harmless (both succeed, one is discarded)". That's
/// false: `open_at` calls `migrate`, which runs `CREATE TABLE` against the
/// on-disk file (`db_path()` is one file shared by every test in this binary,
/// same as it's one file per server process in production). Two threads that
/// both observe `DB.get().is_none()` before either has finished migrating
/// race two *separate* `rusqlite::Connection`s against that same file; SQLite
/// allows only one writer, so the loser's `CREATE TABLE` — or, once the
/// winner has committed, the loser's own first statement on its still-live
/// but now-stale connection — returns `SQLITE_BUSY` ("database is locked").
/// `persist()` treats that as a best-effort failure and only logs it, so the
/// operation itself proceeds and finishes normally in memory, but its journal
/// row is left stuck in a non-terminal state. The next call to [`recover`] —
/// which assumes any non-terminal row is an orphan from a crashed process —
/// then correctly-by-its-own-rules but wrongly-in-fact marks that row
/// `Failed`, even though the operation actually succeeded. That is exactly
/// the `left: Failed, right: Succeeded` assertion in
/// `lifecycle_suite::a_finished_operation_is_durable_by_the_time_the_request_returns`:
/// not a read-too-early bug, a real terminal state — just one written by a
/// journal-write failure this function's old init race made possible.
///
/// `DB_INIT` fixes this with plain double-checked locking: only the thread
/// holding `DB_INIT` ever calls `open_at`, so `migrate` runs against the file
/// exactly once, and every other thread either finds `DB` already set (fast
/// path, no lock contention after startup) or blocks on `DB_INIT` until the
/// first opener finishes and then reads the same, single, fully-migrated
/// connection. `OnceLock::get_or_try_init` would fit better once stable —
/// this is the workaround, not a preference.
fn db() -> Result<&'static StdMutex<Connection>, DurableError> {
    if DB.get().is_none() {
        let _init_guard = DB_INIT.lock().expect("db init lock");
        // Re-check with the init lock held: another thread may have finished
        // opening while this one was waiting for the lock, in which case
        // opening again here would be exactly the race this guards against.
        if DB.get().is_none() {
            let conn = open_at(&db_path())?;
            let _ = DB.set(StdMutex::new(conn));
        }
    }
    Ok(DB.get().expect("just initialized above"))
}

/// A fresh, private, fully-migrated journal — a distinct file, distinct
/// connection, not the shared process-wide [`DB`].
///
/// For a test whose job is to fabricate a "crashed process" row and prove
/// [`recover`]'s close-out logic, not to exercise the shared journal itself.
/// [`recover`] cannot tell a genuinely orphaned row from another test's
/// operation that is simply still running (see its own doc comment); every
/// test in this binary shares one `DB`, so calling the real [`persist`] /
/// [`recover`] to seed and then sweep a synthetic row risks marking some
/// other, real, concurrently in-flight test's row `Failed` too — which is
/// what issue #158 actually was. Pair with [`persist_to`] / [`recover_from`].
///
/// Leaks its `TempDir` and its `Connection` deliberately: this returns
/// `&'static` for the same reason `DB` is `'static` (spawned onto
/// `spawn_blocking`, which requires it), the directory only needs to live as
/// long as the test process does, and nothing has to clean it up any more
/// than [`db_path`]'s shared `TEST_DB_DIR` does.
#[cfg(test)]
pub(crate) fn open_private() -> &'static StdMutex<Connection> {
    let dir = tempfile::tempdir().expect("a throwaway dir for an isolated test journal connection");
    let path = dir.path().join("operations.sqlite3");
    let conn = open_at(&path).expect("a fresh, isolated journal opens cleanly");
    std::mem::forget(dir);
    Box::leak(Box::new(StdMutex::new(conn)))
}

/// [`persist`], against an explicit connection (typically from
/// [`open_private`]) instead of the shared journal.
#[cfg(test)]
pub(crate) async fn persist_to(
    conn: &'static StdMutex<Connection>,
    key: IdempotencyKey,
    status: OperationStatus,
) {
    let kind = redact_operation(&status.operation);
    let result = tokio::task::spawn_blocking(move || persist_blocking(conn, &key, &status)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!(
            "git-vista: couldn't persist operation ({kind}) to an isolated test journal: {e}"
        ),
        Err(e) => eprintln!("git-vista: the journal write task ({kind}) panicked: {e}"),
    }
}

/// [`recover`], against an explicit connection (typically from
/// [`open_private`]) instead of the shared journal — safe to call mid-suite
/// precisely because nothing else can be writing to a private connection.
#[cfg(test)]
pub(crate) async fn recover_from(conn: &'static StdMutex<Connection>) -> RecoveredJournal {
    let loaded = tokio::task::spawn_blocking(move || recover_blocking(conn)).await;
    match loaded {
        Ok(Ok(journal)) => journal,
        Ok(Err(e)) => {
            eprintln!("git-vista: couldn't open the isolated test journal: {e}");
            RecoveredJournal::default()
        }
        Err(e) => {
            eprintln!("git-vista: the isolated-journal recovery task panicked: {e}");
            RecoveredJournal::default()
        }
    }
}

/// The database this process writes to. In production this is the real,
/// persistent [`crate::state::operations_db_path`]; under `cargo test` it is a
/// throwaway file in a directory created once per test binary process.
///
/// The distinction matters beyond hygiene: this module's own tests exercise
/// [`open_at`] directly against scratch files and never touch this path, but
/// every *other* test in this crate that drives a write through
/// `plan_and_execute_tracked` (the coordination, contract, and lifecycle
/// suites) goes through the real [`persist`]/[`db`] singleton. Without this
/// split those tests would read and write Tom's actual
/// `~/.local/state/git-vista/operations.sqlite3` — polluting it with test rows
/// and, since idempotency keys are `UNIQUE`, failing on a second run with the
/// same test names.
fn db_path() -> PathBuf {
    #[cfg(test)]
    {
        static TEST_DB_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
        TEST_DB_DIR
            .get_or_init(|| tempfile::tempdir().expect("a throwaway dir for the test journal"))
            .path()
            .join("operations.sqlite3")
    }
    #[cfg(not(test))]
    {
        crate::state::operations_db_path()
    }
}

// ---------------------------------------------------------------------------
// Persist / load
// ---------------------------------------------------------------------------

/// Write (or overwrite) one operation's row, keyed by the client's idempotency
/// key. Best-effort: a failure is logged, redacted, and swallowed — the git
/// operation this describes has already run or is running, and journal
/// trouble must never be allowed to affect it.
pub(crate) async fn persist(key: IdempotencyKey, status: OperationStatus) {
    // Redacted *before* the move into the blocking closure: on the error path
    // below, the log line names what kind of operation failed to journal
    // without the free text (a commit message, say) it might carry.
    let kind = redact_operation(&status.operation);
    let result = tokio::task::spawn_blocking(move || persist_blocking(db()?, &key, &status)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("git-vista: couldn't persist operation ({kind}) to the journal: {e}")
        }
        Err(e) => eprintln!("git-vista: the journal write task ({kind}) panicked: {e}"),
    }
}

/// Takes the connection explicitly (rather than calling [`db`] itself) so a
/// caller can supply an isolated connection instead of the process-wide
/// singleton — see [`open_private`] for why that matters.
fn persist_blocking(
    conn: &'static StdMutex<Connection>,
    key: &IdempotencyKey,
    status: &OperationStatus,
) -> Result<(), DurableError> {
    let conn = conn.lock().expect("operations db lock");
    insert_or_update(&conn, key, status)?;
    Ok(())
}

fn insert_or_update(
    conn: &Connection,
    key: &IdempotencyKey,
    status: &OperationStatus,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO operations
            (id, idempotency_key, state, stage, operation_json, operation_hash,
             repository, worktree, accepted_at, ended_at, status, message,
             generation, recovery_json, recovers_operation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id) DO UPDATE SET
            state = excluded.state,
            stage = excluded.stage,
            ended_at = excluded.ended_at,
            status = excluded.status,
            message = excluded.message,
            generation = excluded.generation,
            recovery_json = excluded.recovery_json",
        // `recovers_operation` is deliberately absent from the UPDATE SET,
        // the same way `operation_hash`/`repository`/`worktree` above it are:
        // a fact established once, at admission, on the row it describes, and
        // never touched again by that row's own terminal update. M3.25's
        // "never backfilled onto the original row" is exactly this omission,
        // made structural rather than a runtime check anyone could get wrong
        // later — see the test
        // `a_terminal_upsert_can_never_overwrite_the_recovers_link`.
        params![
            status.id.as_str(),
            key.as_str(),
            format!("{:?}", status.state).to_lowercase(),
            format!("{:?}", status.stage).to_lowercase(),
            serde_json::to_string(&status.operation).unwrap_or_default(),
            status.operation_hash.as_str(),
            status.repository.as_str(),
            status.worktree.as_str(),
            status.accepted_at.0,
            status.ended_at.map(|t| t.0),
            status.status,
            status.message,
            status.generation.as_ref().map(|g| g.as_str().to_string()),
            status
                .recovery
                .as_ref()
                .map(|r| serde_json::to_string(r).unwrap_or_default()),
            status.recovers.as_ref().map(|id| id.as_str().to_string()),
        ],
    )?;
    Ok(())
}

/// The `SELECT` column list every [`row_to_loaded`] caller must use, verbatim
/// and in this order — [`load_all_blocking`], [`load_operation_blocking`], and
/// [`select_operations_blocking`].
///
/// [`row_to_loaded`] decodes by **position** (`row.get(0)` … `row.get(14)`),
/// so a list that is reordered, shortened, or replaced with `SELECT *` in one
/// caller misaligns every field after the first difference — and misaligns it
/// *silently*, because most of these columns are `TEXT` and would decode as
/// some other column's perfectly valid string. One constant, three call sites,
/// so that cannot drift.
const STATUS_COLUMNS: &str = "id, idempotency_key, state, stage, operation_json, operation_hash,
     repository, worktree, accepted_at, ended_at, status, message,
     generation, recovery_json, recovers_operation";

/// The raw stored facts of a journal row whose payload this build cannot
/// decode (#509) — most plausibly a row written by a binary that understood an
/// operation this one does not, the way `PopStash`'s removal (#501) stranded
/// its schema-v2 rows. Carries only what the bytes themselves say: the `"op"`
/// discriminant string when the JSON envelope is at least well-formed, the
/// `state` column's verbatim spelling, and the plain columns — never a
/// guessed-at [`OperationStatus`].
///
/// This type existing is the fix's core: before it, "cannot decode" collapsed
/// into "does not exist" at [`row_to_loaded`]'s seam, and every caller
/// downstream — single lookup, history, startup recovery, idempotency reuse —
/// inherited the lie at once.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IncompatibleRecord {
    pub id: OperationId,
    pub key: IdempotencyKey,
    /// The stored operation's `"op"` discriminant, verbatim; `None` when even
    /// the JSON envelope is unreadable. Kind only, never the payload's fields
    /// — the same boundary [`redact_operation`] draws for log lines.
    ///
    /// **A readable `op` string is not evidence of version skew.** It is
    /// lifted from the raw envelope whichever field actually failed, so it
    /// answers "which operation was this row about" and nothing else. What
    /// this build may claim about *why* the row is unreadable comes from
    /// [`DecodeFailure`] alone — see [`IncompatibleRecord::blames_version_skew`].
    pub op_kind: Option<String>,
    /// Which part of the payload this build could not read. The whole reason
    /// it is carried: five of the six ways a row fails to decode say nothing
    /// about the operation being unknown, and a message that blames another
    /// build for a corrupt token or an unrecognised `stage` spelling is a
    /// false sentence — one that [`close_out_incompatible_blocking`] would
    /// then write permanently into the `message` column.
    pub failure: DecodeFailure,
    /// The `state` column verbatim, including spellings this build's
    /// [`parse_state`] doesn't know.
    pub state_raw: String,
    pub repository_raw: String,
    pub worktree_raw: String,
    pub accepted_at: UnixSeconds,
    pub ended_at: Option<UnixSeconds>,
    pub status: Option<u16>,
    pub message: Option<String>,
}

/// Why a stored row would not decode.
///
/// #509 shipped without this distinction and every refusal, note and persisted
/// close-out message keyed its "written by a build that understood an
/// operation this build does not" sentence on `op_kind.is_some()`. But the
/// `op` string is readable whenever the JSON envelope is well-formed, which it
/// is for a row that failed on a validating newtype or an unrecognised `state`
/// spelling — operations this build understands perfectly. Skew is a claim
/// about another binary, so only the one arm that can actually observe it may
/// make it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeFailure {
    /// `serde_json` could not turn the payload into a [`GitOperation`]. This
    /// is the shape #509 exists for — most plausibly a variant a later build
    /// wrote and this one has removed or never had, the way `PopStash`'s
    /// removal (#501) stranded its schema-v2 rows.
    UnknownOperation,
    /// Some other stored field did not survive the trip: a closed-set parser
    /// (`state`, `stage`) met a spelling it does not know, or a validating
    /// newtype rejected its column. Names the field so the operator is told
    /// where to look; says nothing about which build wrote the row, because
    /// nothing here can tell.
    UnreadableField(&'static str),
}

impl IncompatibleRecord {
    /// Terminal by the `state` column's raw spelling — the only fact available
    /// when the payload doesn't decode. An unknown spelling reads as
    /// non-terminal on purpose: a row this build can't even classify gets
    /// closed out by [`recover_blocking`] rather than left running forever.
    pub(crate) fn is_terminal_raw(&self) -> bool {
        matches!(self.state_raw.as_str(), "succeeded" | "failed")
    }

    /// What any message about this row may honestly claim.
    ///
    /// Every sentence this build writes about an incompatible record — the
    /// history note, the recover refusal, the idempotency-key refusal, and the
    /// one persisted into the `message` column — is built from this and never
    /// from `op_kind` directly. That is the whole guard: `op_kind` is readable
    /// whenever the JSON envelope is well-formed, so keying a version-skew
    /// claim on it attributes a corrupt token to another build.
    pub(crate) fn blame(&self) -> Blame {
        match (self.failure, self.op_kind.as_deref()) {
            (DecodeFailure::UnknownOperation, Some(kind)) => {
                Blame::UnknownOperation(kind.to_string())
            }
            (DecodeFailure::UnknownOperation, None) => Blame::Undecodable,
            (DecodeFailure::UnreadableField(field), _) => Blame::UnreadableField(field),
        }
    }
}

/// What may be said about a row that would not decode — the honest claim,
/// computed once by [`IncompatibleRecord::blame`] and carried wherever a
/// message needs it (including into the idempotency-key registry, which
/// outlives the record itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Blame {
    /// The operation itself is unknown to this build and its discriminant is
    /// readable. The one case that is evidence of version skew, and the only
    /// one allowed to say so.
    UnknownOperation(String),
    /// A stored field would not read. Names the field; blames no build,
    /// because nothing here can tell which one wrote the row.
    UnreadableField(&'static str),
    /// The payload would not decode and its envelope carries no readable
    /// `op` either — nothing can be named at all.
    Undecodable,
}

/// One journal row, as much of it as this build can honestly claim.
pub(crate) enum LoadedRow {
    // Boxed for the same reason `PlanSource::Submit` boxes its `Plan`:
    // `OperationStatus` dwarfs the other variant, and every scan pays the
    // enum's stack size.
    Decoded(IdempotencyKey, Box<OperationStatus>),
    Incompatible(IncompatibleRecord),
}

/// Every row in the journal, split into fully-decoded `(key, status)` records
/// and [`IncompatibleRecord`]s whose payload this build cannot read. Only a
/// row whose `id`/`key` columns themselves no longer validate — out-of-band
/// tampering, since the server minted and checked both on write — is logged
/// and skipped: one bad row must not make every other recorded operation
/// unrecoverable, and an incompatible row is not a bad row, it is a fact.
#[allow(clippy::type_complexity)]
fn load_journal_blocking(
    conn: &Connection,
) -> rusqlite::Result<(
    Vec<(IdempotencyKey, OperationStatus)>,
    Vec<IncompatibleRecord>,
)> {
    let mut stmt = conn.prepare(&format!("SELECT {STATUS_COLUMNS} FROM operations"))?;
    let rows = stmt.query_map([], row_to_loaded)?;
    let mut decoded = Vec::new();
    let mut incompatible = Vec::new();
    for row in rows {
        match row {
            Ok(Some(LoadedRow::Decoded(key, status))) => decoded.push((key, *status)),
            Ok(Some(LoadedRow::Incompatible(record))) => incompatible.push(record),
            Ok(None) => {} // id/key unreadable; already logged in row_to_loaded
            Err(e) => eprintln!("git-vista: couldn't read a journal row: {e}"),
        }
    }
    Ok((decoded, incompatible))
}

/// [`load_journal_blocking`]'s decoded half, for the suites written before
/// #509 taught the load path to keep incompatible rows.
#[cfg(test)]
fn load_all_blocking(
    conn: &Connection,
) -> rusqlite::Result<Vec<(IdempotencyKey, OperationStatus)>> {
    Ok(load_journal_blocking(conn)?.0)
}

fn row_to_loaded(row: &rusqlite::Row) -> rusqlite::Result<Option<LoadedRow>> {
    let id: String = row.get(0)?;
    let key: String = row.get(1)?;
    let state: String = row.get(2)?;
    let stage: String = row.get(3)?;
    let operation_json: String = row.get(4)?;
    let operation_hash: String = row.get(5)?;
    let repository: String = row.get(6)?;
    let worktree: String = row.get(7)?;
    let accepted_at: i64 = row.get(8)?;
    let ended_at: Option<i64> = row.get(9)?;
    let status: Option<u16> = row.get(10)?;
    let message: Option<String> = row.get(11)?;
    let generation: Option<String> = row.get(12)?;
    let recovery_json: Option<String> = row.get(13)?;
    let recovers_operation: Option<String> = row.get(14)?;

    // The two columns every outcome below needs a name from. The server
    // minted and validated both on write, so a failure here is out-of-band
    // tampering rather than version skew — the one shape nothing can honestly
    // represent, logged and dropped.
    let (Ok(id), Ok(key)) = (OperationId::new(id), IdempotencyKey::new(key)) else {
        eprintln!("git-vista: a journal row's id or key column no longer validates; skipped");
        return Ok(None);
    };

    // Each field names itself on failure. The `ok_or` noise is the point:
    // collapsing these into one `?`-chain is what let a corrupt token be
    // reported as another build's unknown operation (#509 follow-up).
    let decoded = (|| {
        Ok::<OperationStatus, DecodeFailure>(OperationStatus {
            id: id.clone(),
            state: parse_state(&state).ok_or(DecodeFailure::UnreadableField("state"))?,
            stage: parse_stage(&stage).ok_or(DecodeFailure::UnreadableField("stage"))?,
            operation: serde_json::from_str::<GitOperation>(&operation_json)
                .map_err(|_| DecodeFailure::UnknownOperation)?,
            operation_hash: OperationHash::new(operation_hash)
                .map_err(|_| DecodeFailure::UnreadableField("operation_hash"))?,
            repository: RepositoryToken::new(repository.clone())
                .map_err(|_| DecodeFailure::UnreadableField("repository"))?,
            worktree: WorktreeToken::new(worktree.clone())
                .map_err(|_| DecodeFailure::UnreadableField("worktree"))?,
            accepted_at: UnixSeconds(accepted_at),
            ended_at: ended_at.map(UnixSeconds),
            status,
            message: message.clone(),
            generation: generation.and_then(|g| GenerationToken::new(g).ok()),
            recovery: recovery_json.and_then(|r| serde_json::from_str::<RecoveryStrategy>(&r).ok()),
            // M3.25 (#78): `None` for every row written before the column
            // existed, and for every operation that recovers nothing —
            // which is nearly all of them.
            recovers: recovers_operation.and_then(|id| OperationId::new(id).ok()),
            // M2.20c (#229): transfer progress is deliberately **not** a
            // column and is never rehydrated. It describes a transfer in
            // flight, and this table only ever hands back records this
            // process did not run: every row `recover` returns is
            // terminal (it force-fails anything a prior process left
            // running), so a persisted "receiving 62%" would be a
            // progress report about a process that no longer exists.
            progress: None,
        })
    })();

    Ok(Some(match decoded {
        Ok(status) => LoadedRow::Decoded(key, Box::new(status)),
        Err(failure) => {
            // #509: a payload this build can't decode is an incompatible
            // record, never a vanished one. Kind only in the log — the raw
            // JSON can carry free text a user typed.
            let op_kind = serde_json::from_str::<serde_json::Value>(&operation_json)
                .ok()
                .and_then(|v| v.get("op").and_then(|t| t.as_str()).map(str::to_string));
            // The log says which field failed for the same reason the stored
            // message does: "can't decode (op: commit_on_head)" sent an
            // operator hunting version skew when the real cause was a `stage`
            // spelling this build does not know.
            match failure {
                DecodeFailure::UnknownOperation => eprintln!(
                    "git-vista: journal row {} holds an operation this build does not know (op: {}); kept as incompatible",
                    id.as_str(),
                    op_kind.as_deref().unwrap_or("<unreadable>")
                ),
                DecodeFailure::UnreadableField(field) => eprintln!(
                    "git-vista: journal row {} has an unreadable `{}` column (op: {}); kept as incompatible",
                    id.as_str(),
                    field,
                    op_kind.as_deref().unwrap_or("<unreadable>")
                ),
            }
            LoadedRow::Incompatible(IncompatibleRecord {
                id,
                key,
                op_kind,
                failure,
                state_raw: state,
                repository_raw: repository,
                worktree_raw: worktree,
                accepted_at: UnixSeconds(accepted_at),
                ended_at: ended_at.map(UnixSeconds),
                status,
                message,
            })
        }
    }))
}

fn parse_state(s: &str) -> Option<OperationState> {
    match s {
        "accepted" => Some(OperationState::Accepted),
        "running" => Some(OperationState::Running),
        "succeeded" => Some(OperationState::Succeeded),
        "failed" => Some(OperationState::Failed),
        _ => None,
    }
}

fn parse_stage(s: &str) -> Option<OperationStage> {
    match s {
        "queued" => Some(OperationStage::Queued),
        "planning" => Some(OperationStage::Planning),
        "waiting" => Some(OperationStage::Waiting),
        "checking" => Some(OperationStage::Checking),
        "executing" => Some(OperationStage::Executing),
        "finished" => Some(OperationStage::Finished),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The Recovery Center's read path (M3.25, #78)
// ---------------------------------------------------------------------------

/// One page of this repository's terminal operations, newest first, keyset-
/// paginated on `(accepted_at, id)` — the query behind
/// `GET /api/operations/history`.
///
/// **Not best-effort**, unlike [`persist`]/[`recover`]. Those swallow a
/// failure because the git operation they describe already ran regardless of
/// the journal's health; a history *read* has nothing else to fall back on —
/// the read is the whole of what was asked. Silently returning an empty page
/// on a real database error would read as "nothing happened" to a caller who
/// cannot tell that from "we couldn't check", which is the same confusion
/// `recovery_center::RecoveryClass::CheckFailed` exists to keep out of the
/// classification path.
///
/// `states` is always a `&'static` slice built from a closed, compile-time
/// enum (`recovery_center::HistoryStateFilter::terminal_states`), never
/// request-derived text, and each element is rendered by [`state_literal`]'s
/// exhaustive match into a fixed literal — so the `state IN (...)` fragment
/// this composes contains nothing a request could influence. rusqlite has no
/// placeholder for a variable-length `IN` list; this is why the fragment is
/// composed rather than bound.
///
/// `before`, when present, names the last row of a previous page —
/// `(accepted_at, id)`, both values this endpoint itself already returned.
/// **Pagination cannot key on `id` alone**: `crate::operations::mint_id`
/// mints 128 bits from the OS CSPRNG, so ids carry no ordering. `id` here
/// only ever breaks a tie among rows sharing one `accepted_at` — and
/// `UnixSeconds` has one-second resolution, so ties are ordinary, not an edge
/// case.
///
/// Returns **every row the query scanned**, as [`ScannedOperation`]s, not just
/// the ones whose payload decoded — see that type for why the caller's
/// pagination must be able to tell "the scan ended" from "a row was dropped".
pub(crate) async fn list_operations(
    repository: RepositoryToken,
    states: &'static [OperationState],
    before: Option<(UnixSeconds, OperationId)>,
    limit: u32,
) -> Result<Vec<ScannedOperation>, DurableError> {
    let result = tokio::task::spawn_blocking(move || {
        let conn = db()?;
        let conn = conn.lock().expect("operations db lock");
        select_operations_blocking(&conn, &repository, states, before.as_ref(), limit)
            .map_err(DurableError::from)
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(e) => Err(DurableError::TaskPanicked(e.to_string())),
    }
}

/// The `state` column's stored spelling, as a fixed literal — the exact
/// inverse of [`parse_state`], and the reason [`select_operations_blocking`]'s
/// `IN` fragment can be composed without ever embedding request data. An
/// exhaustive match, so a new [`OperationState`] variant is a compile error
/// here rather than a silently-unmatched row.
fn state_literal(state: OperationState) -> &'static str {
    match state {
        OperationState::Accepted => "'accepted'",
        OperationState::Running => "'running'",
        OperationState::Succeeded => "'succeeded'",
        OperationState::Failed => "'failed'",
    }
}

/// One row the history query scanned: the keyset key `(accepted_at, id)` the
/// row occupies in the walk, plus the decoded record when the row's payload
/// columns decoded too.
///
/// The key and the payload deliberately do not share a fate. The key columns
/// are plain `INTEGER`/`TEXT` SELECT columns the server itself wrote, and they
/// decode even when a fragile payload field (`operation_json`, a token, a
/// state spelling) does not. Handing the caller only the decoded survivors —
/// as this query once did — let one bad row shrink a `limit + 1` lookahead to
/// `limit` rows, which the pager then read as "last page": a single
/// undecodable row silently hid **every older row** behind it. That breaks
/// this module's own promise that one bad row must not make other operations
/// unrecoverable. With the key carried separately, the pager counts and
/// cursors on what was *scanned*, and a bad row costs exactly itself.
#[derive(Debug)]
pub(crate) struct ScannedOperation {
    pub accepted_at: UnixSeconds,
    pub id: OperationId,
    pub payload: ScannedPayload,
}

/// What a scanned row's payload columns yielded. Three outcomes, because two
/// different failures deserve two different faces (#509): an incompatible row
/// still has raw facts worth showing, an unreadable one has nothing but its
/// keyset key.
#[derive(Debug)]
pub(crate) enum ScannedPayload {
    Decoded(Box<OperationStatus>),
    /// The payload didn't decode but the row's stored facts did — surfaced to
    /// the Recovery Center as what it is, never silently dropped.
    ///
    /// Boxed for the same reason `Decoded` is: it grew a [`DecodeFailure`] and
    /// is now the widest variant, and every scan pays the enum's stack size.
    Incompatible(Box<IncompatibleRecord>),
    /// A payload column the SQL driver itself couldn't read, or a key column
    /// that no longer validates — logged, dropped from the page, but still
    /// counted and still able to carry the cursor past itself.
    Unreadable,
}

/// [`row_to_loaded`], with the keyset key read first and kept even when the
/// payload fails to decode.
///
/// `Ok(None)` — the row vanishing from the scan entirely — is reserved for a
/// key that is itself unreadable: an `id` column that no longer satisfies
/// [`OperationId`]'s token rule. The server minted and validated every id it
/// wrote and `id` is the table's PRIMARY KEY, so that takes out-of-band
/// database tampering, not a bad payload; such a row can be neither returned
/// nor named in a cursor, and dropping it is the only honest option left.
/// (A rusqlite type error on the key columns surfaces as `Err` from
/// `query_map` and is likewise logged and dropped by the caller.)
fn row_to_scanned(row: &rusqlite::Row) -> rusqlite::Result<Option<ScannedOperation>> {
    let raw_id: String = row.get(0)?;
    let accepted_at: i64 = row.get(8)?;
    let Ok(id) = OperationId::new(raw_id) else {
        eprintln!("git-vista: a journal row's id column isn't an operation id; skipped");
        return Ok(None);
    };
    let payload = match row_to_loaded(row) {
        Ok(Some(LoadedRow::Decoded(_key, status))) => ScannedPayload::Decoded(status),
        Ok(Some(LoadedRow::Incompatible(record))) => ScannedPayload::Incompatible(Box::new(record)),
        Ok(None) => ScannedPayload::Unreadable, // key column unreadable; logged in row_to_loaded
        Err(e) => {
            // A payload column the SQL driver itself couldn't read (a type
            // mismatch, say). Same outcome as an unreadable key: the record is
            // gone, the keyset key is not.
            eprintln!("git-vista: couldn't read a history row's payload: {e}");
            ScannedPayload::Unreadable
        }
    };
    Ok(Some(ScannedOperation {
        accepted_at: UnixSeconds(accepted_at),
        id,
        payload,
    }))
}

fn select_operations_blocking(
    conn: &Connection,
    repository: &RepositoryToken,
    states: &[OperationState],
    before: Option<&(UnixSeconds, OperationId)>,
    limit: u32,
) -> rusqlite::Result<Vec<ScannedOperation>> {
    let state_list = states
        .iter()
        .copied()
        .map(state_literal)
        .collect::<Vec<_>>()
        .join(",");
    // `ended_at IS NOT NULL` is belt-and-braces beside the terminal-state
    // filter, and it is what lets `recovery_center::HistoryEntry` carry a
    // plain `UnixSeconds` rather than an `Option`: a row this query returns
    // has an end time, enforced by the query rather than assumed from the
    // state.
    let sql = format!(
        "SELECT {STATUS_COLUMNS}
         FROM operations
         WHERE repository = ?1
           AND state IN ({state_list})
           AND ended_at IS NOT NULL
           AND (?2 IS NULL OR accepted_at < ?2 OR (accepted_at = ?2 AND id < ?3))
         ORDER BY accepted_at DESC, id DESC
         LIMIT ?4"
    );
    let (before_secs, before_id): (Option<i64>, Option<&str>) = match before {
        Some((t, id)) => (Some(t.0), Some(id.as_str())),
        None => (None, None),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![repository.as_str(), before_secs, before_id, limit as i64],
        row_to_scanned,
    )?;
    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok(Some(entry)) => out.push(entry),
            Ok(None) => {} // the key itself was unreadable; logged in row_to_scanned
            Err(e) => eprintln!("git-vista: couldn't read a history row: {e}"),
        }
    }
    Ok(out)
}

/// Load one operation's row by its server-minted id, straight from the
/// journal — **not** `crate::operations::lookup`, the in-memory registry,
/// whose bounded size and TTL are exactly why the Recovery Center reads this
/// table in the first place: a browsable history must outlive both the
/// eviction and the process.
///
/// "Cannot decode" and "does not exist" answered apart (#509). This used to be
/// an `Option` whose `None` covered both, which dressed a row from an
/// incompatible build as a 404 — the caller could neither explain the record
/// nor stop treating its id as never-minted.
///
/// A journal that can't be opened at all still reads as [`Missing`]
/// (best-effort, like every read here except [`list_operations`]) — logged,
/// never invented into a record.
///
/// [`Missing`]: DurableLookup::Missing
pub(crate) enum DurableLookup {
    Found(Box<OperationStatus>),
    Incompatible(Box<IncompatibleRecord>),
    Missing,
}

/// `Missing` still covers "no such id ever existed" and the tampering shape
/// where the row's own id/key columns no longer validate; a payload this build
/// cannot decode is `Incompatible`, never `Missing`.
pub(crate) async fn load_operation(id: &OperationId) -> DurableLookup {
    let id = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let Ok(conn) = db() else {
            return DurableLookup::Missing;
        };
        let conn = conn.lock().expect("operations db lock");
        load_operation_blocking(&conn, &id)
    })
    .await;
    match result {
        Ok(found) => found,
        Err(e) => {
            eprintln!("git-vista: the journal read task panicked: {e}");
            DurableLookup::Missing
        }
    }
}

fn load_operation_blocking(conn: &Connection, id: &OperationId) -> DurableLookup {
    let loaded = conn.query_row(
        &format!("SELECT {STATUS_COLUMNS} FROM operations WHERE id = ?1"),
        params![id.as_str()],
        row_to_loaded,
    );
    match loaded {
        Ok(Some(LoadedRow::Decoded(_key, status))) => DurableLookup::Found(status),
        Ok(Some(LoadedRow::Incompatible(record))) => DurableLookup::Incompatible(Box::new(record)),
        Ok(None) | Err(rusqlite::Error::QueryReturnedNoRows) => DurableLookup::Missing,
        Err(e) => {
            eprintln!("git-vista: couldn't read the journal row for an operation id: {e}");
            DurableLookup::Missing
        }
    }
}

// ---------------------------------------------------------------------------
// Startup recovery
// ---------------------------------------------------------------------------

/// Load every journaled operation, close out anything a prior process left
/// non-terminal (see the module docs for why that's the correct answer, not a
/// guess), and return the full set for [`crate::operations::rehydrate`].
///
/// Best-effort like everything else here: a journal that can't be opened
/// (corrupt file, unknown schema version) is logged and the server starts with
/// an empty operation history rather than refusing to start at all — the
/// journal is a recovery aid, not a prerequisite for serving repositories.
///
/// **This is a startup-only operation.** It has no way to distinguish "a row
/// left non-terminal by a process that crashed" from "a row that is
/// non-terminal because the operation is still genuinely running right now" —
/// that distinction only holds if nothing is running yet, which is true at
/// process start (see `main.rs`, the sole production caller) and is *not*
/// true if this is called against a connection anything else might be
/// concurrently writing to. Do not call this against the shared journal from
/// a test or any other code path where operations may be in flight — use
/// [`open_private`] plus [`recover_from`] for that instead of this function;
/// this was the actual root cause of issue #158.
pub(crate) async fn recover() -> RecoveredJournal {
    let loaded = tokio::task::spawn_blocking(|| recover_blocking(db()?)).await;
    match loaded {
        Ok(Ok(journal)) => journal,
        Ok(Err(e)) => {
            eprintln!("git-vista: couldn't open the operation journal: {e}");
            RecoveredJournal::default()
        }
        Err(e) => {
            eprintln!("git-vista: the journal recovery task panicked: {e}");
            RecoveredJournal::default()
        }
    }
}

/// What startup recovery hands to `main.rs`: the decoded records for
/// [`crate::operations::rehydrate`]'s registry, plus the incompatible rows
/// (#509) whose idempotency keys must be guarded against reuse-as-fresh —
/// they still hold their `UNIQUE` key in SQLite, and a registry that has
/// never heard of them would admit that key as a brand-new operation.
#[derive(Default)]
pub(crate) struct RecoveredJournal {
    pub records: Vec<(IdempotencyKey, OperationStatus)>,
    pub incompatible: Vec<IncompatibleRecord>,
}

/// The terminal message the startup sweep writes onto an incompatible row
/// (#509). Names the stored op kind when the bytes carry one, and claims
/// nothing when they don't.
fn incompatible_close_out_message(record: &IncompatibleRecord) -> String {
    // "can never be resumed" was the original wording and it was an absolute
    // this code cannot support: the row is unresumable BY THIS BUILD, which is
    // precisely why the payload is left intact for one that can read it. The
    // sentence is persisted, so a later build reads it as a claim about
    // itself.
    let closed = "This build closed it out as failed rather than leave it \
                  running forever; its stored payload was left untouched.";
    match record.blame() {
        Blame::UnknownOperation(kind) => format!(
            "This operation ('{kind}') was written by a Git-Vista build that \
             understood an operation this build does not, so this build cannot \
             resume it. {closed}"
        ),
        Blame::UnreadableField(field) => format!(
            "This build could not read this record's `{field}`, so it cannot \
             resume the operation. This says nothing about which build wrote \
             the row. {closed}"
        ),
        Blame::Undecodable => {
            format!("This build could not decode this record's stored operation. {closed}")
        }
    }
}

/// Takes the connection explicitly (rather than calling [`db`] itself) so a
/// caller can supply an isolated connection instead of the process-wide
/// singleton — see [`open_private`].
fn recover_blocking(conn: &'static StdMutex<Connection>) -> Result<RecoveredJournal, DurableError> {
    let conn = conn.lock().expect("operations db lock");
    let (mut records, mut incompatible) = load_journal_blocking(&conn)?;
    let now = crate::activity::now_secs();

    for (key, record) in records.iter_mut().filter(|(_, r)| !r.is_terminal()) {
        record.state = OperationState::Failed;
        record.stage = OperationStage::Finished;
        record.status = Some(500);
        record.message = Some(
            "The server restarted before this operation finished. Check the \
             repository before retrying."
                .to_string(),
        );
        record.ended_at = Some(UnixSeconds(now));
        insert_or_update(&conn, key, record)?;
    }

    // #509: the same close-out, for rows whose payload this build can't
    // decode. `insert_or_update` needs an `OperationStatus` this row cannot
    // produce, so the update targets the columns directly.
    //
    // What survives and what does not, stated exactly — the first version of
    // this comment claimed "only the lifecycle columns, never the payload,
    // which stays byte-for-byte" and that is only half true. The
    // `operation_json` payload IS preserved byte-for-byte, deliberately, so a
    // build that understands the operation can still read it. But the
    // lifecycle record is OVERWRITTEN: `state`, `stage`, `status`, `message`
    // and `ended_at` all go, including whatever the original `message` held.
    // A returning build therefore cannot distinguish a genuine failure from
    // this close-out, and cannot recover the state it might have reconciled
    // against git. That is the accepted cost of not leaving a row 'running'
    // forever, and the `closed-out-by-incompatible-build:` prefix below is
    // what lets a later reader tell the two apart at all.
    for record in incompatible.iter_mut().filter(|r| !r.is_terminal_raw()) {
        let message = format!(
            "closed-out-by-incompatible-build: {}",
            incompatible_close_out_message(record)
        );
        conn.execute(
            "UPDATE operations
             SET state = 'failed', stage = 'finished', status = 500,
                 message = ?1, ended_at = ?2
             WHERE id = ?3",
            params![message, now, record.id.as_str()],
        )?;
        record.state_raw = "failed".to_string();
        record.status = Some(500);
        record.message = Some(message);
        record.ended_at = Some(UnixSeconds(now));
    }
    Ok(RecoveredJournal {
        records,
        incompatible,
    })
}

// ---------------------------------------------------------------------------
// Recovery refs
// ---------------------------------------------------------------------------

/// Pin the pre-operation tip a [`RecoveryStrategy`] names as
/// `refs/git-vista/recovery/<operation id>` in `repo`, if the strategy carries
/// a concrete commit to pin. `NotNeeded`, `DeleteCreatedBranch`,
/// `CheckoutPrevious`, and `Irrecoverable` name no oid — there is nothing here
/// for those to write.
///
/// Best-effort — a failure is logged, never turned into a refusal — but **not**
/// off the critical path: the planner calls this from inside the per-repository
/// mutation guard, immediately before `execute`, precisely so the pin exists
/// before the command that makes it necessary runs. See `planner::pin_recovery`
/// for why the earlier arrangement (write it in the tracked wrapper, after the
/// pipeline returned) was a real gc race and not merely a late bonus.
///
/// "Best-effort" here therefore means *this write may fail*, not *this write may
/// be late*. For `RecreateTag` the ref is the only thing keeping the deleted
/// annotated tag's object reachable; the JSON `recovery` field in the row names
/// an oid, and an oid `git gc` has already pruned is not something a human or an
/// `update-ref` can restore. The field is a *record* of the decision, not a
/// second copy of the object.
pub(crate) async fn write_recovery_ref(
    repo: &Path,
    operation: &OperationId,
    recovery: &RecoveryStrategy,
) {
    let Some(oid) = recovery_oid(recovery) else {
        return;
    };
    let ref_name = recovery_ref_name(operation);
    let result = crate::git_cmd::git_output(repo, &["update-ref", &ref_name, oid.as_str()]).await;
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => eprintln!(
            "git-vista: couldn't write recovery ref {ref_name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => eprintln!("git-vista: couldn't run git to write recovery ref {ref_name}: {e}"),
    }
}

/// The object a recovery strategy would restore, if it names one directly.
fn recovery_oid(recovery: &RecoveryStrategy) -> Option<&CommitOid> {
    match recovery {
        // M4.31 (#84): names no object to pin. The discarded side is kept
        // reachable by MERGE_HEAD for as long as the operation runs, and once
        // that ends there is nothing a recovery ref could have held open —
        // pinning a blob would not make the conflict rebuildable, only the
        // bytes retrievable, which is a different and weaker promise than the
        // strategy makes.
        RecoveryStrategy::ConflictRecreatableWhileInProgress => None,
        RecoveryStrategy::ResetRef { to, .. } => Some(to),
        RecoveryStrategy::RecreateBranch { at, .. } => Some(at),
        // M2.21a (#235): for a deleted *annotated* tag `at` is the tag
        // object, not a commit — and pinning it here is load-bearing, not
        // incidental: the recovery ref keeps that now-dangling tag object
        // *reachable*, so git gc cannot prune the only exact copy of the
        // tag (message, tagger, signature) while the pin exists. See
        // `RecreateTag`'s doc in plan.rs — this pin is half the reason the
        // variant carries an oid instead of a message.
        RecoveryStrategy::RecreateTag { at, .. } => Some(at),
        // M3.24 (#77): the same pin, for the same reason. A dropped stash's
        // commit is dangling the moment the entry goes, alive only until
        // gc.reflogExpireUnreachable. The recovery ref keeps it reachable, so
        // the undo stays possible past that window — and it is why the variant
        // carries an oid rather than only a message.
        RecoveryStrategy::RecreateStashEntry { at, .. } => Some(at),
        RecoveryStrategy::RevertCommit { commit } => Some(commit),
        RecoveryStrategy::NotNeeded
        | RecoveryStrategy::DeleteCreatedBranch { .. }
        | RecoveryStrategy::DeleteCreatedTag { .. }
        | RecoveryStrategy::CheckoutPrevious { .. }
        // A dangling blob (if any) isn't a commit a ref can point at — this
        // function only ever writes a ref naming a commit.
        | RecoveryStrategy::RecoverableIfStaged
        | RecoveryStrategy::Irrecoverable => None,
    }
}

/// Read back the ref [`write_recovery_ref`] would have written for
/// `operation`, for tests and any future recovery-browsing endpoint.
#[cfg(test)]
async fn read_recovery_ref(repo: &Path, operation: &OperationId) -> Option<String> {
    let ref_name = recovery_ref_name(operation);
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", &ref_name])
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

/// The operation's kind only (`commit_on_head`, `push_branch`, …) — never its
/// fields, which can carry a commit message or branch name the user typed.
/// Every log line in this module and [`crate::operations`] that would
/// otherwise print an operation goes through this first.
pub(crate) fn redact_operation(op: &GitOperation) -> String {
    serde_json::to_value(op)
        .ok()
        .and_then(|v| v.get("op").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| "<operation>".to_string())
}

#[cfg(test)]
mod journal_suite;

#[cfg(test)]
mod recovery_ref_suite;

#[cfg(test)]
mod redaction_suite;
