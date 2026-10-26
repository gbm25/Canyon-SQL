//! The runner for one datasource on real databases: plan, apply, history, snapshot, lock.

use super::introspection::{apply as run_op, read};
use crate::support::test_db::TestDb;
use crate::support::{is_mysql, per_backend};
use canyon_migrations::dialect::DdlDialect;
use canyon_migrations::error::MigrationError;
use canyon_migrations::memory::{SnapshotDoc, new_run_id};
use canyon_migrations::model::{Ident, QualifiedName, TableDef};
use canyon_migrations::op::{Op, Param, Statement};
use canyon_migrations::planning::{Conflict, Decisions};
use canyon_migrations::report::{DatasourceReport, RenderedOp};
use canyon_migrations::runner::{ApplyOptions, MigrationsConfig, Staged, prepare_one};
use canyon_sql::CanyonResult;
use canyon_sql::connection::{DatabaseType, DbConnection};
use canyon_sql::core::SqlType;
use canyon_sql::query::QueryParameter;
use canyon_sql::runtime::tokio::time::sleep;
use std::time::{Duration, Instant};

const HISTORY: &str = "canyon_migration_history";
const SNAPSHOT: &str = "canyon_schema_snapshot";

pub(crate) fn q(s: &str) -> QualifiedName {
    QualifiedName::parse(s).expect("a valid name")
}

pub(crate) fn id(s: &str) -> Ident {
    Ident::new(s).expect("a valid name")
}

pub(crate) fn settings(lock_timeout_secs: u64) -> MigrationsConfig {
    MigrationsConfig {
        enabled: true,
        at_startup: false,
        lock_timeout_secs,
        ..MigrationsConfig::default()
    }
}

pub(crate) fn league() -> TableDef {
    TableDef::builder("League", "league")
        .column("id", SqlType::Int, false)
        .primary_key(None)
        .column("name", SqlType::Text { max_len: Some(50) }, true)
        .build()
        .expect("a valid table")
}

fn player(with_league: bool) -> TableDef {
    let b = TableDef::builder("Player", "player")
        .column("id", SqlType::Int, false)
        .primary_key(None)
        .column("name", SqlType::Text { max_len: Some(50) }, true);
    let b = if with_league {
        b.column("league", SqlType::Int, false)
            .foreign_key("league", "id")
    } else {
        b
    };
    b.build().expect("a valid table")
}

async fn prepare(
    db: &TestDb,
    desired: &[TableDef],
    settings: &MigrationsConfig,
    decisions: &Decisions,
) -> Result<Staged, MigrationError> {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    let (conn, ds) = (db.conn(), "runner_test");
    match db.db_type() {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => {
            prepare_one::<PgDialect>(conn, ds, desired, settings, decisions, true).await
        }
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => {
            prepare_one::<MsSql>(conn, ds, desired, settings, decisions, true).await
        }
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => {
            prepare_one::<MySql>(conn, ds, desired, settings, decisions, true).await
        }
    }
}

fn render(staged: &Staged) -> Vec<RenderedOp> {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    match staged.db_type() {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => staged.render::<PgDialect>(),
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => staged.render::<MsSql>(),
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => staged.render::<MySql>(),
    }
    .expect("the plan renders")
}

async fn apply(staged: Staged, options: &ApplyOptions) -> Result<DatasourceReport, MigrationError> {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    let run_id = new_run_id();
    match staged.db_type() {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => staged.apply_with::<PgDialect>(&run_id, options).await,
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => staged.apply_with::<MsSql>(&run_id, options).await,
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => staged.apply_with::<MySql>(&run_id, options).await,
    }
}

pub(crate) fn lock_for(db: DatabaseType) -> (Statement, Statement) {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    match db {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => (PgDialect::try_lock(), PgDialect::unlock()),
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => (MsSql::try_lock(), MsSql::unlock()),
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => (MySql::try_lock(), MySql::unlock()),
    }
}

