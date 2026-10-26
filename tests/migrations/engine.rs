//! Canyon's own tables on real databases (created through the engine, then written and
//! read back), and whole runs in special cases: two at once, a statement the database
//! refuses, an upgrade from what the old engine made, hostile values.

use super::api::lock_free_now;
use super::introspection::{apply, read};
use super::runner::{id, league, lock_for, params, q, settings};
use super::scenarios::{num, planned, target};
use crate::support::test_db::TestDb;
use crate::support::{is_mysql, is_postgres, per_backend};
use canyon_migrations::dialect::DdlDialect;
use canyon_migrations::memory::{
    Declined, HistoryRow, SnapshotDoc, canyon_tables, insert_history, insert_snapshot, new_run_id,
    read_legacy, read_snapshot,
};
use canyon_migrations::model::{Generated, Ident, QualifiedName, SchemaModel, TableDef};
use canyon_migrations::op::{Op, Param, Statement};
use canyon_migrations::planning::{Conflict, Decisions};
use canyon_sql::CanyonResult;
use canyon_sql::connection::{DatabaseType, DbConnection};
use canyon_sql::core::{CanyonRows, SqlType};
use canyon_sql::date_time::NaiveDate;
use canyon_sql::migrations::{
    ApplyOptions, DbIdentity, MigrationError, MigrationsConfig, Target, plan_models, run_models,
    run_models_with,
};
use canyon_sql::query::QueryParameter;
use canyon_sql::runtime::tokio;
use std::time::{Duration, Instant};

const NASTY: &str = "it's a 'quote' ] ` -- ; DROP TABLE x";

fn history(db: DatabaseType, row: &HistoryRow<'_>) -> Statement {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    match db {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => insert_history::<PgDialect>(row),
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => insert_history::<MsSql>(row),
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => insert_history::<MySql>(row),
    }
}

fn snapshot(db: DatabaseType, run_id: &str, doc: &SnapshotDoc) -> Statement {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    match db {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => insert_snapshot::<PgDialect>(run_id, doc),
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => insert_snapshot::<MsSql>(run_id, doc),
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => insert_snapshot::<MySql>(run_id, doc),
    }
    .expect("the document is readable")
}

async fn read_latest(db: &TestDb) -> CanyonResult<Option<String>> {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    match db.db_type() {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => read_snapshot::<PgDialect>(db.conn()).await,
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => read_snapshot::<MsSql>(db.conn()).await,
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => read_snapshot::<MySql>(db.conn()).await,
    }
}

async fn run(db: &TestDb, st: &Statement) -> CanyonResult<()> {
    let params: Vec<&dyn QueryParameter> = st.params.iter().map(|p| p.as_query_param()).collect();
    db.conn().execute(&st.sql, &params).await?;
    Ok(())
}

async fn text(db: &TestDb, sql: &str) -> CanyonResult<String> {
    db.conn().query_one_for::<String>(sql, &[]).await
}

fn user_doc() -> SnapshotDoc {
    let table = TableDef::builder("Player", "player")
        .column("id", SqlType::Int, false)
        .primary_key(Some(Generated::Identity))
        .column("name", SqlType::Text { max_len: Some(20) }, true)
        .build()
        .expect("a valid table");
    let declined = Declined {
        table: QualifiedName::parse("player").expect("a valid name"),
        column: Ident::new("name").expect("a valid name"),
        column_type: SqlType::Text { max_len: Some(10) },
    };
    SnapshotDoc::new(
        SchemaModel::from_tables(vec![table]).expect("a valid model"),
        vec![declined],
    )
}

async fn create_canyon_tables(db: &TestDb) -> CanyonResult<()> {
    for table in canyon_tables().expect("static tables") {
        apply(db, &Op::CreateTable(table)).await?;
    }
    Ok(())
}

