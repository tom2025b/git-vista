//! The SQLite journal's own tests (M1.09, #62): fresh-database migration,
//! the v1→v2 in-place migration that must not lose rows, the `recovers` link
//! surviving a persist/reload round trip and never being backfilled onto an
//! already-terminal row, the Recovery Center's keyset-paginated history query
//! (M3.25, #78), and the crash-recovery sweep that closes out a `Running` row
//! left by a process that died mid-operation. Extracted verbatim from
//! `durable.rs`'s inline `mod tests` (a `#[cfg(test)]` child module) so the
//! parent file can be read as production code — see its module doc comment,
//! item 1 ("A SQLite journal"), for the subsystem these all exercise. A
//! child module of `durable`, so it still reaches `durable.rs`'s private
//! items (`open_at`, `migrate_v1_to_v2`, `insert_or_update`,
//! `select_operations_blocking`, `recover_blocking`, …) through `super::`.
//! The recovery-ref and redaction tests that shared this `mod tests` block
//! but exercise different subsystems live separately in
//! `recovery_ref_suite.rs` and `redaction_suite.rs`.

use super::*;
use git_vista_protocol::{CommitMessage, RefName};

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
        recovers: None,
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

/// A version-1 database — every `operations.sqlite3` written before M3.25
/// (#78) — must migrate in place, not be refused. A refusal here is
/// silent, total loss of the user's operation history: [`recover`] treats
/// any [`DurableError`] as best-effort-failed, logs to stderr, and returns
/// an empty vec, so the server still starts but the exact history this
/// feature exists to make browsable vanishes, with no error reachable
/// from the browser (the user has no shell).
///
/// Mutations this goes red on, each hitting a different assertion:
///  * delete `open_at`'s `1 => migrate_v1_to_v2(&conn)?` arm →
///    `open_at(&path).unwrap()` panics on `UnknownSchemaVersion(1)`;
///  * make `migrate_v1_to_v2` a no-op `Ok(())` → the version assertion,
///    and the `recovers_operation` query, both fail;
///  * drop either `CREATE INDEX` from it → the matching
///    `names.contains(...)` assertion fails;
///  * drop the `ALTER TABLE` → the `SELECT recovers_operation` errors out;
///  * remove the transaction wrapping *and* have the pragma bump fail →
///    the re-open at the end hits "duplicate column name". (That last one
///    is why the second `open_at` is here at all.)
#[test]
fn a_version_1_database_migrates_in_place_without_losing_its_rows() {
    let (_dir, path) = scratch_db();
    {
        // A deliberately frozen, hand-written copy of the pre-M3.25
        // 14-column schema — NOT a call to `migrate_fresh`, which now
        // emits the v2 shape directly and so could never exercise the
        // v1→v2 path at all. If the live schema is edited again, this
        // fixture stays as it is: it is a snapshot of what is being
        // migrated *from*, not a second copy of what is created today.
        let conn = Connection::open(&path).unwrap();
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
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        // Seeded with the v1 column list, since `insert_or_update` now
        // writes the 15-column v2 shape this table does not have yet.
        let status = sample("pre-migration");
        conn.execute(
            "INSERT INTO operations
                (id, idempotency_key, state, stage, operation_json, operation_hash,
                 repository, worktree, accepted_at, ended_at, status, message,
                 generation, recovery_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                status.id.as_str(),
                key("pre-migration").as_str(),
                "succeeded",
                "finished",
                serde_json::to_string(&status.operation).unwrap(),
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
                    .map(|r| serde_json::to_string(r).unwrap()),
            ],
        )
        .unwrap();
    }

    // Opening a v1 database must migrate it, not refuse it.
    let conn = open_at(&path).unwrap();

    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION, "the version bump must land");

    // The new column exists and is queryable — this errors outright if
    // the ALTER TABLE was dropped.
    let recovers: Option<String> = conn
        .query_row(
            "SELECT recovers_operation FROM operations WHERE id = ?1",
            params!["pre-migration"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        recovers, None,
        "a pre-existing row's new column must backfill to NULL, not a guess"
    );

    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index'")
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(names.contains(&"idx_operations_history".to_string()));
    assert!(names.contains(&"idx_operations_recovers".to_string()));
    drop(stmt);

    // The pre-migration row survived, unchanged and still decodable.
    let loaded = load_all_blocking(&conn).unwrap();
    assert_eq!(loaded.len(), 1, "migration must not drop existing rows");
    assert_eq!(loaded[0].1.id.as_str(), "pre-migration");
    assert_eq!(loaded[0].1.recovers, None);
    drop(conn);

    // Re-opening an already-migrated database must not re-run the v1 arm
    // — that would hit SQLite's "duplicate column name" and refuse the
    // journal permanently, exactly the state a non-atomic migration could
    // leave behind if it died between the ALTER and the version bump.
    let reopened = open_at(&path).unwrap();
    let version_again: i32 = reopened
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version_again, SCHEMA_VERSION);
}