pub(crate) fn params(st: &Statement) -> Vec<&dyn QueryParameter> {
    st.params.iter().map(|p| p.as_query_param()).collect()
}

pub(crate) async fn rows(db: &TestDb, sql: &str) -> CanyonResult<usize> {
    Ok(db.conn().query_rows(sql, &[]).await?.len())
}

/// Another session can take the lock. Polls a little: a closed connection frees it a
/// few ms later.
async fn lock_is_free(db: &TestDb) -> CanyonResult<bool> {
    let (try_lock, unlock) = lock_for(db.db_type());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let session = db.conn().acquire().await?;
        let taken: i64 = session
            .query_one_for(&try_lock.sql, &params(&try_lock))
            .await?;
        if taken == 1 {
            session.query_rows(&unlock.sql, &params(&unlock)).await?;
            session.release().await?;
            return Ok(true);
        }
        session.release().await?;
        if Instant::now() > deadline {
            return Ok(false);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn descriptions(ops: &[RenderedOp]) -> Vec<&str> {
    ops.iter().map(|o| o.description.as_str()).collect()
}

fn statements(ops: &[RenderedOp]) -> usize {
    ops.iter().map(|o| o.statements.len()).sum()
}

pub(crate) async fn applies_then_has_nothing_to_do(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "runner_apply", async |db| {
        let staged = prepare(db, &[league()], &settings(5), &Decisions::default()).await?;
        assert!(staged.conflicts().is_empty(), "{:?}", staged.conflicts());
        let plan = render(&staged);
        assert_eq!(
            descriptions(&plan),
            [
                "create table canyon_schema_snapshot",
                "create table canyon_migration_history",
                "create table league"
            ]
        );
        let report = apply(staged, &ApplyOptions::default()).await?;
        assert_eq!(report.statements, statements(&plan));

        let catalog = read(db, &[]).await;
        let tables: Vec<_> = catalog.model.tables.keys().cloned().collect();
        assert_eq!(tables, [q("league")]);
        assert_eq!(catalog.canyon_tables, [HISTORY, SNAPSHOT]);
        let ok = format!("SELECT 1 FROM {HISTORY} WHERE status = 'ok'");
        assert_eq!(rows(db, &ok).await?, report.statements);
        assert_eq!(
            rows(db, &format!("SELECT 1 FROM {HISTORY}")).await?,
            report.statements
        );
        let runs = format!("SELECT DISTINCT run_id FROM {HISTORY}");
        assert_eq!(rows(db, &runs).await?, 1);
        let json: String = db
            .conn()
            .query_one_for(&format!("SELECT model FROM {SNAPSHOT}"), &[])
            .await?;
        let doc = SnapshotDoc::from_json(&json).expect("a readable snapshot");
        let stored: Vec<_> = doc.tables.tables.keys().cloned().collect();
        assert_eq!(stored, [q("league")], "the model, without Canyon's tables");
        assert!(lock_is_free(db).await?, "lock released after the run");

        // Nothing changed: nothing planned, nothing written, not even a snapshot
        let staged = prepare(db, &[league()], &settings(5), &Decisions::default()).await?;
        assert!(render(&staged).is_empty());
        let report = apply(staged, &ApplyOptions::default()).await?;
        assert_eq!(report.statements, 0);
        assert_eq!(rows(db, &format!("SELECT 1 FROM {SNAPSHOT}")).await?, 1);
        assert!(lock_is_free(db).await?, "lock released after an empty run");
        Ok(())
    })
    .await
}
per_backend!(
    applies_then_has_nothing_to_do,
    super::applies_then_has_nothing_to_do
);

pub(crate) async fn conflicts_write_nothing(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "runner_conflict", async |db| {
        run_op(db, &Op::CreateTable(league())).await?;
        run_op(db, &Op::CreateTable(player(false))).await?;
        db.conn()
            .execute("INSERT INTO player (id, name) VALUES (1, 'a')", &[])
            .await?;
        // Every row of the new NOT NULL key gets 7, which `league` doesn't have
        let mut decisions = Decisions::default();
        decisions
            .fill_values
            .insert((q("player"), id("league")), Param::Int(7));
        let desired = [league(), player(true)];
        let before = read(db, &[]).await;

        let staged = prepare(db, &desired, &settings(5), &decisions).await?;
        assert_eq!(
            staged.conflicts(),
            [Conflict::OrphanRows {
                table: q("player"),
                columns: vec![id("league")],
                target: q("league"),
            }]
        );
        let err = apply(staged, &ApplyOptions::default())
            .await
            .expect_err("a plan with conflicts");
        assert!(matches!(err, MigrationError::Conflicts(_)), "{err}");
        assert!(lock_is_free(db).await?, "lock released after the conflict");
        let after = read(db, &[]).await;
        assert_eq!(after.model, before.model);
        assert!(after.canyon_tables.is_empty(), "not even Canyon's tables");

        // Once the target has the value, the same answers go through
        db.conn()
            .execute("INSERT INTO league (id, name) VALUES (7, 'l')", &[])
            .await?;
        let staged = prepare(db, &desired, &settings(5), &decisions).await?;
        assert!(staged.conflicts().is_empty(), "{:?}", staged.conflicts());
        apply(staged, &ApplyOptions::default()).await?;
        let model = read(db, &[]).await.model;
        let player = model.table(&q("player")).expect("player");
        assert_eq!(player.foreign_keys.len(), 1);
        let league = (player.columns.iter())
            .find(|c| c.name == id("league"))
            .expect("the new column");
        assert!(!league.nullable);
        let sql = match db.db_type() {
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => "SELECT CAST(league AS SIGNED) FROM player",
            _ => "SELECT CAST(league AS BIGINT) FROM player",
        };
        let filled: i64 = db.conn().query_one_for(sql, &[]).await?;
        assert_eq!(filled, 7);
        Ok(())
    })
    .await
}
per_backend!(conflicts_write_nothing, super::conflicts_write_nothing);

