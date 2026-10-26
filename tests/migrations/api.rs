//! The API over several datasources (two throwaway databases on the same server): plan,
//! all-or-nothing run, and the interactive loop with a scripted prompt.

use super::introspection::{apply as run_op, read};
use super::runner::{id, league, lock_for, params, q, rows, settings};
use crate::support::per_backend;
use crate::support::test_db::TestDb;
use canyon_migrations::interactive::{Answer, Prompt, Question};
use canyon_migrations::memory::SnapshotDoc;
use canyon_migrations::model::{Logical, TableDef};
use canyon_migrations::op::Op;
use canyon_migrations::planning::Conflict;
use canyon_sql::connection::{DatabaseConnector, DbConnection};
use canyon_sql::core::{Canyon, SqlType};
use canyon_sql::migrations::{
    ApplyOptions, DbIdentity, MAX_ROUNDS, MigrationError, Report, Target, interactive_models,
    plan_models, run_models, run_models_with,
};
use canyon_sql::runtime::tokio;
use canyon_sql::{CanyonError, CanyonResult};
use std::sync::{Arc, Mutex};

const SNAPSHOT: &str = "canyon_schema_snapshot";

fn target<'a>(name: &'a str, db: &'a TestDb) -> Target<'a> {
    Target {
        datasource: name,
        conn: db.conn(),
        settings: settings(5),
        identity: DbIdentity::default(),
    }
}

/// `player (id, name)` plus nullable text columns `extra`.
fn player(name: bool, extra: &[&str]) -> TableDef {
    let mut b = TableDef::builder("Player", "player")
        .column("id", SqlType::Int, false)
        .primary_key(None);
    if name {
        b = b.column("name", SqlType::Text { max_len: Some(50) }, true);
    }
    for c in extra {
        b = b.column(c, SqlType::Text { max_len: Some(50) }, true);
    }
    b.build().expect("a valid table")
}

fn with_score() -> TableDef {
    TableDef::builder("Player", "player")
        .column("id", SqlType::Int, false)
        .primary_key(None)
        .column("name", SqlType::Text { max_len: Some(50) }, true)
        .column("score", SqlType::Int, false)
        .build()
        .expect("a valid table")
}

/// Another session takes the migration lock at once: no waiting for it.
pub(crate) async fn lock_free_now(db: &TestDb) -> CanyonResult<bool> {
    let (try_lock, unlock) = lock_for(db.db_type());
    let session = db.conn().acquire().await?;
    let taken: i64 = session
        .query_one_for(&try_lock.sql, &params(&try_lock))
        .await?;
    if taken == 1 {
        session.query_rows(&unlock.sql, &params(&unlock)).await?;
    }
    session.release().await?;
    Ok(taken == 1)
}

fn column_names(table: Option<&TableDef>) -> Vec<String> {
    (table.into_iter())
        .flat_map(|t| t.columns.iter())
        .map(|c| c.name.as_str().to_owned())
        .collect()
}

pub(crate) async fn plan_writes_nothing(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_plan", async |db| {
        let plan = plan_models(&[target("a", db)], &[league()]).await?;
        assert_eq!(plan.datasources.len(), 1);
        let ops = plan.datasources[0].outcome.as_ref().expect("no conflicts");
        let described: Vec<&str> = ops.iter().map(|o| o.description.as_str()).collect();
        assert_eq!(
            described,
            [
                "create table canyon_schema_snapshot",
                "create table canyon_migration_history",
                "create table league"
            ]
        );
        assert!(plan.to_string().contains("create table league"));
        let after = read(db, &[]).await;
        assert!(after.model.tables.is_empty(), "{:?}", after.model);
        assert!(after.canyon_tables.is_empty());
        assert!(lock_free_now(db).await?);
        Ok(())
    })
    .await
}
per_backend!(plan_writes_nothing, super::plan_writes_nothing);

