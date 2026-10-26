//! One connection held for a whole unit of work (locks, transactions).
//!
//! On PostgreSQL and SQL Server the connection is opened outside the pool: dropping
//! the [`Session`] closes it, so the server ends the session and with it any open
//! transaction or session lock, also when the future is cancelled or panics. On
//! MySQL it comes from the pool, which resets it (rollback, locks released) when it
//! comes back.
//!
//! A statement still running when the session is dropped isn't cancelled on
//! PostgreSQL and MySQL: the server ends the session (or the pool resets it) only once
//! that statement finishes, and its locks are held until then. On MySQL a schema
//! change that finishes this way stays applied.

use crate::connection::contracts::DbConnection;
use crate::connection::database_type::DatabaseType;
use crate::connection::db_connector::DatabaseConnector;
use crate::error::CanyonResult;
use crate::mapper::RowMapper;
use crate::query::parameters::QueryParameter;
use crate::rows::{CanyonRows, FromSqlOwnedValue};

#[cfg(feature = "mssql")]
use crate::connection::clients::mssql::ms_ops;
#[cfg(feature = "mysql")]
use crate::connection::clients::mysql::my_ops;
#[cfg(feature = "postgres")]
use crate::connection::clients::postgresql::pg_ops;
#[cfg(any(feature = "postgres", feature = "mssql", feature = "mysql"))]
use crate::error::QueryError;
#[cfg(feature = "mysql")]
use mysql_async::prelude::Queryable;
#[cfg(any(feature = "mssql", feature = "mysql"))]
use tokio::sync::Mutex;

/// A connection of its own for statements that must run on the same server
/// session, like a lock and the transaction it guards.
#[doc(hidden)]
pub enum Session {
    #[cfg(feature = "postgres")]
    Postgres(tokio_postgres::Client),
    #[cfg(feature = "mssql")]
    SqlServer(Mutex<bb8_tiberius::rt::Client>),
    #[cfg(feature = "mysql")]
    MySQL(Mutex<mysql_async::Conn>),
}

impl DatabaseConnector {
    /// Opens a [`Session`] on this datasource.
    #[doc(hidden)]
    pub async fn acquire(&self) -> CanyonResult<Session> {
        Ok(match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => Session::Postgres(c.dedicated().await?),
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => Session::SqlServer(Mutex::new(c.dedicated().await?)),
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => Session::MySQL(Mutex::new(c.get_conn().await?)),
        })
    }
}

impl Session {
    /// Starts a transaction.
    pub async fn begin(&self) -> CanyonResult<()> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => pg_batch(c, "BEGIN").await,
            // An RPC `BEGIN TRANSACTION` fails and still leaves a transaction open:
            // transaction control on SQL Server only goes as plain batches
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => ms_batch(c, "SET XACT_ABORT ON; BEGIN TRANSACTION").await,
            // The prepared protocol rejects `START TRANSACTION` (error 1295)
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => my_text(c, "START TRANSACTION").await,
        }
    }

    /// Commits the open transaction.
    pub async fn commit(&self) -> CanyonResult<()> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => pg_batch(c, "COMMIT").await,
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => ms_batch(c, "COMMIT TRANSACTION").await,
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => my_text(c, "COMMIT").await,
        }
    }

    /// Rolls back the open transaction, if there is one.
    pub async fn rollback(&self) -> CanyonResult<()> {
        match self {
            // Outside a transaction PostgreSQL only warns
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => pg_batch(c, "ROLLBACK").await,
            // With XACT_ABORT ON a failed statement already rolled everything back
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => ms_batch(c, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await,
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => my_text(c, "ROLLBACK").await,
        }
    }

    /// Rolls back whatever is still open and closes the connection (MySQL: gives it
    /// back to the pool, without any user lock). The connection goes away even when
    /// the rollback fails; that error is returned.
    pub async fn release(self) -> CanyonResult<()> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => {
                let rolled_back = pg_batch(&c, "ROLLBACK").await;
                // Dropping the client ends the connection task and the server session
                drop(c);
                rolled_back
            }
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => {
                let rolled_back = ms_batch(&c, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
                let closed = c
                    .into_inner()
                    .close()
                    .await
                    .map_err(|source| QueryError::sql_server(source).into());
                rolled_back.and(closed)
            }
            // The pool resets the connection on return, but asynchronously: unlock now
            // so the next run doesn't wait for it
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => {
                let rolled_back = my_text(&c, "ROLLBACK").await;
                let unlocked = my_text(&c, "DO RELEASE_ALL_LOCKS()").await;
                rolled_back.and(unlocked)
            }
        }
    }

    /// `false` where schema changes commit on their own (MySQL), so a plan can't
    /// run inside one transaction there.
    pub fn transactional_ddl(&self) -> bool {
        match self {
            #[cfg(feature = "mysql")]
            Self::MySQL(_) => false,
            #[allow(unreachable_patterns)]
            _ => true,
        }
    }

    /// The backend this session is connected to.
    pub fn db_type(&self) -> DatabaseType {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => DatabaseType::PostgreSql,
            #[cfg(feature = "mssql")]
            Self::SqlServer(_) => DatabaseType::SqlServer,
            #[cfg(feature = "mysql")]
            Self::MySQL(_) => DatabaseType::MySQL,
        }
    }
}