pub(crate) async fn nulls_filled_under_a_new_key_must_exist(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "runner_fill_nulls", async |db| {
        // `player.league` exists but is nullable and has no key yet
        let loose = TableDef::builder("Player", "player")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .column("name", SqlType::Text { max_len: Some(50) }, true)
            .column("league", SqlType::Int, true)
            .build()
            .expect("a valid table");
        run_op(db, &Op::CreateTable(league())).await?;
        run_op(db, &Op::CreateTable(loose)).await?;
        for sql in [
            "INSERT INTO league (id, name) VALUES (1, 'one')",
            "INSERT INTO player (id, name, league) VALUES (1, 'a', 1)",
            "INSERT INTO player (id, name, league) VALUES (2, 'b', NULL)",
        ] {
            db.conn().execute(sql, &[]).await?;
        }
        // The NULL gets 7, which `league` doesn't have; the row with 1 is fine
        let mut decisions = Decisions::default();
        decisions
            .fill_values
            .insert((q("player"), id("league")), Param::Int(7));
        let desired = [league(), player(true)];
        let before = read(db, &[]).await;
        let staged = prepare(db, &desired, &settings(5), &decisions).await?;
        assert_eq!(
            staged.conflicts(),
            [Conflict::OrphanRows {
                table: q("player"),
                columns: vec![id("league")],
                target: q("league"),
            }]
        );
        staged.release().await?;
        assert!(lock_is_free(db).await?);
        assert_eq!(read(db, &[]).await.model, before.model);

        db.conn()
            .execute("INSERT INTO league (id, name) VALUES (7, 'seven')", &[])
            .await?;
        let staged = prepare(db, &desired, &settings(5), &decisions).await?;
        assert!(staged.conflicts().is_empty(), "{:?}", staged.conflicts());
        apply(staged, &ApplyOptions::default()).await?;
        let model = read(db, &[]).await.model;
        let player = model.table(&q("player")).expect("player");
        assert_eq!(player.foreign_keys.len(), 1);
        let cast = match db.db_type() {
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => "SIGNED",
            _ => "BIGINT",
        };
        for (row, want) in [(1, 1i64), (2, 7)] {
            let sql = format!("SELECT CAST(league AS {cast}) FROM player WHERE id = {row}");
            let got: i64 = db.conn().query_one_for(&sql, &[]).await?;
            assert_eq!(got, want, "player {row}");
        }
        Ok(())
    })
    .await
}
per_backend!(
    nulls_filled_under_a_new_key_must_exist,
    super::nulls_filled_under_a_new_key_must_exist
);