pub(crate) async fn one_conflict_writes_nowhere(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_clean", async |clean| {
        TestDb::with(datasource, "api_rows", async |busy| {
            for db in [clean, busy] {
                run_op(db, &Op::CreateTable(player(true, &[]))).await?;
            }
            busy.conn()
                .execute("INSERT INTO player (id, name) VALUES (1, 'a')", &[])
                .await?;
            let targets = [target("clean", clean), target("busy", busy)];
            let desired = [with_score()];
            let (clean_before, busy_before) = (read(clean, &[]).await, read(busy, &[]).await);

            // A NOT NULL column without a value: only `busy` has rows
            let err = run_models(&targets, &desired)
                .await
                .expect_err("a conflict on `busy`");
            let MigrationError::Conflicts(conflicts) = &err else {
                panic!("expected conflicts, got {err}");
            };
            assert_eq!(
                conflicts,
                &[(
                    "busy".to_owned(),
                    vec![Conflict::NotNullOnTableWithRows {
                        table: q("player"),
                        column: id("score"),
                    }]
                )]
            );
            for (db, before) in [(clean, &clean_before), (busy, &busy_before)] {
                let after = read(db, &[]).await;
                assert_eq!(after.model, before.model);
                assert!(after.canyon_tables.is_empty(), "not even Canyon's tables");
                assert!(lock_free_now(db).await?, "lock released after the conflict");
            }

            // `plan` reports it per datasource instead of failing
            let plan = plan_models(&targets, &desired).await?;
            assert!(plan.datasources[0].outcome.is_ok());
            assert!(plan.datasources[1].outcome.is_err());

            // Clean everywhere: both apply, under one run id
            busy.conn().execute("DELETE FROM player", &[]).await?;
            let report = run_models(&targets, &desired).await?;
            let names: Vec<&str> = (report.datasources.iter())
                .map(|r| r.datasource.as_str())
                .collect();
            assert_eq!(names, ["clean", "busy"]);
            assert_eq!(report.datasources[0].run_id, report.datasources[1].run_id);
            for db in [clean, busy] {
                let after = read(db, &[]).await;
                assert_eq!(
                    column_names(after.model.table(&q("player"))),
                    ["id", "name", "score"]
                );
                assert_eq!(after.canyon_tables.len(), 2);
                assert!(lock_free_now(db).await?, "lock released after the run");
            }
            Ok(())
        })
        .await
    })
    .await
}
per_backend!(
    one_conflict_writes_nowhere,
    super::one_conflict_writes_nowhere
);

type Asked = Arc<Mutex<Vec<Question>>>;

/// Answers in order, then stops. On a "yes" to a confirmation it can add a column by
/// hand first, as someone else migrating in between would.
struct Scripted {
    answers: std::vec::IntoIter<Answer>,
    asked: Asked,
    /// Columns to add to `player`, one per confirmation.
    meddle: Option<(DatabaseConnector, tokio::runtime::Handle, Vec<String>)>,
}

impl Prompt for Scripted {
    fn ask(&mut self, question: &Question) -> Answer {
        if let Ok(mut asked) = self.asked.lock() {
            asked.push(question.clone());
        }
        let answer = self.answers.next().unwrap_or(Answer::Abort);
        if let (Question::Confirm { .. }, Some((conn, handle, columns))) =
            (question, &mut self.meddle)
            && let Some(column) = columns.pop()
        {
            let sql = format!("ALTER TABLE player ADD {column} INT");
            handle
                .block_on(conn.execute(&sql, &[]))
                .expect("adding a column by hand");
        }
        answer
    }
}

fn scripted(answers: Vec<Answer>) -> (Box<dyn Prompt + Send>, Asked) {
    let asked = Asked::default();
    let prompt = Scripted {
        answers: answers.into_iter(),
        asked: Arc::clone(&asked),
        meddle: None,
    };
    (Box::new(prompt), asked)
}

fn asked(log: &Asked) -> Vec<Question> {
    log.lock().map(|a| a.clone()).unwrap_or_default()
}

/// The datasource's configuration, pointed at the test's database (a macro: the config
/// type isn't exported to the tests).
macro_rules! config_for {
    ($datasource:expr, $db:expr) => {{
        let mut config = Canyon::instance()?
            .find_datasource_by_name_or_default($datasource)?
            .clone();
        config.properties.db_name = $db.name().to_owned();
        config
    }};
}

/// A second pool to the test's database, for a runtime of its own.
async fn connect_again(datasource: &str, db: &TestDb) -> CanyonResult<DatabaseConnector> {
    DatabaseConnector::new(&config_for!(datasource, db)).await
}