pub(crate) async fn canyon_tables_hold_history_and_snapshots(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "canyon_tables", async |db| {
        assert!(read(db, &[]).await.canyon_tables.is_empty());
        create_canyon_tables(db).await?;
        let catalog = read(db, &[]).await;
        assert_eq!(
            catalog.canyon_tables,
            ["canyon_migration_history", "canyon_schema_snapshot"]
        );
        assert!(catalog.model.tables.is_empty(), "left out of the model");

        let started = NaiveDate::from_ymd_opt(2026, 10, 25)
            .and_then(|d| d.and_hms_micro_opt(12, 30, 5, 123_456))
            .expect("a valid time");
        let run_id = new_run_id();
        let mut row = HistoryRow {
            run_id: &run_id,
            datasource: "ds",
            started_at: started,
            duration_ms: 5_000_000_000,
            op: "CreateTable",
            statement: NASTY,
            status: "ok",
            error: None,
        };
        run(db, &history(db.db_type(), &row)).await?;
        row.status = "failed";
        row.error = Some(NASTY);
        run(db, &history(db.db_type(), &row)).await?;

        let stored = text(
            db,
            "SELECT statement FROM canyon_migration_history WHERE status = 'ok'",
        )
        .await?;
        assert_eq!(stored, NASTY);
        let stored = text(
            db,
            "SELECT error FROM canyon_migration_history WHERE status = 'failed'",
        )
        .await?;
        assert_eq!(stored, NASTY);
        let stored = text(
            db,
            "SELECT run_id FROM canyon_migration_history WHERE status = 'ok'",
        )
        .await?;
        assert_eq!(stored, run_id);
        let nulls = db
            .conn()
            .query_rows(
                "SELECT 1 FROM canyon_migration_history WHERE error IS NULL AND status = 'ok'",
                &[],
            )
            .await?;
        assert_eq!(nulls.len(), 1, "no error is NULL");

        // No snapshot yet, then the newest one
        assert_eq!(read_latest(db).await?, None);
        let first = user_doc();
        run(db, &snapshot(db.db_type(), "run-1", &first)).await?;
        let mut second = user_doc();
        second.declined_tighten.clear();
        run(db, &snapshot(db.db_type(), "run-2", &second)).await?;
        let json = read_latest(db).await?.expect("a snapshot");
        assert_eq!(SnapshotDoc::from_json(&json).expect("readable"), second);

        // Writing them didn't change what introspection sees of the user's schema
        assert!(read(db, &[]).await.model.tables.is_empty());
        Ok(())
    })
    .await
}
per_backend!(
    canyon_tables_hold_history_and_snapshots,
    super::canyon_tables_hold_history_and_snapshots
);

pub(crate) async fn legacy_rows_are_read_when_the_table_exists(
    datasource: &str,
) -> CanyonResult<()> {
    TestDb::with(datasource, "legacy", async |db| {
        let catalog = read(db, &[]).await;
        assert!(!catalog.canyon_tables.iter().any(|t| t == "canyon_memory"));
        // MySQL answers without asking the database, so no table is needed there
        if is_mysql(db.db_type()) {
            assert!(read_legacy(db.conn()).await?.is_empty());
        }

        let ddl = match db.db_type() {
            #[cfg(feature = "postgres")]
            DatabaseType::PostgreSql => {
                "CREATE TABLE canyon_memory (id INTEGER PRIMARY KEY GENERATED ALWAYS AS IDENTITY, \
                 filepath VARCHAR NOT NULL, struct_name VARCHAR NOT NULL, \
                 declared_table_name VARCHAR NOT NULL)"
            }
            #[cfg(feature = "mssql")]
            DatabaseType::SqlServer => {
                "CREATE TABLE canyon_memory (id INT PRIMARY KEY IDENTITY, \
                 filepath NVARCHAR(250) NOT NULL, struct_name NVARCHAR(100) NOT NULL, \
                 declared_table_name NVARCHAR(100) NOT NULL)"
            }
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => {
                "CREATE TABLE canyon_memory (id INT PRIMARY KEY AUTO_INCREMENT, \
                 filepath VARCHAR(250) NOT NULL, struct_name VARCHAR(100) NOT NULL, \
                 declared_table_name VARCHAR(100) NOT NULL)"
            }
        };
        db.conn().execute(ddl, &[]).await?;
        for insert in [
            "INSERT INTO canyon_memory (filepath, struct_name, declared_table_name) \
             VALUES ('src/a.rs', 'League', 'leagues')",
            "INSERT INTO canyon_memory (filepath, struct_name, declared_table_name) \
             VALUES ('src/b.rs', 'Player', 'player')",
        ] {
            db.conn().execute(insert, &[]).await?;
        }

        let catalog = read(db, &[]).await;
        assert_eq!(catalog.canyon_tables, ["canyon_memory"]);
        assert!(catalog.model.tables.is_empty(), "left out of the model");
        let legacy = read_legacy(db.conn()).await?;
        match db.db_type() {
            // The old engine never made this table there
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => assert!(legacy.is_empty()),
            _ => {
                let pairs: Vec<_> = legacy
                    .iter()
                    .map(|r| (r.struct_name.as_str(), r.table.as_str()))
                    .collect();
                assert_eq!(pairs, [("League", "leagues"), ("Player", "player")]);
            }
        }
        Ok(())
    })
    .await
}
per_backend!(
    legacy_rows_are_read_when_the_table_exists,
    super::legacy_rows_are_read_when_the_table_exists
);