/// M3.25 (#78): a row that *is* a recovery carries `recovers_operation`,
/// and it survives insert-then-reload.
///
/// Goes red if `recovers_operation` is dropped from the INSERT column
/// list, from `STATUS_COLUMNS` (the shared SELECT list), or from
/// `row_to_status`'s decode — any one of which strands the field as
/// always-`None`.
#[test]
fn a_recovery_row_persists_and_reloads_its_recovers_link() {
    let (_dir, path) = scratch_db();
    let conn = open_at(&path).unwrap();
    let k = key("recovery-row");
    let mut status = sample("recovery-row");
    status.recovers = Some(OperationId::new("earlier-op-id").unwrap());
    insert_or_update(&conn, &k, &status).unwrap();

    let loaded = load_all_blocking(&conn).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0], (k, status));
}

/// The spec's "never backfilled onto the original row", pinned as
/// structure rather than convention: `recovers_operation` is absent from
/// `insert_or_update`'s `ON CONFLICT DO UPDATE SET`, so the second
/// (terminal) write of the same row cannot change it — in either
/// direction.
///
/// Goes red the moment `recovers_operation = excluded.recovers_operation`
/// is added to that update list: the reload would then see `None`, the
/// value the second write carried.
#[test]
fn a_terminal_upsert_can_never_overwrite_the_recovers_link() {
    let (_dir, path) = scratch_db();
    let conn = open_at(&path).unwrap();
    let k = key("recovery-upsert");
    let mut admitted = sample("recovery-upsert");
    admitted.recovers = Some(OperationId::new("earlier-op-id").unwrap());
    admitted.state = OperationState::Running;
    admitted.ended_at = None;
    insert_or_update(&conn, &k, &admitted).unwrap();

    // A later write of the same row that has *lost* the link — the shape a
    // future refactor could produce by rebuilding the terminal status from
    // something other than the admitted record.
    let mut terminal = sample("recovery-upsert");
    terminal.recovers = None;
    insert_or_update(&conn, &k, &terminal).unwrap();

    let loaded = load_all_blocking(&conn).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].1.recovers,
        Some(OperationId::new("earlier-op-id").unwrap()),
        "the recovery link is written once, at admission, and no later \
         upsert of the same row may change it"
    );
    // The rest of the row still updates, so this is an exclusion of one
    // column and not a broken upsert.
    assert_eq!(loaded[0].1.state, OperationState::Succeeded);
}

