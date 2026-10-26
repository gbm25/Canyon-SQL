//! The SQL the runner asks the database: prechecks and the migration lock.

use super::introspection::read;
use super::runner::lock_for;
use crate::support::per_backend;
use crate::support::test_db::TestDb;
use canyon_migrations::dialect::DdlDialect;
use canyon_migrations::model::{Ident, QualifiedName};
use canyon_migrations::op::Statement;
use canyon_migrations::planning::Precheck;
use canyon_sql::CanyonResult;
use canyon_sql::connection::{DatabaseType, DbConnection};
use canyon_sql::core::CanyonRows;
use canyon_sql::query::QueryParameter;
use canyon_sql::runtime::tokio::time::sleep;
use std::time::Duration;

fn q(s: &str) -> QualifiedName {
    QualifiedName::parse(s).expect("a valid name")
}

fn id(s: &str) -> Ident {
    Ident::new(s).expect("a valid name")
}

fn precheck_for(db: DatabaseType, p: &Precheck, schema: &str) -> Statement {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    match db {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => PgDialect::precheck(p, schema),
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => MsSql::precheck(p, schema),
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => MySql::precheck(p, schema),
    }
}

fn snapshot_for(db: DatabaseType) -> Statement {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    match db {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => PgDialect::latest_snapshot(),
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => MsSql::latest_snapshot(),
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => MySql::latest_snapshot(),
    }
}

async fn number(conn: &impl DbConnection, st: &Statement) -> CanyonResult<i64> {
    let params: Vec<&dyn QueryParameter> = st.params.iter().map(|p| p.as_query_param()).collect();
    conn.query_one_for::<i64>(&st.sql, &params).await
}

// `unlock` answers with a bool, an int or nothing, by backend: only that it runs matters
async fn run_unlock(conn: &impl DbConnection, st: &Statement) -> CanyonResult<()> {
    let params: Vec<&dyn QueryParameter> = st.params.iter().map(|p| p.as_query_param()).collect();
    conn.query_rows(&st.sql, &params).await?;
    Ok(())
}

fn first_text(rows: CanyonRows) -> Vec<String> {
    use CanyonRows;
    match rows {
        #[cfg(feature = "postgres")]
        CanyonRows::Postgres(rows) => rows.iter().map(|r| r.get::<usize, String>(0)).collect(),
        #[cfg(feature = "mssql")]
        CanyonRows::Tiberius(rows) => rows
            .iter()
            .filter_map(|r| r.get::<&str, usize>(0).map(str::to_owned))
            .collect(),
        #[cfg(feature = "mysql")]
        CanyonRows::MySQL(rows) => rows
            .iter()
            .filter_map(|r| r.get::<String, usize>(0))
            .collect(),
    }
}

// DDL that runs on all three
async fn setup(db: &TestDb) -> CanyonResult<()> {
    for ddl in [
        "CREATE TABLE p (id INT, age INT, name VARCHAR(20), lg SMALLINT)",
        "CREATE TABLE empty_t (id INT)",
        "CREATE TABLE n (a INT)",
        "CREATE TABLE l (id INT)",
        "INSERT INTO p VALUES (1, 10, 'abc', 5)",
        "INSERT INTO p VALUES (2, NULL, 'abcdefgh', 7)",
        "INSERT INTO p VALUES (2, 20, 'x', NULL)",
        "INSERT INTO n VALUES (NULL)",
        "INSERT INTO l VALUES (5)",
    ] {
        db.conn().execute(ddl, &[]).await?;
    }
    Ok(())
}