// ---- through the whole run: concurrency, failures, upgrade, hostile values

const HISTORY: &str = "canyon_migration_history";
const SNAPSHOT: &str = "canyon_schema_snapshot";
const HOSTILE: &str = "it's ] -- ;";
const HOSTILE_NAME: &str = "it's \"q\" `b` ]x /* c */ -- ;";

fn destructive() -> MigrationsConfig {
    let mut all = settings(5);
    all.allow_destructive = true;
    all
}

/// The first column of every row, as text (`NULL` read as `None`).
async fn texts(db: &TestDb, sql: &str) -> CanyonResult<Vec<Option<String>>> {
    Ok(match db.conn().query_rows(sql, &[]).await? {
        #[cfg(feature = "postgres")]
        CanyonRows::Postgres(rows) => rows
            .iter()
            .map(|r| r.get::<usize, Option<String>>(0))
            .collect(),
        #[cfg(feature = "mssql")]
        CanyonRows::Tiberius(rows) => rows
            .iter()
            .map(|r| r.get::<&str, usize>(0).map(str::to_owned))
            .collect(),
        #[cfg(feature = "mysql")]
        CanyonRows::MySQL(rows) => rows
            .iter()
            .map(|r| r.get::<Option<String>, usize>(0).flatten())
            .collect(),
    })
}

/// What a plan has to be, as `planned` gives it.
fn ops(described: &[&str]) -> Result<Vec<String>, Vec<String>> {
    Ok(described.iter().map(|d| (*d).to_owned()).collect())
}

fn player_named(name: SqlType) -> TableDef {
    TableDef::builder("Player", "player")
        .column("id", SqlType::Int, false)
        .primary_key(None)
        .column("name", name, false)
        .build()
        .expect("a valid table")
}

pub(crate) async fn two_runs_at_once(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "two_runs", async |db| {
        let desired = [league(), player_named(SqlType::Text { max_len: Some(20) })];
        let (one, two) = (target(db, settings(10)), target(db, settings(10)));
        let started = Instant::now();
        let (a, b) = tokio::join!(run_models(&one, &desired), run_models(&two, &desired));
        let elapsed = started.elapsed();
        let (a, b) = (a?, b?);
        let (a, b) = (&a.datasources[0], &b.datasources[0]);
        let (applied, idle) = if a.statements > 0 { (a, b) } else { (b, a) };
        assert!(applied.statements > 0, "{a:?} {b:?}");
        assert_eq!(idle.statements, 0, "the second found nothing to do");
        // Refused at least once, the second waited a retry before it got the lock
        assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");

        let runs = texts(db, &format!("SELECT DISTINCT run_id FROM {HISTORY}")).await?;
        assert_eq!(
            runs,
            [Some(applied.run_id.clone())],
            "one run in the history"
        );
        let n = num(db, "COUNT(*)", HISTORY).await?;
        assert_eq!(usize::try_from(n).ok(), Some(applied.statements));
        assert_eq!(num(db, "COUNT(*)", SNAPSHOT).await?, 1);
        let tables: Vec<_> = read(db, &[]).await.model.tables.into_keys().collect();
        assert_eq!(tables, [q("league"), q("player")]);
        // Free at once, not a few ms later (MySQL unlocks before closing)
        assert!(lock_free_now(db).await?);

        // While another session holds the lock, a run gives up after its timeout
        let (try_lock, unlock) = lock_for(db.db_type());
        let holder = db.conn().acquire().await?;
        let taken: i64 = holder
            .query_one_for(&try_lock.sql, &params(&try_lock))
            .await?;
        assert_eq!(taken, 1);
        let outcome = run_models(&target(db, settings(1)), &desired).await;
        holder.query_rows(&unlock.sql, &params(&unlock)).await?;
        holder.release().await?;
        assert!(
            matches!(outcome, Err(MigrationError::LockTimeout { .. })),
            "{outcome:?}"
        );
        Ok(())
    })
    .await
}
per_backend!(two_runs_at_once, super::two_runs_at_once);