#[cfg(feature = "postgres")]
async fn pg_batch(c: &tokio_postgres::Client, sql: &str) -> CanyonResult<()> {
    c.batch_execute(sql)
        .await
        .map_err(|source| QueryError::postgres(source).into())
}

#[cfg(feature = "mssql")]
async fn ms_batch(c: &Mutex<bb8_tiberius::rt::Client>, sql: &str) -> CanyonResult<()> {
    let mut c = c.lock().await;
    c.simple_query(sql)
        .await
        .map_err(QueryError::sql_server)?
        .into_results()
        .await
        .map_err(QueryError::sql_server)?;
    Ok(())
}

#[cfg(feature = "mysql")]
async fn my_text(c: &Mutex<mysql_async::Conn>, sql: &str) -> CanyonResult<()> {
    c.lock()
        .await
        .query_drop(sql)
        .await
        .map_err(|source| QueryError::mysql(source).into())
}

impl DbConnection for Session {
    async fn query_rows(
        &self,
        stmt: &str,
        params: &[&'_ dyn QueryParameter],
    ) -> CanyonResult<CanyonRows> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => pg_ops::query_rows(c, stmt, params).await,
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => ms_ops::query_rows(&mut *c.lock().await, stmt, params).await,
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => my_ops::query_rows(&mut *c.lock().await, stmt, params).await,
        }
    }

    async fn query<S, R>(&self, stmt: S, params: &[&'_ dyn QueryParameter]) -> CanyonResult<Vec<R>>
    where
        S: AsRef<str> + Send,
        R: RowMapper,
        Vec<R>: FromIterator<<R as RowMapper>::Output>,
    {
        let stmt = stmt.as_ref();
        let rows = match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => pg_ops::query::<R>(c, stmt, params).await?,
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => ms_ops::query::<R>(&mut *c.lock().await, stmt, params).await?,
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => my_ops::query::<R>(&mut *c.lock().await, stmt, params).await?,
        };
        Ok(rows.into_iter().collect())
    }

    async fn query_one<R>(
        &self,
        stmt: &str,
        params: &[&'_ dyn QueryParameter],
    ) -> CanyonResult<Option<R::Output>>
    where
        R: RowMapper,
    {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => pg_ops::query_one::<R>(c, stmt, params).await,
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => ms_ops::query_one::<R>(&mut *c.lock().await, stmt, params).await,
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => my_ops::query_one::<R>(&mut *c.lock().await, stmt, params).await,
        }
    }

    async fn query_one_for<T: FromSqlOwnedValue>(
        &self,
        stmt: &str,
        params: &[&'_ dyn QueryParameter],
    ) -> CanyonResult<T> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => pg_ops::query_one_for::<T>(c, stmt, params).await,
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => {
                ms_ops::query_one_for::<T>(&mut *c.lock().await, stmt, params).await
            }
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => my_ops::query_one_for::<T>(&mut *c.lock().await, stmt, params).await,
        }
    }

    async fn execute(&self, stmt: &str, params: &[&'_ dyn QueryParameter]) -> CanyonResult<u64> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(c) => pg_ops::execute(c, stmt, params).await,
            #[cfg(feature = "mssql")]
            Self::SqlServer(c) => ms_ops::execute(&mut *c.lock().await, stmt, params).await,
            #[cfg(feature = "mysql")]
            Self::MySQL(c) => my_ops::execute(&mut *c.lock().await, stmt, params).await,
        }
    }

    fn get_database_type(&self) -> CanyonResult<DatabaseType> {
        Ok(self.db_type())
    }
}

#[cfg(test)]
mod tests {
    use super::Session;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn session_is_send_and_sync() {
        assert_send_sync::<Session>();
    }
}