pub(crate) async fn prechecks_find_the_data(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "prechecks", async |db| {
        setup(db).await?;
        let schema = read(db, &[]).await.default_schema;
        let ask = async |p: Precheck| {
            let st = precheck_for(db.db_type(), &p, &schema);
            number(db.conn(), &st).await
        };
        let range = |column: &str, min, max| Precheck::OutOfRange {
            table: q("p"),
            column: id(column),
            min,
            max,
        };
        let longer = |max_len| Precheck::LongerThan {
            table: q("p"),
            column: id("name"),
            max_len,
        };
        let dup = |columns: &[&str]| Precheck::Duplicates {
            table: q("p"),
            columns: columns.iter().map(|c| id(c)).collect(),
        };
        let orphans = |target: &str| Precheck::Orphans {
            table: q("p"),
            columns: vec![id("lg")],
            target: q(target),
            target_columns: vec![id("id")],
        };
        let cases = [
            ("rows", Precheck::TableHasRows(q("p")), 1),
            ("no rows", Precheck::TableHasRows(q("empty_t")), 0),
            // counted up to 2
            ("count of three rows", Precheck::RowCount(q("p")), 2),
            ("count of one row", Precheck::RowCount(q("l")), 1),
            ("count of no rows", Precheck::RowCount(q("empty_t")), 0),
            (
                "non null found",
                Precheck::NonNull {
                    table: q("p"),
                    columns: vec![id("age"), id("lg")],
                },
                1,
            ),
            (
                "non null none",
                Precheck::NonNull {
                    table: q("n"),
                    columns: vec![id("a")],
                },
                0,
            ),
            (
                "nulls",
                Precheck::HasNulls {
                    table: q("p"),
                    column: id("age"),
                },
                1,
            ),
            (
                "no nulls",
                Precheck::HasNulls {
                    table: q("p"),
                    column: id("id"),
                },
                0,
            ),
            ("out of range int", range("age", 0, 15), 1),
            ("in range int", range("age", 0, 100), 0),
            // smallint column, bounds beyond i32
            ("out of range smallint", range("lg", 6, 100), 1),
            (
                "in range smallint",
                range("lg", -3_000_000_000, 3_000_000_000),
                0,
            ),
            ("longer", longer(5), 1),
            ("not longer", longer(8), 0),
            ("duplicates", dup(&["id"]), 1),
            ("no duplicates", dup(&["id", "age"]), 0),
            ("orphans", orphans("l"), 1),
            (
                "estimate of a missing table",
                Precheck::RowEstimate(q("no_such_table")),
                0,
            ),
        ];
        for (what, p, expected) in cases {
            assert_eq!(ask(p).await?, expected, "{what}");
        }
        // every orphan has its parent
        db.conn().execute("INSERT INTO l VALUES (7)", &[]).await?;
        assert_eq!(ask(orphans("l")).await?, 0, "no orphans");

        // an estimate: exact on SQL Server, from statistics elsewhere
        match db.db_type() {
            #[cfg(feature = "postgres")]
            DatabaseType::PostgreSql => {
                db.conn().execute("ANALYZE p", &[]).await?;
                assert_eq!(ask(Precheck::RowEstimate(q("p"))).await?, 3);
            }
            #[cfg(feature = "mssql")]
            DatabaseType::SqlServer => {
                assert_eq!(ask(Precheck::RowEstimate(q("p"))).await?, 3);
            }
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => {
                // statistics are cached for a day by default: read them fresh
                let session = db.conn().acquire().await?;
                session
                    .execute("SET SESSION information_schema_stats_expiry = 0", &[])
                    .await?;
                session.execute("ANALYZE TABLE p", &[]).await?;
                let st = precheck_for(db.db_type(), &Precheck::RowEstimate(q("p")), &schema);
                let estimate = number(&session, &st).await?;
                session.release().await?;
                assert_eq!(estimate, 3);
            }
        }
        Ok(())
    })
    .await
}
per_backend!(prechecks_find_the_data, super::prechecks_find_the_data);