/// A statement the database refuses (`'b'` isn't a number): the error says where the
/// run stopped and what stayed, the history has the failure, and once the data is fixed
/// the next run gets to the target.
pub(crate) async fn a_failing_alter_reports_where_it_stopped(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "failing_alter", async |db| {
        run_models(
            &target(db, destructive()),
            &[player_named(SqlType::Text { max_len: Some(20) })],
        )
        .await?;
        for sql in [
            "INSERT INTO player (id, name) VALUES (1, '1')",
            "INSERT INTO player (id, name) VALUES (2, 'b')",
        ] {
            db.conn().execute(sql, &[]).await?;
        }
        let desired = [TableDef::builder("Player", "player")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .column("name", SqlType::Int, false)
            .column("nick", SqlType::Text { max_len: Some(20) }, true)
            .build()
            .expect("a valid table")];
        let (added, change) = (
            "add column player.nick (text(20))",
            "change type of player.name to integer",
        );
        assert_eq!(
            planned(db, &destructive(), &desired, None).await?.0,
            ops(&[added, change])
        );
        let before = read(db, &[]).await;
        let history_before = num(db, "COUNT(*)", HISTORY).await?;
        let err = run_models(&target(db, destructive()), &desired)
            .await
            .expect_err("'b' is not a number");
        let (statement, refused, stored, outcome) = match db.db_type() {
            #[cfg(feature = "postgres")]
            DatabaseType::PostgreSql => (
                r#"ALTER TABLE "player" ALTER COLUMN "name" TYPE integer USING "name"::integer"#,
                r#"invalid input syntax for type integer: "b""#,
                "PostgreSQL query failed (SQLSTATE 22P02)",
                "`scenario` was rolled back.",
            ),
            #[cfg(feature = "mssql")]
            DatabaseType::SqlServer => (
                "ALTER TABLE [player] ALTER COLUMN [name] int NOT NULL",
                "Conversion failed when converting the nvarchar value 'b' to data type int.",
                "SQL Server query failed (SQL Server error 245)",
                "`scenario` was rolled back.",
            ),
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => (
                "ALTER TABLE `player` MODIFY COLUMN `name` int NOT NULL",
                "Incorrect integer value: 'b' for column 'name' at row 2",
                "MySQL query failed (MySQL error 1366)",
                "Statements before this one stay applied (MySQL can't roll back schema changes); \
                 the next run continues from here.",
            ),
        };
        let text = err.to_string();
        let MigrationError::Apply {
            datasource,
            op,
            statement: failed,
            transactional,
            rolled_back,
            ..
        } = &err
        else {
            panic!("not an apply error: {text}");
        };
        assert_eq!(
            (datasource.as_str(), op.as_str(), failed.as_str()),
            ("scenario", change, statement)
        );
        assert!(
            text.starts_with(&format!(
                "Migrations for datasource `scenario` failed at \"{change}\": "
            )),
            "{text}"
        );
        assert!(text.contains(refused), "{text}");
        assert!(
            text.ends_with(&format!("\n  Statement: {statement}\n  {outcome}")),
            "{text}"
        );
        assert_eq!(*transactional, !is_mysql(db.db_type()));
        assert_eq!(rolled_back, transactional);
        assert!(lock_free_now(db).await?, "lock released after the failure");

        // The failure is in the history, with the error code but not the server's text
        let failed_row =
            |column: &str| format!("SELECT {column} FROM {HISTORY} WHERE status = 'failed'");
        assert_eq!(
            texts(db, &failed_row("op")).await?,
            [Some(change.to_owned())]
        );
        assert_eq!(
            texts(db, &failed_row("statement")).await?,
            [Some(statement.to_owned())]
        );
        let error = texts(db, &failed_row("error")).await?;
        assert_eq!(error, [Some(stored.to_owned())]);
        let error = error[0].as_deref().unwrap_or_default();
        assert!(
            !error.contains(refused) && !error.contains("'b'") && !error.contains("\"b\""),
            "{error}"
        );

        let after = read(db, &[]).await;
        if *transactional {
            // Rolled back: only the failure row is new
            assert_eq!(after.model, before.model);
            assert_eq!(after.canyon_tables, before.canyon_tables);
            assert_eq!(num(db, "COUNT(*)", HISTORY).await?, history_before + 1);
            assert_eq!(
                planned(db, &destructive(), &desired, None).await?.0,
                ops(&[added, change])
            );
        } else {
            // The added column stays, with its history row
            assert_eq!(num(db, "COUNT(*)", HISTORY).await?, history_before + 2);
            assert_eq!(
                planned(db, &destructive(), &desired, None).await?.0,
                ops(&[change])
            );
        }
        assert_eq!(
            num(db, "COUNT(*)", SNAPSHOT).await?,
            1,
            "no snapshot of a failed run"
        );

        // Fixed by hand, the next run gets to the target
        db.conn()
            .execute("UPDATE player SET name = '2' WHERE id = 2", &[])
            .await?;
        run_models(&target(db, destructive()), &desired).await?;
        assert_eq!(
            planned(db, &destructive(), &desired, None).await?.0,
            ops(&[])
        );
        assert_eq!(num(db, "SUM(name)", "player").await?, 3);
        let runs = format!("SELECT DISTINCT run_id FROM {HISTORY}");
        assert_eq!(
            texts(db, &runs).await?.len(),
            3,
            "start, failed and fixed runs"
        );
        assert_eq!(num(db, "COUNT(*)", SNAPSHOT).await?, 2);
        Ok(())
    })
    .await
}
per_backend!(
    a_failing_alter_reports_where_it_stopped,
    super::a_failing_alter_reports_where_it_stopped
);

