//! `Session`: one connection held across statements, with its own transaction.

use crate::support::per_backend;
use crate::support::test_db::TestDb;
use canyon_sql::CanyonResult;
use canyon_sql::connection::{DatabaseType, DbConnection};
use canyon_sql::runtime::tokio::time::sleep;
use std::time::Duration;

// COUNT(*) is BIGINT on PostgreSQL and MySQL, int on SQL Server
fn count_sql(db: DatabaseType) -> &'static str {
    match db {
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => "SELECT CAST(COUNT(*) AS BIGINT) FROM s",
        #[allow(unreachable_patterns)]
        _ => "SELECT COUNT(*) FROM s",
    }
}

// The server's id for the session the statement runs on
fn session_id_sql(db: DatabaseType) -> &'static str {
    match db {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => "SELECT pg_catalog.pg_backend_pid()::bigint",
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => "SELECT CAST(@@SPID AS BIGINT)",
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => "SELECT CAST(CONNECTION_ID() AS SIGNED)",
    }
}

// PostgreSQL / SQL Server: is the server session still there (it holds the
// transaction and its locks). MySQL keeps the pooled connection, so: does it
// still have an open InnoDB transaction
fn session_alive_sql(db: DatabaseType) -> &'static str {
    match db {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => {
            "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE pid::bigint = $1"
        }
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => {
            "SELECT CAST(COUNT(*) AS BIGINT) FROM sys.dm_exec_sessions WHERE session_id = @P1"
        }
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => {
            "SELECT COUNT(*) FROM information_schema.INNODB_TRX WHERE trx_mysql_thread_id = ?"
        }
    }
}

// Closing a connection ends the server session a few ms later: poll for it.
// `false` if it's still there after 2 s
async fn wait_until_gone(db: &TestDb, id: i64) -> CanyonResult<bool> {
    let sql = session_alive_sql(db.db_type());
    for _ in 0..100 {
        if db.conn().query_one_for::<i64>(sql, &[&id]).await? == 0 {
            return Ok(true);
        }
        sleep(Duration::from_millis(20)).await;
    }
    Ok(false)
}

async fn create_table(db: &TestDb) -> CanyonResult<()> {
    db.conn().execute("CREATE TABLE s (c INT)", &[]).await?;
    Ok(())
}

pub(crate) async fn rollback_undoes_writes(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "session_rollback", async |db| {
        create_table(db).await?;
        let count = count_sql(db.db_type());
        let session = db.conn().acquire().await?;
        let transactional_ddl = match db.db_type() {
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => false,
            #[allow(unreachable_patterns)]
            _ => true,
        };
        assert_eq!(session.transactional_ddl(), transactional_ddl);
        session.begin().await?;
        session.execute("INSERT INTO s (c) VALUES (1)", &[]).await?;
        assert_eq!(
            session.query_one_for::<i64>(count, &[]).await?,
            1,
            "the session sees its own write"
        );
        session.rollback().await?;
        assert_eq!(session.query_one_for::<i64>(count, &[]).await?, 0);
        session.release().await?;
        // DML rolls back on MySQL too, only DDL commits on its own there
        assert_eq!(db.conn().query_one_for::<i64>(count, &[]).await?, 0);
        Ok(())
    })
    .await
}
per_backend!(
    session_rollback_undoes_writes,
    super::rollback_undoes_writes
);

pub(crate) async fn commit_keeps_writes(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "session_commit", async |db| {
        create_table(db).await?;
        let session = db.conn().acquire().await?;
        session.begin().await?;
        session.execute("INSERT INTO s (c) VALUES (1)", &[]).await?;
        session.commit().await?;
        session.release().await?;
        let after = db
            .conn()
            .query_one_for::<i64>(count_sql(db.db_type()), &[])
            .await?;
        assert_eq!(after, 1);
        Ok(())
    })
    .await
}
per_backend!(session_commit_keeps_writes, super::commit_keeps_writes);

pub(crate) async fn release_rolls_back_and_closes(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "session_release", async |db| {
        create_table(db).await?;
        let session = db.conn().acquire().await?;
        let id = session
            .query_one_for::<i64>(session_id_sql(db.db_type()), &[])
            .await?;
        session.begin().await?;
        session.execute("INSERT INTO s (c) VALUES (1)", &[]).await?;
        session.release().await?; // no commit: must roll back
        assert!(wait_until_gone(db, id).await?, "session {id} still open");
        let after = db
            .conn()
            .query_one_for::<i64>(count_sql(db.db_type()), &[])
            .await?;
        assert_eq!(after, 0);
        Ok(())
    })
    .await
}
per_backend!(
    session_release_rolls_back_and_closes,
    super::release_rolls_back_and_closes
);

pub(crate) async fn dropped_session_leaves_nothing(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "session_dropped", async |db| {
        create_table(db).await?;
        let session = db.conn().acquire().await?;
        let id = session
            .query_one_for::<i64>(session_id_sql(db.db_type()), &[])
            .await?;
        session.begin().await?;
        session.execute("INSERT INTO s (c) VALUES (1)", &[]).await?;
        // A cancelled or panicking run drops the session without `release`
        drop(session);
        assert!(wait_until_gone(db, id).await?, "session {id} still open");
        let after = db
            .conn()
            .query_one_for::<i64>(count_sql(db.db_type()), &[])
            .await?;
        assert_eq!(after, 0);
        Ok(())
    })
    .await
}
per_backend!(
    session_dropped_session_leaves_nothing,
    super::dropped_session_leaves_nothing
);