pub(crate) async fn snapshot_read_has_no_row_when_empty(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "snapshot_read", async |db| {
        // The statement is valid against the table the runner keeps
        let ddl = match db.db_type() {
            #[cfg(feature = "postgres")]
            DatabaseType::PostgreSql => {
                "CREATE TABLE canyon_schema_snapshot (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, model TEXT NOT NULL)"
            }
            #[cfg(feature = "mssql")]
            DatabaseType::SqlServer => {
                "CREATE TABLE canyon_schema_snapshot (id BIGINT IDENTITY(1,1) PRIMARY KEY, model NVARCHAR(MAX) NOT NULL)"
            }
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => {
                "CREATE TABLE canyon_schema_snapshot (id BIGINT AUTO_INCREMENT PRIMARY KEY, model LONGTEXT NOT NULL)"
            }
        };
        db.conn().execute(ddl, &[]).await?;
        let st = snapshot_for(db.db_type());
        assert!(db.conn().query_rows(&st.sql, &[]).await?.is_empty());
        for model in ["old", "new"] {
            db.conn()
                .execute(
                    &format!("INSERT INTO canyon_schema_snapshot (model) VALUES ('{model}')"),
                    &[],
                )
                .await?;
        }
        let rows = db.conn().query_rows(&st.sql, &[]).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(first_text(rows), vec!["new".to_string()], "the newest row");
        Ok(())
    })
    .await
}
per_backend!(
    snapshot_read_has_no_row_when_empty,
    super::snapshot_read_has_no_row_when_empty
);

pub(crate) async fn prechecks_edge_cases(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "prechecks_edge", async |db| {
        for ddl in [
            "CREATE TABLE r (v INT)",
            "INSERT INTO r VALUES (0)",
            "INSERT INTO r VALUES (10)",
            "CREATE TABLE two (a INT, b INT)",
            "INSERT INTO two VALUES (1, NULL)",
            "INSERT INTO two VALUES (NULL, 2)",
            "CREATE TABLE parent (a INT, b INT)",
            "INSERT INTO parent VALUES (1, 1)",
            "CREATE TABLE child (x INT, y INT)",
            "INSERT INTO child VALUES (1, NULL)",
            "INSERT INTO child VALUES (NULL, NULL)",
            "INSERT INTO child VALUES (1, 1)",
            "CREATE TABLE d (a INT, b INT)",
            "INSERT INTO d VALUES (NULL, 1)",
            "CREATE TABLE s (name VARCHAR(20))",
            "INSERT INTO s VALUES ('abc   ')",
        ] {
            db.conn().execute(ddl, &[]).await?;
        }
        let schema = read(db, &[]).await.default_schema;
        let ask = async |p: Precheck| {
            let st = precheck_for(db.db_type(), &p, &schema);
            number(db.conn(), &st).await
        };
        let range = |min, max| Precheck::OutOfRange {
            table: q("r"),
            column: id("v"),
            min,
            max,
        };
        let non_null = Precheck::NonNull {
            table: q("two"),
            columns: vec![id("a"), id("b")],
        };
        let orphans = Precheck::Orphans {
            table: q("child"),
            columns: vec![id("x"), id("y")],
            target: q("parent"),
            target_columns: vec![id("a"), id("b")],
        };
        let dup = Precheck::Duplicates {
            table: q("d"),
            columns: vec![id("a"), id("b")],
        };
        let longer = |max_len| Precheck::LongerThan {
            table: q("s"),
            column: id("name"),
            max_len,
        };

        // the bounds themselves are allowed
        assert_eq!(ask(range(0, 10)).await?, 0, "values at the bounds");
        assert_eq!(ask(range(1, 10)).await?, 1, "below the min");
        assert_eq!(ask(range(0, 9)).await?, 1, "above the max");

        // the columns are ANDed: a row with only one of them set doesn't count
        assert_eq!(ask(non_null.clone()).await?, 0, "one of two columns set");
        db.conn()
            .execute("INSERT INTO two VALUES (3, 3)", &[])
            .await?;
        assert_eq!(ask(non_null).await?, 1, "both columns set");

        // a row with a NULL in the key is exempt (MATCH SIMPLE); (1, 1) has its parent
        assert_eq!(
            ask(orphans.clone()).await?,
            0,
            "partly NULL rows are exempt"
        );
        db.conn()
            .execute("INSERT INTO child VALUES (1, 9)", &[])
            .await?;
        assert_eq!(ask(orphans).await?, 1, "a real composite orphan");

        // GROUP BY puts NULLs together: a single row is no duplicate, two equal ones are
        assert_eq!(ask(dup.clone()).await?, 0, "one row with a NULL");
        db.conn()
            .execute("INSERT INTO d VALUES (NULL, 1)", &[])
            .await?;
        assert_eq!(
            ask(dup).await?,
            1,
            "rows equal but for NULLs are duplicates"
        );

        // trailing spaces: PostgreSQL and SQL Server trim them when a column shrinks,
        // MySQL refuses the change
        let trimmed = match db.db_type() {
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => 1,
            _ => 0,
        };
        assert_eq!(ask(longer(5)).await?, trimmed, "'abc   ' into 5");
        assert_eq!(ask(longer(2)).await?, 1, "'abc' does not fit in 2");
        Ok(())
    })
    .await
}
per_backend!(prechecks_edge_cases, super::prechecks_edge_cases);

