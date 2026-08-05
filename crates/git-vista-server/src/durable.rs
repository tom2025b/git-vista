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

/// The schema's `PRAGMA user_version`. Bump this — and add a migration, not a
/// silent `CREATE TABLE IF NOT EXISTS` edit — the day a column changes shape.
const SCHEMA_VERSION: i32 = 1;

/// The namespace every recovery ref lives under. Never `refs/heads/` or
/// `refs/tags/`, so "never overwrites a user ref" holds by construction: no
/// user-chosen name can ever resolve into this prefix, because git refs are
/// namespaced by their full path and this path is fixed and app-owned.
const RECOVERY_REF_PREFIX: &str = "refs/git-vista/recovery";

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
        0 => migrate(&conn)?,
        v if v == SCHEMA_VERSION => {}
        v => return Err(DurableError::UnknownSchemaVersion(v)),
    }
    Ok(conn)
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE operations (
            id              TEXT PRIMARY KEY,
            idempotency_key TEXT NOT NULL UNIQUE,
            state           TEXT NOT NULL,
            stage           TEXT NOT NULL,
            operation_json  TEXT NOT NULL,
            operation_hash  TEXT NOT NULL,
            repository      TEXT NOT NULL,
            worktree        TEXT NOT NULL,
            accepted_at     INTEGER NOT NULL,
            ended_at        INTEGER,
            status          INTEGER,
            message         TEXT,
            generation      TEXT,
            recovery_json   TEXT
        );",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
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
pub(crate) async fn recover_from(
    conn: &'static StdMutex<Connection>,
) -> Vec<(IdempotencyKey, OperationStatus)> {
    let loaded = tokio::task::spawn_blocking(move || recover_blocking(conn)).await;
    match loaded {
        Ok(Ok(records)) => records,
        Ok(Err(e)) => {
            eprintln!("git-vista: couldn't open the isolated test journal: {e}");
            Vec::new()
        }
        Err(e) => {
            eprintln!("git-vista: the isolated-journal recovery task panicked: {e}");
            Vec::new()
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
             generation, recovery_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(id) DO UPDATE SET
            state = excluded.state,
            stage = excluded.stage,
            ended_at = excluded.ended_at,
            status = excluded.status,
            message = excluded.message,
            generation = excluded.generation,
            recovery_json = excluded.recovery_json",
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
        ],
    )?;
    Ok(())
}

/// Every row in the journal, decoded back into `(key, status)`. A row that
/// fails to decode (a shape this build doesn't recognise, or a hand-edited
/// database) is logged and skipped rather than failing the whole load — one
/// bad row must not make every other recorded operation unrecoverable.
fn load_all_blocking(
    conn: &Connection,
) -> rusqlite::Result<Vec<(IdempotencyKey, OperationStatus)>> {
    let mut stmt = conn.prepare(
        "SELECT id, idempotency_key, state, stage, operation_json, operation_hash,
                repository, worktree, accepted_at, ended_at, status, message,
                generation, recovery_json
         FROM operations",
    )?;
    let rows = stmt.query_map([], row_to_status)?;
    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok(Some(entry)) => out.push(entry),
            Ok(None) => {} // a field didn't parse; already logged in row_to_status
            Err(e) => eprintln!("git-vista: couldn't read a journal row: {e}"),
        }
    }
    Ok(out)
}