/// `interactive_models` on a current-thread runtime of its own thread, with a pool
/// made there: asking can't use `block_in_place`, which panics on such a runtime.
async fn interactive_on_current_thread(
    datasource: &str,
    db: &TestDb,
    desired: Vec<TableDef>,
    prompt: Box<dyn Prompt + Send>,
) -> CanyonResult<Result<Report, MigrationError>> {
    let config = config_for!(datasource, db);
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime");
        runtime.block_on(async move {
            let conn = DatabaseConnector::new(&config).await?;
            let targets = [Target {
                datasource: "a",
                conn: &conn,
                settings: settings(5),
                identity: DbIdentity::default(),
            }];
            Ok::<_, CanyonError>(interactive_models(&targets, &desired, prompt).await)
        })
    });
    tokio::task::spawn_blocking(move || thread.join())
        .await
        .expect("the waiting task")
        .expect("the current-thread test thread")
}

pub(crate) async fn interactive_answers_a_rename(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_rename", async |db| {
        run_models(&[target("a", db)], &[player(true, &[])]).await?;
        db.conn()
            .execute("INSERT INTO player (id, name) VALUES (1, 'kept')", &[])
            .await?;
        // `name` went, two text columns came: which one is it?
        let desired = vec![player(false, &["nick", "alias"])];
        let (prompt, log) = scripted(vec![Answer::Choose(0)]);
        let report = interactive_on_current_thread(datasource, db, desired.clone(), prompt)
            .await?
            .expect("the rename is answered and applied");
        assert_eq!(report.datasources.len(), 1);
        assert!(report.datasources[0].statements > 0);
        let questions = asked(&log);
        assert!(
            matches!(&questions[..], [Question::ColumnRename { removed, added, .. }]
                if removed.name == id("name") && added.len() == 2),
            "{questions:?}"
        );

        let model = read(db, &[]).await.model;
        assert_eq!(
            column_names(model.table(&q("player"))),
            ["id", "nick", "alias"]
        );
        let nick: String = db
            .conn()
            .query_one_for("SELECT nick FROM player WHERE id = 1", &[])
            .await?;
        assert_eq!(nick, "kept", "renamed, not dropped and added");
        assert_eq!(rows(db, &format!("SELECT 1 FROM {SNAPSHOT}")).await?, 2);
        assert!(lock_free_now(db).await?);

        // The snapshot has the answer: nothing to ask, nothing to do
        let (prompt, log) = scripted(Vec::new());
        let report = interactive_models(&[target("a", db)], &desired, prompt).await?;
        assert_eq!(report.datasources[0].statements, 0);
        assert!(asked(&log).is_empty());
        Ok(())
    })
    .await
}
per_backend!(
    interactive_answers_a_rename,
    super::interactive_answers_a_rename
);

pub(crate) async fn a_no_stops_with_the_conflict(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_no", async |db| {
        run_models(&[target("a", db)], &[player(true, &[])]).await?;
        let before = read(db, &[]).await;
        let (prompt, log) = scripted(vec![Answer::No]);
        let err = interactive_models(&[target("a", db)], &[player(false, &[])], prompt)
            .await
            .expect_err("the drop isn't confirmed");
        let MigrationError::Conflicts(conflicts) = &err else {
            panic!("expected conflicts, got {err}");
        };
        assert!(
            matches!(&conflicts[..], [(ds, c)] if ds == "a"
                && matches!(&c[..], [Conflict::Destructive { .. }])),
            "{err}"
        );
        assert!(
            matches!(&asked(&log)[..], [Question::Confirm { .. }]),
            "asked once"
        );
        assert_eq!(read(db, &[]).await.model, before.model);
        assert_eq!(rows(db, &format!("SELECT 1 FROM {SNAPSHOT}")).await?, 1);
        assert!(lock_free_now(db).await?);
        Ok(())
    })
    .await
}
per_backend!(
    a_no_stops_with_the_conflict,
    super::a_no_stops_with_the_conflict
);