/// **The pagination test a distinct-timestamp fixture cannot be.** Three
/// rows share one `accepted_at` and the page boundary falls *inside* that
/// tie — so resuming correctly requires the `id` tie-break, which `mint_id`
/// deliberately makes unordered.
///
/// The fixture shape is load-bearing and was earned: an earlier version
/// put two tied rows on page 1 and the odd one out on page 2, so the
/// boundary never landed inside the tie and the whole thing passed
/// unchanged with the tie-break deleted. It was a test that could not fail
/// on the defect it named. Mutation-checked in both directions now:
///
///  * `accepted_at < ?2` alone (tie-break dropped) — the third tied row is
///    skipped, the walk yields 3 rows, and the multiset assertion fires;
///  * `accepted_at <= ?2` alone — page 2 repeats page 1's rows forever, so
///    the walk hits the page cap and the "terminated" assertion fires.
#[test]
fn pagination_breaks_a_shared_timestamp_tie_on_the_id() {
    let (_dir, path) = scratch_db();
    let conn = open_at(&path).unwrap();

    // Three rows share `accepted_at = 500`; at limit 2 the boundary falls
    // between the second and third of them.
    for (id, at) in [
        ("op-c", 500),
        ("op-b", 500),
        ("op-a", 500),
        ("op-older", 400),
    ] {
        let mut status = sample(id);
        status.accepted_at = UnixSeconds(at);
        status.ended_at = Some(UnixSeconds(at + 1));
        insert_or_update(&conn, &key(id), &status).unwrap();
    }

    let repo = RepositoryToken::new("r").unwrap();
    let terminal = &[OperationState::Succeeded, OperationState::Failed][..];

    // Walk every page the way a client does: follow the cursor until a
    // page comes back short. Capped, so a predicate that never advances
    // fails loudly instead of hanging the suite.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<(UnixSeconds, OperationId)> = None;
    let mut terminated = false;
    for _ in 0..10 {
        let page = select_operations_blocking(&conn, &repo, terminal, cursor.as_ref(), 2).unwrap();
        if page.is_empty() {
            terminated = true;
            break;
        }
        let last = page.last().unwrap();
        cursor = Some((last.accepted_at, last.id.clone()));
        seen.extend(page.iter().map(|s| s.id.as_str().to_string()));
    }
    assert!(
        terminated,
        "the walk never reached an empty page — a keyset predicate that \
         does not advance past a tie repeats the same page forever, which \
         in a browser is an infinite scroll with no way out: {seen:?}"
    );

    // Every row exactly once — duplicates and omissions both fail here,
    // because this compares the multiset, not a set.
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec!["op-a", "op-b", "op-c", "op-older"],
        "every row must appear exactly once across the pages: {seen:?}"
    );

    // And the order the pages hand them back in is the total order the
    // index describes: newest first, id descending inside a shared second.
    assert_eq!(seen, vec!["op-c", "op-b", "op-a", "op-older"]);
}

/// The history query is scoped to one repository and to rows that really
/// have settled: another repository's row, a still-running row, and a row
/// that is inconsistent in each of the two possible directions are all
/// invisible to it.
///
/// The three excluded rows are chosen so that **each** predicate is the
/// only thing keeping one of them out — mutation-checked, after an earlier
/// version was found vacuous: with one ordinary `Running`-and-unfinished
/// row, `state IN (...)` and `ended_at IS NOT NULL` each covered for the
/// other, so deleting either one alone left the test green.
///
///  * drop `repository = ?1` → `other-repo` appears;
///  * drop `state IN (...)` → `running-but-ended` appears;
///  * drop `ended_at IS NOT NULL` → `ended-at-missing` appears, and
///    `HistoryEntry`'s non-optional `ended_at` would have nothing to
///    decode from it.
#[test]
fn the_history_query_shows_only_this_repositorys_terminal_rows() {
    let (_dir, path) = scratch_db();
    let conn = open_at(&path).unwrap();

    insert_or_update(&conn, &key("mine"), &sample("mine")).unwrap();

    // Non-terminal, but carrying an end time — only the state filter
    // excludes it.
    let mut running = sample("running-but-ended");
    running.state = OperationState::Running;
    running.stage = OperationStage::Executing;
    insert_or_update(&conn, &key("running-but-ended"), &running).unwrap();

    // Terminal, but with no end time — the shape a hand-edited or
    // half-written row has, and only the `ended_at` guard excludes it.
    let mut unfinished = sample("ended-at-missing");
    unfinished.ended_at = None;
    insert_or_update(&conn, &key("ended-at-missing"), &unfinished).unwrap();

    let mut elsewhere = sample("other-repo");
    elsewhere.repository = RepositoryToken::new("other").unwrap();
    insert_or_update(&conn, &key("other-repo"), &elsewhere).unwrap();

    let rows = select_operations_blocking(
        &conn,
        &RepositoryToken::new("r").unwrap(),
        &[OperationState::Succeeded, OperationState::Failed],
        None,
        50,
    )
    .unwrap();
    assert_eq!(
        rows.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["mine"]
    );
}