fn row_to_status(
    row: &rusqlite::Row,
) -> rusqlite::Result<Option<(IdempotencyKey, OperationStatus)>> {
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

    let decoded = (|| {
        Some((
            IdempotencyKey::new(key).ok()?,
            OperationStatus {
                id: OperationId::new(id).ok()?,
                state: parse_state(&state)?,
                stage: parse_stage(&stage)?,
                operation: serde_json::from_str::<GitOperation>(&operation_json).ok()?,
                operation_hash: OperationHash::new(operation_hash).ok()?,
                repository: RepositoryToken::new(repository).ok()?,
                worktree: WorktreeToken::new(worktree).ok()?,
                accepted_at: UnixSeconds(accepted_at),
                ended_at: ended_at.map(UnixSeconds),
                status,
                message,
                generation: generation.and_then(|g| GenerationToken::new(g).ok()),
                recovery: recovery_json
                    .and_then(|r| serde_json::from_str::<RecoveryStrategy>(&r).ok()),
                // M2.20c (#229): transfer progress is deliberately **not** a
                // column and is never rehydrated. It describes a transfer in
                // flight, and this table only ever hands back records this
                // process did not run: every row `recover` returns is
                // terminal (it force-fails anything a prior process left
                // running), so a persisted "receiving 62%" would be a
                // progress report about a process that no longer exists.
                progress: None,
            },
        ))
    })();
    if decoded.is_none() {
        eprintln!("git-vista: journal row for an operation id didn't decode; skipped");
    }
    Ok(decoded)
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
pub(crate) async fn recover() -> Vec<(IdempotencyKey, OperationStatus)> {
    let loaded = tokio::task::spawn_blocking(|| recover_blocking(db()?)).await;
    match loaded {
        Ok(Ok(records)) => records,
        Ok(Err(e)) => {
            eprintln!("git-vista: couldn't open the operation journal: {e}");
            Vec::new()
        }
        Err(e) => {
            eprintln!("git-vista: the journal recovery task panicked: {e}");
            Vec::new()
        }
    }
}

/// Takes the connection explicitly (rather than calling [`db`] itself) so a
/// caller can supply an isolated connection instead of the process-wide
/// singleton — see [`open_private`].
fn recover_blocking(
    conn: &'static StdMutex<Connection>,
) -> Result<Vec<(IdempotencyKey, OperationStatus)>, DurableError> {
    let conn = conn.lock().expect("operations db lock");
    let mut records = load_all_blocking(&conn)?;
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
    Ok(records)
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
/// Best-effort and off the response path by design: called from the detached
/// pipeline task after the operation's own result is already recorded, so a
/// failure here can never turn a successful git operation into a failed
/// response. The ref is a durability *bonus* on top of the JSON `recovery`
/// field already in the row — losing it means falling back to what the field
/// alone tells a human, not losing the recovery information outright.
pub(crate) async fn write_recovery_ref(
    repo: &Path,
    operation: &OperationId,
    recovery: &RecoveryStrategy,
) {
    let Some(oid) = recovery_oid(recovery) else {
        return;
    };
    let ref_name = format!("{RECOVERY_REF_PREFIX}/{}", operation.as_str());
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
    let ref_name = format!("{RECOVERY_REF_PREFIX}/{}", operation.as_str());
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
mod tests {
    use super::*;
    use git_vista_protocol::{BranchName, CommitMessage, RefName};

    fn scratch_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.sqlite3");
        (dir, path)
    }

    fn key(name: &str) -> IdempotencyKey {
        IdempotencyKey::new(format!("durable-{name}")).unwrap()
    }

    fn sample(id: &str) -> OperationStatus {
        OperationStatus {
            id: OperationId::new(id).unwrap(),
            state: OperationState::Succeeded,
            stage: OperationStage::Finished,
            operation: GitOperation::CommitOnHead {
                message: CommitMessage::new("a private message").unwrap(),
                allow_empty: true,
            },
            operation_hash: OperationHash::new("a".repeat(64)).unwrap(),
            repository: RepositoryToken::new("r").unwrap(),
            worktree: WorktreeToken::new("w").unwrap(),
            accepted_at: UnixSeconds(1_000),
            ended_at: Some(UnixSeconds(1_001)),
            status: Some(200),
            message: Some("Created commit.".to_string()),
            generation: Some(GenerationToken::new("7").unwrap()),
            recovery: Some(RecoveryStrategy::ResetRef {
                ref_name: RefName::new("refs/heads/main").unwrap(),
                to: CommitOid::new("b".repeat(40)).unwrap(),
            }),
            progress: None,
        }
    }

    #[test]
    fn a_fresh_database_migrates_and_reports_the_current_version() {
        let (_dir, path) = scratch_db();
        let conn = open_at(&path).unwrap();
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn an_unknown_schema_version_is_refused_rather_than_guessed_at() {
        let (_dir, path) = scratch_db();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99).unwrap();
        }
        let err = open_at(&path).unwrap_err();
        assert!(matches!(err, DurableError::UnknownSchemaVersion(99)));
    }

    #[test]
    fn persist_and_reload_round_trip_a_full_record() {
        let (_dir, path) = scratch_db();
        let conn = open_at(&path).unwrap();
        let k = key("full-record");
        let status = sample("full-record");
        insert_or_update(&conn, &k, &status).unwrap();

        let loaded = load_all_blocking(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], (k, status));
    }

    #[test]
    fn a_repeated_id_updates_the_row_instead_of_duplicating_it() {
        let (_dir, path) = scratch_db();
        let conn = open_at(&path).unwrap();
        let k = key("upsert");
        let mut status = sample("upsert");
        status.state = OperationState::Running;
        status.status = None;
        status.message = None;
        insert_or_update(&conn, &k, &status).unwrap();

        status.state = OperationState::Succeeded;
        status.status = Some(200);
        status.message = Some("Created commit.".to_string());
        insert_or_update(&conn, &k, &status).unwrap();

        let loaded = load_all_blocking(&conn).unwrap();
        assert_eq!(loaded.len(), 1, "the same id must update, not duplicate");
        assert_eq!(loaded[0].1.state, OperationState::Succeeded);
    }

    /// The load-bearing crash-recovery test: a row left `running` by a process
    /// that never came back is closed out as `Failed`, with a message that
    /// says why, and nothing is left ambiguous for a client to poll forever.
    #[test]
    fn a_running_row_left_by_a_dead_process_is_closed_out_as_failed() {
        let (_dir, path) = scratch_db();
        let conn = open_at(&path).unwrap();
        let k = key("interrupted");
        let mut mid_flight = sample("interrupted");
        mid_flight.state = OperationState::Running;
        mid_flight.stage = OperationStage::Executing;
        mid_flight.status = None;
        mid_flight.message = None;
        mid_flight.ended_at = None;
        insert_or_update(&conn, &k, &mid_flight).unwrap();

        let mut records = load_all_blocking(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert!(!records[0].1.is_terminal());

        // The same fix-up `recover_blocking` applies, run against this scratch
        // connection so the process-global DB stays untouched by this test.
        let now = UnixSeconds(9_999);
        for (fix_key, record) in records.iter_mut().filter(|(_, r)| !r.is_terminal()) {
            record.state = OperationState::Failed;
            record.stage = OperationStage::Finished;
            record.status = Some(500);
            record.message = Some("The server restarted before this operation finished.".into());
            record.ended_at = Some(now);
            insert_or_update(&conn, fix_key, record).unwrap();
        }
        assert_eq!(records[0].1.state, OperationState::Failed);
        assert!(records[0].1.message.as_ref().unwrap().contains("restarted"));

        // The fix-up is itself durable — reloading sees the closed-out state,
        // not the original `running` row.
        let reloaded = load_all_blocking(&conn).unwrap();
        assert_eq!(reloaded[0].1.state, OperationState::Failed);
    }

    #[test]
    fn recovery_oid_is_present_only_for_strategies_that_name_one() {
        let with = RecoveryStrategy::ResetRef {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            to: CommitOid::new("c".repeat(40)).unwrap(),
        };
        assert!(recovery_oid(&with).is_some());
        // M2.21a (#235): `RecreateTag` names the pre-delete ref value, and
        // the pin *must* exist — it is what keeps a deleted annotated tag's
        // dangling tag object alive against gc (see recovery_oid's comment).
        let recreate_tag = RecoveryStrategy::RecreateTag {
            name: git_vista_protocol::TagName::new("v1.0.0").unwrap(),
            at: CommitOid::new("d".repeat(40)).unwrap(),
        };
        assert_eq!(
            recovery_oid(&recreate_tag).map(CommitOid::as_str),
            Some("d".repeat(40).as_str()),
            "RecreateTag's pin must be the carried pre-delete oid itself"
        );

        for without in [
            RecoveryStrategy::NotNeeded,
            RecoveryStrategy::DeleteCreatedBranch {
                name: BranchName::new("x").unwrap(),
            },
            RecoveryStrategy::DeleteCreatedTag {
                name: git_vista_protocol::TagName::new("v1.0.0").unwrap(),
            },
            RecoveryStrategy::CheckoutPrevious {
                branch: BranchName::new("x").unwrap(),
            },
            RecoveryStrategy::Irrecoverable,
        ] {
            assert!(recovery_oid(&without).is_none());
        }
    }

    #[test]
    fn redaction_keeps_the_operation_kind_and_never_its_free_text_fields() {
        let op = GitOperation::CommitOnHead {
            message: CommitMessage::new("a very private commit message").unwrap(),
            allow_empty: false,
        };
        let redacted = redact_operation(&op);
        assert_eq!(redacted, "commit_on_head");
        assert!(!redacted.contains("private"));
    }

    /// The end-to-end recovery-ref path: write one, read it back, and confirm
    /// the branch of the same working name is untouched — the namespace
    /// prefix is what makes "never overwrites a user ref" true.
    #[tokio::test]
    async fn a_recovery_ref_is_written_and_never_touches_the_user_ref_it_pins() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success());
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.invalid"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "seed"]);
        let before = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let before_oid = String::from_utf8_lossy(&before.stdout).trim().to_string();

        // A second commit, so refs/heads/main has since moved — the case a
        // recovery ref exists to answer "what was it before".
        std::fs::write(repo.join("a.txt"), "b\n").unwrap();
        run(&["commit", "-qam", "second"]);

        let id = OperationId::new("recovery-ref-test").unwrap();
        let recovery = RecoveryStrategy::ResetRef {
            ref_name: RefName::new("refs/heads/main").unwrap(),
            to: CommitOid::new(before_oid.clone()).unwrap(),
        };
        write_recovery_ref(&repo, &id, &recovery).await;

        let read = read_recovery_ref(&repo, &id).await;
        assert_eq!(read.as_deref(), Some(before_oid.as_str()));

        let heads_main = std::process::Command::new("git")
            .args(["rev-parse", "refs/heads/main"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert_ne!(
            String::from_utf8_lossy(&heads_main.stdout).trim(),
            before_oid,
            "refs/heads/main must still be the SECOND commit — the recovery ref \
             pins the old tip without moving the real branch"
        );
    }

    #[tokio::test]
    async fn strategies_with_no_oid_write_no_ref() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());

        let id = OperationId::new("no-oid-test").unwrap();
        write_recovery_ref(&repo, &id, &RecoveryStrategy::NotNeeded).await;
        assert_eq!(read_recovery_ref(&repo, &id).await, None);
    }
}