pub(crate) async fn a_value_for_existing_rows_is_never_asked(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_no_fill", async |db| {
        run_models(&[target("a", db)], &[player(true, &[])]).await?;
        db.conn()
            .execute("INSERT INTO player (id, name) VALUES (1, 'a')", &[])
            .await?;
        let before = read(db, &[]).await;
        // A NOT NULL column on a table with rows: stops like `run`, nothing asked
        let (prompt, log) = scripted(vec![Answer::Yes]);
        let err = interactive_models(&[target("a", db)], &[with_score()], prompt)
            .await
            .expect_err("no value for the existing rows");
        let MigrationError::Conflicts(conflicts) = &err else {
            panic!("expected conflicts, got {err}");
        };
        assert_eq!(
            conflicts,
            &[(
                "a".to_owned(),
                vec![Conflict::NotNullOnTableWithRows {
                    table: q("player"),
                    column: id("score"),
                }]
            )]
        );
        assert!(asked(&log).is_empty(), "{:?}", asked(&log));
        assert_eq!(read(db, &[]).await.model, before.model);
        assert!(lock_free_now(db).await?);
        Ok(())
    })
    .await
}
per_backend!(
    a_value_for_existing_rows_is_never_asked,
    super::a_value_for_existing_rows_is_never_asked
);

pub(crate) async fn nulls_blocking_not_null_are_never_asked(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_no_fill_nulls", async |db| {
        run_models(&[target("a", db)], &[player(true, &[])]).await?;
        db.conn()
            .execute("INSERT INTO player (id, name) VALUES (1, NULL)", &[])
            .await?;
        let before = read(db, &[]).await;
        // `name` becomes NOT NULL over a NULL: stops, nothing asked
        let required = TableDef::builder("Player", "player")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .column("name", SqlType::Text { max_len: Some(50) }, false)
            .build()
            .expect("a valid table");
        let (prompt, log) = scripted(vec![Answer::Yes]);
        let err = interactive_models(&[target("a", db)], &[required], prompt)
            .await
            .expect_err("no value for the NULL rows");
        let MigrationError::Conflicts(conflicts) = &err else {
            panic!("expected conflicts, got {err}");
        };
        assert_eq!(
            conflicts,
            &[(
                "a".to_owned(),
                vec![Conflict::NullsBlockNotNull {
                    table: q("player"),
                    column: id("name"),
                }]
            )]
        );
        assert!(asked(&log).is_empty(), "{:?}", asked(&log));
        assert_eq!(read(db, &[]).await.model, before.model);
        assert!(lock_free_now(db).await?);
        Ok(())
    })
    .await
}
per_backend!(
    nulls_blocking_not_null_are_never_asked,
    super::nulls_blocking_not_null_are_never_asked
);

/// `player (id, x)` with one row, then `x` gone and `y`, `z` (NOT NULL) added.
async fn rename_or_not_null(
    db: &TestDb,
    answer: usize,
) -> CanyonResult<(Result<Report, MigrationError>, Vec<Question>)> {
    let text = SqlType::Text { max_len: Some(50) };
    let before = TableDef::builder("Player", "player")
        .column("id", SqlType::Int, false)
        .primary_key(None)
        .column("x", text, true)
        .build()
        .expect("a valid table");
    run_models(&[target("a", db)], &[before]).await?;
    db.conn()
        .execute("INSERT INTO player (id, x) VALUES (1, 'kept')", &[])
        .await?;
    let after = TableDef::builder("Player", "player")
        .column("id", SqlType::Int, false)
        .primary_key(None)
        .column("y", text, true)
        .column("z", text, false)
        .build()
        .expect("a valid table");
    let (prompt, log) = scripted(vec![Answer::Choose(answer)]);
    let outcome = interactive_models(&[target("a", db)], &[after], prompt).await;
    Ok((outcome, asked(&log)))
}