/// A row whose payload no longer decodes must still be **scanned** — key
/// intact, payload carried as incompatible (#509) — never silently dropped
/// from the walk.
///
/// This is the durable half of the pagination fix: `recovery_center`'s
/// pager counts and cursors on scanned keys, which only works if a bad row
/// reaches it as a keyed entry. Goes red if `row_to_scanned` reverts to
/// returning only decoded survivors — the corrupt row vanishes, the scan
/// comes back one short, and one bad row can again hide every older
/// operation behind it.
#[test]
fn a_row_with_an_undecodable_payload_is_scanned_with_its_key_intact() {
    let (_dir, path) = scratch_db();
    let conn = open_at(&path).unwrap();
    for (id, at) in [("op-new", 300), ("op-bad", 200), ("op-old", 100)] {
        let mut status = sample(id);
        status.accepted_at = UnixSeconds(at);
        status.ended_at = Some(UnixSeconds(at + 1));
        insert_or_update(&conn, &key(id), &status).unwrap();
    }
    // Corrupt the middle row's payload out-of-band, the way a partial
    // write or a hand edit would. Its key columns are untouched.
    conn.execute(
        "UPDATE operations SET operation_json = 'not json' WHERE id = 'op-bad'",
        [],
    )
    .unwrap();

    let repo = RepositoryToken::new("r").unwrap();
    let terminal = &[OperationState::Succeeded, OperationState::Failed][..];
    let rows = select_operations_blocking(&conn, &repo, terminal, None, 50).unwrap();

    assert_eq!(
        rows.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["op-new", "op-bad", "op-old"],
        "the corrupt row must still occupy its place in the scan"
    );
    assert_eq!(rows[1].accepted_at, UnixSeconds(200));
    // #509: an undecodable payload is carried as an incompatible record —
    // never a decoded guess, and (since `'not json'` has no `"op"` envelope)
    // never a claimed op kind either.
    match &rows[1].payload {
        ScannedPayload::Incompatible(record) => {
            assert_eq!(record.op_kind, None, "'not json' carries no op string");
        }
        other => panic!("the corrupt row's payload must not be guessed at: {other:?}"),
    }
    assert!(
        matches!(rows[0].payload, ScannedPayload::Decoded(_))
            && matches!(rows[2].payload, ScannedPayload::Decoded(_)),
        "the rows around it decode as before"
    );
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

// ---------------------------------------------------------------------------
// #509 — rows from a binary that understood an operation this one does not
// ---------------------------------------------------------------------------

/// The issue's own fixture, verbatim: a schema-v2 `operation_json` written by
/// a binary that still had `PopStash` (removed in #501). The current enum has
/// no `pop_stash` arm, so this payload can never decode here.
const POP_STASH_JSON: &str =
    r#"{"op":"pop_stash","repo":"/tmp/repo","selector":"stash@{0}","expected_oid":"0123...789"}"#;

/// Overwrite `id`'s payload with [`POP_STASH_JSON`] out-of-band — the state a
/// journal is in after a downgrade past an operation's removal.
fn strand_as_pop_stash(conn: &Connection, id: &str) {
    conn.execute(
        "UPDATE operations SET operation_json = ?1 WHERE id = ?2",
        rusqlite::params![POP_STASH_JSON, id],
    )
    .unwrap();
}

/// #509, acceptance 1: "cannot decode" and "does not exist" are
/// distinguishable at the single-record lookup. Before the fix both were
/// `None`, and the handler above answered 404 for a row that was sitting
/// right there in the table.
#[test]
fn a_pop_stash_row_is_looked_up_as_incompatible_never_as_missing() {
    let (_dir, path) = scratch_db();
    let conn = open_at(&path).unwrap();
    insert_or_update(&conn, &key("pop-stash"), &sample("pop-stash-op")).unwrap();
    strand_as_pop_stash(&conn, "pop-stash-op");

    match load_operation_blocking(&conn, &OperationId::new("pop-stash-op").unwrap()) {
        DurableLookup::Incompatible(record) => {
            // The raw stored facts are readable even though the payload is
            // not — and the op string is the bytes' own word, not a guess.
            assert_eq!(record.op_kind.as_deref(), Some("pop_stash"));
            assert_eq!(record.key, key("pop-stash"));
            assert_eq!(record.state_raw, "succeeded");
            assert_eq!(record.repository_raw, "r");
            assert_eq!(record.accepted_at, UnixSeconds(1_000));
        }
        DurableLookup::Found(_) => panic!("an unknown op variant must not decode"),
        DurableLookup::Missing => {
            panic!("#509: 'cannot decode' must never read as 'does not exist'")
        }
    }

    // The other half of the distinction: an id nothing ever wrote.
    assert!(matches!(
        load_operation_blocking(&conn, &OperationId::new("never-written").unwrap()),
        DurableLookup::Missing
    ));
}

/// A row whose `stage` column carries a spelling this build does not know —
/// the shape a DOWNGRADE produces when a later build added a stage. The
/// payload here is an operation this build understands perfectly.
fn strand_with_unknown_stage(conn: &Connection, id: &str) {
    conn.execute(
        "UPDATE operations SET stage = 'verifying' WHERE id = ?1",
        rusqlite::params![id],
    )
    .unwrap();
}

/// **A readable `op` string is not evidence of version skew.**
///
/// #509 shipped keying its "written by a build that understood an operation
/// this build does not" sentence on `op_kind.is_some()` — but `op_kind` is
/// lifted from the raw JSON envelope whichever field actually failed, and a
/// row can fail on `state`, `stage`, `operation_hash`, `repository` or
/// `worktree` while carrying an operation this build knows. The message was
/// then UPDATEd permanently into the `message` column, so a later build that
/// *does* understand the operation would read its own history claiming it does
/// not.
///
/// This drives the REAL decode path for both shapes — the hand-built
/// `IncompatibleRecord`s elsewhere cannot observe which arm failed, which is
/// exactly what hid this.
///
/// MUTATION 1 (mechanism removed): make the decode closure collapse to one
///   failure again (`DecodeFailure::UnknownOperation` for every arm) — red on
///   the unknown-stage row's blame.
/// MUTATION 2 (mechanism weakened): have `blame()` fall back to
///   `UnknownOperation(kind)` whenever `op_kind` is readable — red on the
///   persisted close-out sentence instead, a different assertion.
#[test]
fn only_an_unknown_operation_may_be_blamed_on_another_build() {
    let (_dir, path) = scratch_db();
    let conn = open_at(&path).unwrap();

    // (a) genuinely unknown operation -> skew may be claimed.
    insert_or_update(&conn, &key("skew"), &sample("skew-op")).unwrap();
    strand_as_pop_stash(&conn, "skew-op");

    // (b) an operation this build knows, in a row whose `stage` it does not.
    insert_or_update(&conn, &key("stage"), &sample("stage-op")).unwrap();
    strand_with_unknown_stage(&conn, "stage-op");

    let blame_of = |id: &str| match load_operation_blocking(&conn, &OperationId::new(id).unwrap()) {
        DurableLookup::Incompatible(record) => record.blame(),
        DurableLookup::Found(_) => panic!("{id}: expected an incompatible row, it decoded"),
        DurableLookup::Missing => panic!("{id}: expected an incompatible row, it read as missing"),
    };

    assert_eq!(
        blame_of("skew-op"),
        crate::durable::Blame::UnknownOperation("pop_stash".to_string()),
        "an operation this build cannot deserialize is the one case skew may be claimed"
    );
    assert_eq!(
        blame_of("stage-op"),
        crate::durable::Blame::UnreadableField("stage"),
        "#509 follow-up: an unreadable column must never be blamed on another build's operation"
    );

    // And the sentence that gets PERSISTED — the one a later build reads back
    // as a claim about itself. It must name the field and blame nobody.
    let message = incompatible_close_out_message_for_test(&conn, "stage-op");
    assert!(
        message.contains("`stage`") && !message.contains("understood an operation"),
        "the persisted close-out must say what could not be read, not who wrote it; got: {message}"
    );
}

/// The close-out sentence this build would persist for `id`, by the real
/// path — loads the row, then asks the same builder `recover_blocking` uses.
fn incompatible_close_out_message_for_test(conn: &Connection, id: &str) -> String {
    match load_operation_blocking(conn, &OperationId::new(id).unwrap()) {
        DurableLookup::Incompatible(record) => super::incompatible_close_out_message(&record),
        DurableLookup::Found(_) => panic!("{id}: expected an incompatible row, it decoded"),
        DurableLookup::Missing => panic!("{id}: expected an incompatible row, it read as missing"),
    }
}

/// #509, acceptance 2: the history scan carries the stranded row as
/// incompatible, with its stored facts intact, instead of omitting it.
#[test]
fn a_pop_stash_row_surfaces_in_the_history_scan_with_its_stored_facts() {
    let (_dir, path) = scratch_db();
    let conn = open_at(&path).unwrap();
    let mut status = sample("pop-stash-history");
    // The shape such a row has once startup recovery has closed it out.
    status.state = OperationState::Failed;
    status.status = Some(500);
    insert_or_update(&conn, &key("pop-stash-history"), &status).unwrap();
    strand_as_pop_stash(&conn, "pop-stash-history");

    let rows = select_operations_blocking(
        &conn,
        &RepositoryToken::new("r").unwrap(),
        &[OperationState::Succeeded, OperationState::Failed],
        None,
        50,
    )
    .unwrap();
    assert_eq!(rows.len(), 1, "the row must occupy its place in the scan");
    match &rows[0].payload {
        ScannedPayload::Incompatible(record) => {
            assert_eq!(record.op_kind.as_deref(), Some("pop_stash"));
            assert_eq!(record.state_raw, "failed");
            assert_eq!(record.status, Some(500));
        }
        other => panic!("the stranded row must surface as incompatible: {other:?}"),
    }
}

/// #509, acceptance 3: startup recovery closes out a stranded `running` row —
/// the row that, before the fix, stayed 'running' forever because the sweep
/// only saw records that decoded. The close-out is durable, honest about why,
/// and leaves the payload bytes untouched for a build that understands them.
#[test]
fn a_running_pop_stash_row_is_closed_out_as_failed_at_startup() {
    let (dir, path) = scratch_db();
    {
        let conn = open_at(&path).unwrap();
        let mut stranded = sample("stranded-pop-stash");
        stranded.state = OperationState::Running;
        stranded.stage = OperationStage::Executing;
        stranded.status = None;
        stranded.message = None;
        stranded.ended_at = None;
        insert_or_update(&conn, &key("stranded"), &stranded).unwrap();
        strand_as_pop_stash(&conn, "stranded-pop-stash");
    }
    // `recover_blocking` wants the same `'static` shape `open_private` hands
    // out; leak a private connection the same way it does.
    let conn: &'static StdMutex<Connection> =
        Box::leak(Box::new(StdMutex::new(open_at(&path).unwrap())));
    std::mem::forget(dir);

    let journal = recover_blocking(conn).unwrap();
    assert!(
        journal
            .records
            .iter()
            .all(|(_, s)| s.id.as_str() != "stranded-pop-stash"),
        "an undecodable row must never be dressed as a decoded record"
    );
    let record = journal
        .incompatible
        .iter()
        .find(|r| r.id.as_str() == "stranded-pop-stash")
        .expect("the stranded row must come back as incompatible");
    assert_eq!(record.state_raw, "failed");
    assert_eq!(record.status, Some(500));
    assert!(record.ended_at.is_some(), "terminal means an end time");
    let message = record.message.as_deref().unwrap();
    assert!(message.contains("'pop_stash'"), "{message}");
    assert!(message.contains("closed it out as failed"), "{message}");
    // The marker a returning build needs. The close-out OVERWRITES `state`,
    // `stage`, `status`, `message` and `ended_at`, so without this prefix a
    // later build that understands `pop_stash` cannot tell a genuine failure
    // from a stranger's close-out — it would read a 500 it never produced as
    // its own operation's real outcome.
    assert!(
        message.starts_with("closed-out-by-incompatible-build:"),
        "a close-out must be distinguishable from a genuine failure: {message}"
    );

    // Durable, not just in the returned view: the row itself can never read
    // 'running' again, while its payload stays byte-for-byte what the older
    // binary wrote.
    let guard = conn.lock().unwrap();
    let (state, ended_at, json): (String, Option<i64>, String) = guard
        .query_row(
            "SELECT state, ended_at, operation_json FROM operations
             WHERE id = 'stranded-pop-stash'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "failed");
    assert!(ended_at.is_some());
    assert_eq!(json, POP_STASH_JSON);
}