/// What the old engine made, statement by statement: `CreateTable` with
/// `to_postgres_syntax` / `to_sqlserver_syntax`, then on PostgreSQL the primary key, the
/// identity and the foreign key (on SQL Server those three were never written), and its
/// `canyon_memory` table with a row per entity. It never migrated MySQL.
fn old_canyon_ddl(db: DatabaseType) -> &'static [&'static str] {
    match db {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => &[
            "CREATE TABLE \"league\" (\"id\" integer NOT NULL, \"ext_id\" bigint NOT NULL, \
             \"name\" text NOT NULL, \"short\" integer NOT NULL, \"nick\" text NOT NULL, \
             \"note\" text, \"created\" timestamp without time zone NOT NULL);",
            "ALTER TABLE \"league\" ADD PRIMARY KEY (\"id\");",
            "ALTER TABLE \"league\" ALTER COLUMN \"id\" ADD GENERATED ALWAYS AS IDENTITY;",
            "CREATE TABLE \"tournament\" (\"id\" integer NOT NULL, \"league\" integer NOT NULL);",
            "ALTER TABLE \"tournament\" ADD PRIMARY KEY (\"id\");",
            "ALTER TABLE \"tournament\" ALTER COLUMN \"id\" ADD GENERATED ALWAYS AS IDENTITY;",
            "ALTER TABLE tournament ADD CONSTRAINT tournament_league_fkey \
             FOREIGN KEY (league) REFERENCES league (id);",
            "CREATE TABLE IF NOT EXISTS canyon_memory (
            id INTEGER PRIMARY KEY GENERATED ALWAYS AS IDENTITY,
            filepath VARCHAR NOT NULL,
            struct_name VARCHAR NOT NULL,
            declared_table_name VARCHAR NOT NULL
        )",
        ],
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => &[
            "CREATE TABLE league (id INT NOT NULL, ext_id BIGINT NOT NULL, \
             name nvarchar(max) NOT NULL DEFAULT '', short INT NOT NULL, \
             nick nvarchar(max) NOT NULL DEFAULT '', note nvarchar(max), \
             created DATETIME2 NOT NULL);",
            "CREATE TABLE tournament (id INT NOT NULL, league INT NOT NULL);",
            "IF OBJECT_ID(N'[dbo].[canyon_memory]', N'U') IS NULL
        BEGIN
            CREATE TABLE dbo.canyon_memory (
                id					    INT PRIMARY KEY IDENTITY,
                filepath			    NVARCHAR(250) NOT NULL,
                struct_name			    NVARCHAR(100) NOT NULL,
                declared_table_name	    NVARCHAR(100) NOT NULL
            );
        END",
        ],
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => &[],
    }
}