pub(crate) async fn a_failure_rolls_back_or_stops(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "runner_failure", async |db| {
        let desired = [league(), player(true)];
        let staged = prepare(db, &desired, &settings(5), &Decisions::default()).await?;
        let plan = render(&staged);
        // Canyon's two tables and `league`
        assert_eq!(plan[2].description, "create table league");
        let n = statements(&plan[..3]);
        let err = apply(
            staged,
            &ApplyOptions {
                fail_after: Some(n),
            },
        )
        .await
        .expect_err("a forced failure");
        let MigrationError::Apply {
            transactional,
            rolled_back,
            ..
        } = err
        else {
            panic!("not an apply error: {err}");
        };
        assert!(lock_is_free(db).await?, "lock released after the failure");
        let catalog = read(db, &[]).await;
        let tables: Vec<_> = catalog.model.tables.keys().cloned().collect();
        if !is_mysql(db.db_type()) {
            assert!(transactional && rolled_back);
            assert!(tables.is_empty(), "{tables:?}");
            assert!(catalog.canyon_tables.is_empty());

            // With the history table there before the run, the failure stays recorded
            let staged = prepare(db, &[league()], &settings(5), &Decisions::default()).await?;
            let applied = apply(staged, &ApplyOptions::default()).await?.statements;
            let staged = prepare(db, &desired, &settings(5), &Decisions::default()).await?;
            let err = apply(
                staged,
                &ApplyOptions {
                    fail_after: Some(1),
                },
            )
            .await
            .expect_err("a forced failure");
            assert!(
                matches!(
                    err,
                    MigrationError::Apply {
                        rolled_back: true,
                        ..
                    }
                ),
                "{err}"
            );
            assert!(lock_is_free(db).await?);
            assert!(read(db, &[]).await.model.table(&q("player")).is_none());
            let ok = format!("SELECT 1 FROM {HISTORY} WHERE status = 'ok'");
            assert_eq!(
                rows(db, &ok).await?,
                applied,
                "the rolled-back rows are gone"
            );
            let failed = format!("SELECT 1 FROM {HISTORY} WHERE status = 'failed'");
            assert_eq!(rows(db, &failed).await?, 1);
            assert_eq!(rows(db, &format!("SELECT 1 FROM {SNAPSHOT}")).await?, 1);
            return Ok(());
        }
        // MySQL: what ran stays, with its history, and no snapshot
        assert!(!transactional);
        assert_eq!(tables, [q("league")]);
        assert_eq!(catalog.canyon_tables, [HISTORY, SNAPSHOT]);
        let ok = format!("SELECT 1 FROM {HISTORY} WHERE status = 'ok'");
        assert_eq!(rows(db, &ok).await?, n);
        let failed = format!("SELECT 1 FROM {HISTORY} WHERE status = 'failed'");
        assert_eq!(rows(db, &failed).await?, 1);
        assert_eq!(rows(db, &format!("SELECT 1 FROM {SNAPSHOT}")).await?, 0);

        // The next run continues where this one stopped
        let staged = prepare(db, &desired, &settings(5), &Decisions::default()).await?;
        let plan = render(&staged);
        assert_eq!(plan[0].description, "create table player", "{plan:?}");
        assert!(
            (descriptions(&plan).iter())
                .all(|d| *d != "create table league" && !d.starts_with("create table canyon")),
            "{plan:?}"
        );
        apply(staged, &ApplyOptions::default()).await?;
        assert_eq!(rows(db, &format!("SELECT 1 FROM {SNAPSHOT}")).await?, 1);
        Ok(())
    })
    .await
}
per_backend!(
    a_failure_rolls_back_or_stops,
    super::a_failure_rolls_back_or_stops
);