// Gives the server a moment to notice a closed connection (it frees the lock a few ms
// later); `false` if it's still held after 2 s.
async fn free_soon(db: &TestDb) -> CanyonResult<bool> {
    let (lock, unlock) = lock_for(db.db_type());
    let other = db.conn().acquire().await?;
    for _ in 0..100 {
        if number(&other, &lock).await? == 1 {
            run_unlock(&other, &unlock).await?;
            other.release().await?;
            return Ok(true);
        }
        sleep(Duration::from_millis(20)).await;
    }
    other.release().await?;
    Ok(false)
}

pub(crate) async fn lock_is_exclusive_and_freed(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "lock", async |db| {
        let (lock, unlock) = lock_for(db.db_type());
        let first = db.conn().acquire().await?;
        let second = db.conn().acquire().await?;
        assert_eq!(number(&first, &lock).await?, 1, "free lock is taken");
        assert_eq!(
            number(&second, &lock).await?,
            0,
            "refused from a second session"
        );
        // unlock frees it at once, no waiting
        run_unlock(&first, &unlock).await?;
        assert_eq!(number(&second, &lock).await?, 1, "free right after unlock");
        run_unlock(&second, &unlock).await?;

        // the lock goes with the session
        assert_eq!(number(&first, &lock).await?, 1);
        first.release().await?;
        assert!(free_soon(db).await?, "lock still held after release");
        second.release().await?;
        Ok(())
    })
    .await
}
per_backend!(
    lock_is_exclusive_and_freed,
    super::lock_is_exclusive_and_freed
);

pub(crate) async fn dropped_session_frees_the_lock(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "lock_dropped", async |db| {
        let (lock, _) = lock_for(db.db_type());
        let session = db.conn().acquire().await?;
        assert_eq!(number(&session, &lock).await?, 1);
        drop(session);
        assert!(free_soon(db).await?, "lock still held after drop");
        Ok(())
    })
    .await
}
per_backend!(
    dropped_session_frees_the_lock,
    super::dropped_session_frees_the_lock
);

// GET_LOCK names are server-wide: the lock has to be per database
#[cfg(feature = "mysql")]
#[canyon_sql::macros::canyon_tokio_test]
fn mysql_locks_are_per_database() {
    let ds = crate::constants::MYSQL_DS;
    TestDb::with(ds, "lock_a", async |a| {
        TestDb::with(ds, "lock_b", async |b| {
            let (lock, unlock) = lock_for(a.db_type());
            let in_a = a.conn().acquire().await?;
            let in_b = b.conn().acquire().await?;
            let also_in_a = a.conn().acquire().await?;
            assert_eq!(number(&in_a, &lock).await?, 1);
            assert_eq!(
                number(&in_b, &lock).await?,
                1,
                "another database has its own lock"
            );
            assert_eq!(
                number(&also_in_a, &lock).await?,
                0,
                "same database is refused"
            );
            run_unlock(&in_a, &unlock).await?;
            run_unlock(&in_b, &unlock).await?;
            in_a.release().await?;
            in_b.release().await?;
            also_in_a.release().await?;
            Ok(())
        })
        .await
    })
    .await?;
}