/// The same entities today: `League` now maps to `leagues`, `short` is the `i16` it
/// always was, and `nick` became an `Option<String>`.
fn new_canyon_model() -> [TableDef; 2] {
    let text = SqlType::Text { max_len: None };
    let leagues = TableDef::builder("League", "leagues")
        .column("id", SqlType::Int, false)
        .primary_key(Some(Generated::Identity))
        .column("ext_id", SqlType::BigInt, false)
        .column("name", text, false)
        .column("short", SqlType::SmallInt, false)
        .column("nick", text, true)
        .column("note", text, true)
        .column("created", SqlType::Timestamp { precision: 6 }, false)
        .build()
        .expect("a valid table");
    let tournament = TableDef::builder("Tournament", "tournament")
        .column("id", SqlType::Int, false)
        .primary_key(Some(Generated::Identity))
        .column("league", SqlType::Int, false)
        .foreign_key("leagues", "id")
        .build()
        .expect("a valid table");
    [leagues, tournament]
}

pub(crate) async fn upgrade_from_old_canyon(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "upgrade", async |db| {
        let legacy = old_canyon_ddl(db.db_type());
        for ddl in legacy {
            db.conn().execute(ddl, &[]).await?;
        }
        if !legacy.is_empty() {
            let explicit_id = !is_postgres(db.db_type());
            let (league, tournament) = if explicit_id {
                (
                    "INSERT INTO league (id, ext_id, name, short, nick, note, created) \
                     VALUES (1, 10, 'Liga', 40000, 'l', NULL, '2020-01-02 03:04:05')",
                    "INSERT INTO tournament (id, league) VALUES (1, 1)",
                )
            } else {
                (
                    "INSERT INTO league (ext_id, name, short, nick, note, created) \
                     VALUES (10, 'Liga', 40000, 'l', NULL, '2020-01-02 03:04:05')",
                    "INSERT INTO tournament (league) VALUES (1)",
                )
            };
            for sql in [
                league,
                tournament,
                "INSERT INTO canyon_memory (filepath, struct_name, declared_table_name) \
                 VALUES ('src/league.rs', 'League', 'league')",
                "INSERT INTO canyon_memory (filepath, struct_name, declared_table_name) \
                 VALUES ('src/tournament.rs', 'Tournament', 'tournament')",
            ] {
                db.conn().execute(sql, &[]).await?;
            }
        }
        let desired = new_canyon_model();
        let canyon_memory = || async {
            let catalog = read(db, &[]).await;
            let rows = if catalog.canyon_tables.iter().any(|t| t == "canyon_memory") {
                Some(num(db, "COUNT(*)", "canyon_memory").await?)
            } else {
                None
            };
            Ok::<_, canyon_sql::CanyonError>(rows)
        };

        if legacy.is_empty() {
            // Nothing to upgrade: a first run like any other
            assert_eq!(
                planned(db, &settings(5), &desired, None).await?.0,
                ops(&[
                    "create table leagues",
                    "create table tournament",
                    "add foreign key tournament (league) to leagues"
                ])
            );
        } else {
            // The old `integer` for an `i16` holds a value `smallint` can't: refused, and
            // nothing written
            let before = read(db, &[]).await;
            let err = run_models(&target(db, settings(5)), &desired)
                .await
                .expect_err("40000 doesn't fit a smallint");
            let MigrationError::Conflicts(conflicts) = &err else {
                panic!("expected conflicts, got {err}");
            };
            assert_eq!(
                conflicts,
                &[(
                    "scenario".to_owned(),
                    vec![Conflict::ValuesOutOfRange {
                        table: q("leagues"),
                        column: id("short"),
                        to: SqlType::SmallInt,
                    }]
                )]
            );
            let after = read(db, &[]).await;
            assert_eq!(after.model, before.model);
            assert_eq!(after.canyon_tables, ["canyon_memory"]);
            assert_eq!(canyon_memory().await?, Some(2));

            db.conn()
                .execute("UPDATE league SET short = 7", &[])
                .await?;
            // `canyon_memory` keeps the rename; on SQL Server the old engine left the
            // keys out, so they come now (the identities by rebuilding the tables)
            let mut want = vec![
                "rename table league to leagues",
                "change type of leagues.short to smallint",
                "make leagues.nick nullable",
            ];
            if !is_postgres(db.db_type()) {
                want.extend([
                    "add primary key to leagues (id)",
                    "add primary key to tournament (id)",
                    "rebuild table leagues",
                    "rebuild table tournament",
                    "add foreign key tournament (league) to leagues",
                ]);
            }
            assert_eq!(
                planned(db, &settings(5), &desired, None).await?.0,
                ops(&want)
            );
        }

        run_models(&target(db, settings(5)), &desired).await?;
        assert_eq!(
            planned(db, &settings(5), &desired, None).await?.0,
            ops(&[]),
            "converged"
        );
        let model = read(db, &[]).await.model;
        let tables: Vec<_> = model.tables.keys().cloned().collect();
        assert_eq!(tables, [q("leagues"), q("tournament")]);
        assert!(
            model
                .table(&q("leagues"))
                .is_some_and(|t| t.is_identity(&id("id")))
        );
        assert!(
            model
                .table(&q("tournament"))
                .is_some_and(|t| t.is_identity(&id("id")))
        );

        // The identity carries on after the rows that were there; the key holds
        db.conn()
            .execute(
                "INSERT INTO leagues (ext_id, name, short, created) \
                 VALUES (20, 'Two', 2, '2021-01-01 00:00:00')",
                &[],
            )
            .await?;
        let kept = if legacy.is_empty() { 0 } else { 1 };
        assert_eq!(num(db, "MAX(id)", "leagues").await?, kept + 1);
        assert_eq!(num(db, "COUNT(*)", "leagues").await?, kept + 1);
        if kept == 1 {
            assert_eq!(num(db, "short", "leagues WHERE id = 1").await?, 7);
            assert_eq!(num(db, "ext_id", "leagues WHERE id = 1").await?, 10);
            assert_eq!(
                texts(db, "SELECT name FROM leagues WHERE id = 1").await?,
                [Some("Liga".to_owned())]
            );
            assert_eq!(num(db, "league", "tournament WHERE id = 1").await?, 1);
            let orphan = db
                .conn()
                .execute("INSERT INTO tournament (league) VALUES (99)", &[])
                .await;
            assert!(orphan.is_err(), "the foreign key is there");
        }
        // Read once, never written: the old table stays as it was
        assert_eq!(canyon_memory().await?, (kept == 1).then_some(2));
        Ok(())
    })
    .await
}
per_backend!(upgrade_from_old_canyon, super::upgrade_from_old_canyon);