pub(crate) async fn a_rename_is_asked_before_a_not_null_conflict(
    datasource: &str,
) -> CanyonResult<()> {
    TestDb::with(datasource, "api_rename_z", async |db| {
        // `x` → `z`: no column added, the conflict goes away
        let (outcome, questions) = rename_or_not_null(db, 1).await?;
        outcome.expect("the rename is answered and applied");
        assert!(
            matches!(&questions[..], [Question::ColumnRename { removed, added, .. }]
                if removed.name == id("x") && added.len() == 2),
            "{questions:?}"
        );
        let model = read(db, &[]).await.model;
        // Renamed in place, `y` added after it
        assert_eq!(column_names(model.table(&q("player"))), ["id", "z", "y"]);
        let z: String = db
            .conn()
            .query_one_for("SELECT z FROM player WHERE id = 1", &[])
            .await?;
        assert_eq!(z, "kept");
        assert!(lock_free_now(db).await?);
        Ok(())
    })
    .await?;
    TestDb::with(datasource, "api_rename_none", async |db| {
        // Not a rename: `z` is added NOT NULL over the row, the run stops
        let (outcome, questions) = rename_or_not_null(db, 2).await?;
        let err = outcome.expect_err("the NOT NULL conflict stays");
        assert!(
            matches!(&questions[..], [Question::ColumnRename { .. }]),
            "{questions:?}"
        );
        let MigrationError::Conflicts(conflicts) = &err else {
            panic!("expected conflicts, got {err}");
        };
        let not_null = Conflict::NotNullOnTableWithRows {
            table: q("player"),
            column: id("z"),
        };
        assert!(
            matches!(&conflicts[..], [(ds, c)] if ds == "a" && c.contains(&not_null)),
            "{err}"
        );
        let after = read(db, &[]).await.model;
        assert_eq!(
            column_names(after.table(&q("player"))),
            ["id", "x"],
            "nothing applied"
        );
        assert!(lock_free_now(db).await?);
        Ok(())
    })
    .await
}
per_backend!(
    a_rename_is_asked_before_a_not_null_conflict,
    super::a_rename_is_asked_before_a_not_null_conflict
);

/// Each "yes" is followed by someone adding a column, which the locked plan then wants
/// to drop: asked again each round. `added` columns are added in all.
async fn meddled(
    datasource: &str,
    db: &TestDb,
    added: usize,
) -> CanyonResult<Result<Report, MigrationError>> {
    run_models(&[target("a", db)], &[player(true, &[])]).await?;
    let columns = (1..=added).rev().map(|i| format!("extra{i}")).collect();
    let log = Asked::default();
    let prompt = Scripted {
        answers: vec![Answer::Yes; MAX_ROUNDS + 2].into_iter(),
        asked: Arc::clone(&log),
        meddle: Some((
            connect_again(datasource, db).await?,
            tokio::runtime::Handle::current(),
            columns,
        )),
    };
    let outcome =
        interactive_models(&[target("a", db)], &[player(false, &[])], Box::new(prompt)).await;
    let confirms = asked(&log)
        .iter()
        .filter(|q| matches!(q, Question::Confirm { .. }))
        .count();
    assert_eq!(confirms, (added + 1).min(MAX_ROUNDS), "{outcome:?}");
    assert!(lock_free_now(db).await?);
    Ok(outcome)
}

pub(crate) async fn new_conflicts_are_asked_again(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_meddled", async |db| {
        // One column added while asking: asked about too, then everything goes
        meddled(datasource, db, 1).await??;
        let model = read(db, &[]).await.model;
        assert_eq!(column_names(model.table(&q("player"))), ["id"]);
        Ok(())
    })
    .await?;
    TestDb::with(datasource, "api_rounds", async |db| {
        // A new one every round: it gives up, and drops nothing
        let err = meddled(datasource, db, MAX_ROUNDS)
            .await?
            .expect_err("the rounds run out");
        assert!(matches!(err, MigrationError::Conflicts(_)), "{err}");
        let model = read(db, &[]).await.model;
        let mut expected = vec!["id".to_owned(), "name".to_owned()];
        expected.extend((1..=MAX_ROUNDS).map(|i| format!("extra{i}")));
        assert_eq!(column_names(model.table(&q("player"))), expected);
        Ok(())
    })
    .await
}
per_backend!(
    new_conflicts_are_asked_again,
    super::new_conflicts_are_asked_again
);