pub(crate) async fn a_held_lock_times_out(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "runner_lock", async |db| {
        let (try_lock, unlock) = lock_for(db.db_type());
        let holder = db.conn().acquire().await?;
        let taken: i64 = holder
            .query_one_for(&try_lock.sql, &params(&try_lock))
            .await?;
        assert_eq!(taken, 1);

        let started = Instant::now();
        let outcome = prepare(db, &[league()], &settings(1), &Decisions::default()).await;
        let waited = started.elapsed();
        holder.query_rows(&unlock.sql, &params(&unlock)).await?;
        holder.release().await?;
        match outcome {
            Err(MigrationError::LockTimeout { .. }) => {}
            Err(e) => panic!("not a lock timeout: {e}"),
            Ok(_) => panic!("took a lock another session holds"),
        }
        assert!(waited >= Duration::from_millis(900), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        assert!(lock_is_free(db).await?);
        assert!(read(db, &[]).await.canyon_tables.is_empty());
        Ok(())
    })
    .await
}
per_backend!(a_held_lock_times_out, super::a_held_lock_times_out);

#[cfg(feature = "postgres")]
fn player_name(max_len: u32) -> TableDef {
    TableDef::builder("Player", "player")
        .column("id", SqlType::Int, false)
        .primary_key(None)
        .column(
            "name",
            SqlType::Text {
                max_len: Some(max_len),
            },
            true,
        )
        .build()
        .expect("a valid table")
}

/// PostgreSQL: a text column made shorter over a row written after the checks ran (here,
/// after staging) fails the ALTER instead of cutting the value. Trailing spaces are
/// still trimmed, as on any assignment.
#[cfg(feature = "postgres")]
#[canyon_sql::macros::canyon_tokio_test]
fn pg_text_shrink_refuses_a_row_written_after_the_checks() {
    TestDb::with(crate::constants::PSQL_DS, "pg_shrink", async |db| {
        let staged = prepare(db, &[player_name(20)], &settings(5), &Decisions::default()).await?;
        apply(staged, &ApplyOptions::default()).await?;
        db.conn()
            .execute("INSERT INTO player (id, name) VALUES (1, 'short')", &[])
            .await?;

        // The snapshot says 20, so 5 is the code's intent: a checked type change
        let staged = prepare(db, &[player_name(5)], &settings(5), &Decisions::default()).await?;
        assert_eq!(
            descriptions(&render(&staged)),
            ["change type of player.name to text(5)"]
        );
        db.conn()
            .execute("INSERT INTO player (id, name) VALUES (2, 'too long')", &[])
            .await?;
        let err = apply(staged, &ApplyOptions::default())
            .await
            .expect_err("the long row is refused");
        let text = err.to_string();
        assert!(
            matches!(
                err,
                MigrationError::Apply {
                    rolled_back: true,
                    ..
                }
            ),
            "{text}"
        );
        assert!(
            text.contains("value too long for type character varying(5)"),
            "{text}"
        );
        let long = "SELECT 1 FROM player WHERE id = 2 AND name = 'too long'";
        assert_eq!(rows(db, long).await?, 1, "the row is intact");
        assert!(lock_is_free(db).await?);

        // Only spaces over the length: trimmed, the change goes through
        db.conn()
            .execute("UPDATE player SET name = 'abc      ' WHERE id = 2", &[])
            .await?;
        let staged = prepare(db, &[player_name(5)], &settings(5), &Decisions::default()).await?;
        apply(staged, &ApplyOptions::default()).await?;
        let trimmed = "SELECT 1 FROM player WHERE id = 2 AND name = 'abc  '";
        assert_eq!(rows(db, trimmed).await?, 1);
        Ok(())
    })
    .await?;
}