/// Quotes, brackets, comment and statement markers in a fill value, in the value a
/// precheck filters on, and in a column name from the catalog: bound or quoted, they are
/// stored and read back as they are.
pub(crate) async fn hostile_values(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "hostile", async |db| {
        let code = SqlType::Text { max_len: Some(50) };
        let team = TableDef::builder("Team", "team")
            .column("code", code, false)
            .primary_key(None)
            .build()
            .expect("a valid table");
        let start = [team.clone(), player_named(code)];
        run_models(&target(db, destructive()), &start).await?;
        // Every backend's own quote character is in the name, doubled when quoted
        let quoted = match db.db_type() {
            #[cfg(feature = "postgres")]
            DatabaseType::PostgreSql => format!("\"{}\"", HOSTILE_NAME.replace('"', "\"\"")),
            #[cfg(feature = "mssql")]
            DatabaseType::SqlServer => format!("[{}]", HOSTILE_NAME.replace(']', "]]")),
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => format!("`{}`", HOSTILE_NAME.replace('`', "``")),
        };
        db.conn()
            .execute(&format!("ALTER TABLE player ADD {quoted} INT"), &[])
            .await?;
        db.conn()
            .execute("INSERT INTO player (id, name) VALUES (1, 'a')", &[])
            .await?;

        // Every player gets the hostile team code, which must exist in `team`
        let desired = [
            team,
            TableDef::builder("Player", "player")
                .column("id", SqlType::Int, false)
                .primary_key(None)
                .column("name", code, false)
                .column("team", code, false)
                .foreign_key("team", "code")
                .build()
                .expect("a valid table"),
        ];
        let mut decisions = Decisions::from_config(true, 100_000);
        decisions
            .fill_values
            .insert((q("player"), id("team")), Param::Text(HOSTILE.to_owned()));
        let place = match db.db_type() {
            #[cfg(feature = "postgres")]
            DatabaseType::PostgreSql => "$1",
            #[cfg(feature = "mssql")]
            DatabaseType::SqlServer => "@P1",
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => "?",
        };
        let add_team = format!("INSERT INTO team (code) VALUES ({place})");
        let run = async || {
            run_models_with(
                &target(db, destructive()),
                &desired,
                Some(&decisions),
                &ApplyOptions::default(),
            )
            .await
        };

        // Only a code one character short exists: the precheck compares the whole value
        let near = &HOSTILE[..HOSTILE.len() - 1];
        db.conn().execute(&add_team, &[&near]).await?;
        let before = read(db, &[]).await;
        let err = run().await.expect_err("no team has that code yet");
        let MigrationError::Conflicts(conflicts) = &err else {
            panic!("expected conflicts, got {err}");
        };
        assert_eq!(
            conflicts,
            &[(
                "scenario".to_owned(),
                vec![Conflict::OrphanRows {
                    table: q("player"),
                    columns: vec![id("team")],
                    target: q("team"),
                }]
            )]
        );
        assert_eq!(read(db, &[]).await.model, before.model);

        db.conn().execute(&add_team, &[&HOSTILE]).await?;
        let report = run().await?;
        assert_eq!(
            planned(db, &destructive(), &desired, None).await?.0,
            ops(&[]),
            "converged"
        );
        assert_eq!(
            texts(db, "SELECT team FROM player").await?,
            [Some(HOSTILE.to_owned())]
        );
        let model = read(db, &[]).await.model;
        let columns: Vec<_> = (model.table(&q("player")).into_iter())
            .flat_map(|t| t.columns.iter())
            .map(|c| c.name.as_str().to_owned())
            .collect();
        assert_eq!(columns, ["id", "name", "team"]);

        // The history has the statements as they ran: the name quoted, the value bound
        let run_id = &report.datasources[0].run_id;
        let statements = texts(
            db,
            &format!("SELECT statement FROM {HISTORY} WHERE run_id = '{run_id}' ORDER BY id"),
        )
        .await?;
        let player = match db.db_type() {
            #[cfg(feature = "postgres")]
            DatabaseType::PostgreSql => "\"player\"",
            #[cfg(feature = "mssql")]
            DatabaseType::SqlServer => "[player]",
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => "`player`",
        };
        let drop = format!("ALTER TABLE {player} DROP COLUMN {quoted}");
        assert_eq!(statements.len(), report.datasources[0].statements);
        assert_eq!(
            (statements.iter().flatten())
                .filter(|s| **s == drop)
                .count(),
            1,
            "{statements:?}"
        );
        assert!(
            (statements.iter().flatten()).any(|s| s.starts_with("UPDATE") && s.contains(place)),
            "{statements:?}"
        );
        assert!(
            !(statements.iter().flatten()).any(|s| s.contains("'it")),
            "the value is never written into the SQL: {statements:?}"
        );
        Ok(())
    })
    .await
}
per_backend!(hostile_values, super::hostile_values);