pub(crate) async fn a_declined_tighten_is_remembered(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_tighten", async |db| {
        // The database's `name` is wider than the field: offered, declined
        let wide = TableDef::builder("Player", "player")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .column("name", SqlType::Text { max_len: Some(100) }, true)
            .build()
            .expect("a valid table");
        run_op(db, &Op::CreateTable(wide)).await?;
        let desired = [player(true, &[])];
        let (prompt, log) = scripted(vec![Answer::No]);
        interactive_models(&[target("a", db)], &desired, prompt).await?;
        assert!(
            matches!(&asked(&log)[..], [Question::Tighten { column, .. }] if *column == id("name")),
            "{:?}",
            asked(&log)
        );
        let json: String = db
            .conn()
            .query_one_for(&format!("SELECT model FROM {SNAPSHOT}"), &[])
            .await?;
        let doc = SnapshotDoc::from_json(&json).expect("a readable snapshot");
        let declined: Vec<_> = (doc.declined_tighten.iter())
            .map(|d| (d.table.clone(), d.column.clone()))
            .collect();
        assert_eq!(declined, [(q("player"), id("name"))]);
        let model = read(db, &[]).await.model;
        let name = (model.table(&q("player")).into_iter())
            .flat_map(|t| t.columns.iter())
            .find(|c| c.name == id("name"))
            .map(|c| c.ty.clone());
        assert_eq!(
            name,
            Some(Logical::Known(SqlType::Text { max_len: Some(100) })),
            "left as it was"
        );

        // Not asked again while the field stays the same
        let (prompt, log) = scripted(Vec::new());
        let report = interactive_models(&[target("a", db)], &desired, prompt).await?;
        assert!(asked(&log).is_empty(), "{:?}", asked(&log));
        assert_eq!(report.datasources[0].statements, 0);
        assert!(lock_free_now(db).await?);
        Ok(())
    })
    .await
}
per_backend!(
    a_declined_tighten_is_remembered,
    super::a_declined_tighten_is_remembered
);

pub(crate) async fn a_later_failure_keeps_what_was_applied(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "api_part_a", async |a| {
        TestDb::with(datasource, "api_part_b", async |b| {
            TestDb::with(datasource, "api_part_c", async |c| {
                // `a` only needs `create table player`; `b` and `c` need Canyon's tables too
                run_models(&[target("a", a)], &[league()]).await?;
                let targets = [target("a", a), target("b", b), target("c", c)];
                let options = ApplyOptions {
                    fail_after: Some(2),
                };
                let err = run_models_with(&targets, &[league(), player(true, &[])], None, &options)
                    .await
                    .expect_err("`b` fails on purpose");
                let MigrationError::PartlyApplied {
                    applied,
                    not_attempted,
                    error,
                } = &err
                else {
                    panic!("expected a partial run, got {err}");
                };
                let names: Vec<&str> = applied.iter().map(|r| r.datasource.as_str()).collect();
                assert_eq!(names, ["a"]);
                assert_eq!(not_attempted, &["c".to_owned()]);
                let MigrationError::Apply {
                    datasource: failed,
                    transactional,
                    rolled_back,
                    ..
                } = &**error
                else {
                    panic!("expected an apply error, got {error}");
                };
                assert_eq!(failed, "b");
                let text = err.to_string();
                assert!(text.contains("`a` (1 statement)"), "{text}");
                assert!(text.contains("`c` was not attempted"), "{text}");
                assert!(
                    text.contains("`b` was rolled back") == *rolled_back,
                    "{text}"
                );

                let tables = read(a, &[])
                    .await
                    .model
                    .tables
                    .into_keys()
                    .collect::<Vec<_>>();
                assert_eq!(tables, [q("league"), q("player")]);

                // `b`: all or nothing where DDL is transactional, what ran stays on MySQL
                let after_b = read(b, &[]).await;
                if *transactional {
                    assert!(*rolled_back, "{text}");
                    assert!(after_b.model.tables.is_empty(), "{:?}", after_b.model);
                    assert!(after_b.canyon_tables.is_empty());
                } else {
                    assert!(
                        !after_b.canyon_tables.is_empty() || !after_b.model.tables.is_empty(),
                        "the statements before the failure stay"
                    );
                }

                // `c` was never touched
                let after_c = read(c, &[]).await;
                assert!(after_c.model.tables.is_empty(), "{:?}", after_c.model);
                assert!(after_c.canyon_tables.is_empty());
                for db in [a, b, c] {
                    assert!(lock_free_now(db).await?);
                }
                Ok(())
            })
            .await
        })
        .await
    })
    .await
}
per_backend!(
    a_later_failure_keeps_what_was_applied,
    super::a_later_failure_keeps_what_was_applied
);
