use crate::connection::clients::mssql::sqlserver_query_launcher::{
    execute_query, generate_mssql_query_client,
};
use crate::connection::contracts::DbConnection;
use crate::connection::database_type::DatabaseType;
use crate::connection::datasources::DatasourceConfig;
#[cfg(any(feature = "postgres", feature = "mysql"))]
use crate::error::ConfigurationError;
use crate::error::{CanyonResult, ConnectionError, QueryError};
use crate::mapper::RowMapper;
use crate::query::parameters::QueryParameter;
use crate::rows::{CanyonRows, FromSqlOwnedValue};
use bb8::PooledConnection;
use bb8_tiberius::ConnectionManager as TiberiusConnectionManager;
use std::sync::Arc;
use std::time::Duration;
use tiberius::Query;

type SqlServerConnectionPool = Arc<bb8::Pool<TiberiusConnectionManager>>;

/// How long a dedicated connection may take to connect and log in. It doesn't go
/// through the pool, so the pool's connection timeout doesn't apply.
const DEDICATED_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A connector for a `SqlServer` database
pub struct SqlServerConnector(SqlServerConnectionPool);

impl SqlServerConnector {
    pub async fn new(config: &DatasourceConfig) -> CanyonResult<Self> {
        Ok(Self(__impl::create_sqlserver_connector(config).await?))
    }
    pub async fn get_pooled(
        &self,
    ) -> CanyonResult<PooledConnection<'_, TiberiusConnectionManager>> {
        self.0
            .get()
            .await
            .map_err(|source| ConnectionError::sql_server_pool(source).into())
    }

    /// A new connection outside the pool: dropping it ends the server session. Gives up
    /// with a pool timeout error after [`DEDICATED_CONNECT_TIMEOUT`].
    pub(crate) async fn dedicated(&self) -> CanyonResult<bb8_tiberius::rt::Client> {
        match tokio::time::timeout(DEDICATED_CONNECT_TIMEOUT, self.0.dedicated_connection()).await {
            Ok(conn) => conn.map_err(|source| ConnectionError::sql_server_manager(source).into()),
            Err(_) => Err(ConnectionError::sql_server_pool(bb8::RunError::TimedOut).into()),
        }
    }
}

impl DbConnection for SqlServerConnector {
    async fn query_rows(
        &self,
        stmt: &str,
        params: &[&'_ dyn QueryParameter],
    ) -> CanyonResult<CanyonRows> {
        let mut conn = self.get_pooled().await?;
        ms_ops::query_rows(&mut conn, stmt, params).await
    }

    async fn query<S, R>(&self, stmt: S, params: &[&'_ dyn QueryParameter]) -> CanyonResult<Vec<R>>
    where
        S: AsRef<str> + Send,
        R: RowMapper,
        Vec<R>: FromIterator<<R as RowMapper>::Output>,
    {
        let mut conn = self.get_pooled().await?;
        Ok(ms_ops::query::<R>(&mut conn, stmt.as_ref(), params)
            .await?
            .into_iter()
            .collect())
    }

    async fn query_one<R>(
        &self,
        stmt: &str,
        params: &[&'_ dyn QueryParameter],
    ) -> CanyonResult<Option<R::Output>>
    where
        R: RowMapper,
    {
        let mut conn = self.get_pooled().await?;
        ms_ops::query_one::<R>(&mut conn, stmt, params).await
    }

    async fn query_one_for<T: FromSqlOwnedValue>(
        &self,
        stmt: &str,
        params: &[&'_ dyn QueryParameter],
    ) -> CanyonResult<T> {
        let mut conn = self.get_pooled().await?;
        ms_ops::query_one_for::<T>(&mut conn, stmt, params).await
    }

    async fn execute(&self, stmt: &str, params: &[&'_ dyn QueryParameter]) -> CanyonResult<u64> {
        let mut conn = self.get_pooled().await?;
        ms_ops::execute(&mut conn, stmt, params).await
    }

    fn get_database_type(&self) -> CanyonResult<DatabaseType> {
        Ok(DatabaseType::SqlServer)
    }
}

/// The client operations over a single connection, so a caller holding one
/// connection can run them without going through the pool.
pub(crate) mod ms_ops {
    use super::*;

    pub(crate) async fn query_rows(
        conn: &mut bb8_tiberius::rt::Client,
        stmt: &str,
        params: &[&dyn QueryParameter],
    ) -> CanyonResult<CanyonRows> {
        let result = execute_query(stmt, params, conn)
            .await?
            .into_results()
            .await
            .map_err(QueryError::sql_server)?
            .into_iter()
            .flatten()
            .collect();

        Ok(CanyonRows::Tiberius(result))
    }

    pub(crate) async fn query<R: RowMapper>(
        conn: &mut bb8_tiberius::rt::Client,
        stmt: &str,
        params: &[&dyn QueryParameter],
    ) -> CanyonResult<Vec<R::Output>> {
        execute_query(stmt, params, conn)
            .await?
            .into_results()
            .await
            .map_err(QueryError::sql_server)?
            .into_iter()
            .flatten()
            .map(|row| R::deserialize_sqlserver(&row))
            .collect()
    }

    pub(crate) async fn query_one<R: RowMapper>(
        conn: &mut bb8_tiberius::rt::Client,
        stmt: &str,
        params: &[&dyn QueryParameter],
    ) -> CanyonResult<Option<R::Output>> {
        let result = execute_query(stmt, params, conn)
            .await?
            .into_row()
            .await
            .map_err(QueryError::sql_server)?;

        match result {
            Some(r) => Ok(Some(R::deserialize_sqlserver(&r)?)),
            None => Ok(None),
        }
    }

    pub(crate) async fn query_one_for<T: FromSqlOwnedValue>(
        conn: &mut bb8_tiberius::rt::Client,
        stmt: &str,
        params: &[&dyn QueryParameter],
    ) -> CanyonResult<T> {
        let row = execute_query(stmt, params, conn)
            .await?
            .into_row()
            .await
            .map_err(QueryError::sql_server)?
            .ok_or(QueryError::NoRows)?;

        row.into_iter()
            .next()
            .ok_or(QueryError::NoColumns)
            .and_then(|value| {
                T::from_sql_owned(value)
                    .map_err(QueryError::sql_server)
                    .and_then(|value| value.ok_or(QueryError::UnexpectedNull))
            })
            .map_err(Into::into)
    }

    pub(crate) async fn execute(
        conn: &mut bb8_tiberius::rt::Client,
        stmt: &str,
        params: &[&dyn QueryParameter],
    ) -> CanyonResult<u64> {
        generate_mssql_query_client(stmt, params)
            .execute(conn)
            .await
            .map(|r| r.total())
            .map_err(|source| QueryError::sql_server(source).into())
    }
}

pub(crate) mod sqlserver_query_launcher {
    use super::*;
    use tiberius::QueryStream;

    pub(crate) async fn execute_query<'a>(
        stmt: &str,
        params: &[&dyn QueryParameter],
        conn: &'a mut bb8_tiberius::rt::Client,
    ) -> CanyonResult<QueryStream<'a>> {
        let mssql_query = generate_mssql_query_client(stmt, params);
        mssql_query
            .query(conn)
            .await
            .map_err(|source| QueryError::sql_server(source).into())
    }

    /// Builds a Tiberius query without modifying the supplied SQL.
    ///
    /// Statements produced by Canyon's query builder already use SQL Server's
    /// identifier, placeholder and `OUTPUT INSERTED` syntax. Callers providing
    /// raw statements are likewise responsible for supplying SQL Server SQL.
    pub(crate) fn generate_mssql_query_client<'a>(
        stmt: &str,
        params: &[&'a dyn QueryParameter],
    ) -> Query<'a> {
        generate_query_and_bind_params(stmt.to_owned(), params)
    }

    // Query and parameters are generated in this procedure together to avoid lifetime errors
    fn generate_query_and_bind_params<'a>(
        stmt: String,
        params: &[&'a (dyn QueryParameter + 'a)],
    ) -> Query<'a> {
        let mut mssql_query = Query::new(stmt);
        params.iter().for_each(|param| {
            mssql_query.bind(*param);
        });
        mssql_query
    }
}

pub(crate) mod __impl {
    use super::*;
    use crate::connection::datasources::{Auth, SqlServerAuth, SqlServerTlsMode};
    use bb8::Pool;
    use std::sync::Arc;
    use tiberius::{Config, EncryptionLevel};

    pub(crate) async fn create_sqlserver_connector(
        datasource: &DatasourceConfig,
    ) -> CanyonResult<Arc<Pool<TiberiusConnectionManager>>> {
        let sqlserver_config = sqlserver_config_from_datasource(datasource)?;
        let pool_config = datasource.pool_config()?;

        let manager = TiberiusConnectionManager::new(sqlserver_config);
        let pool = bb8::Pool::builder()
            .min_idle(pool_config.min_size)
            .max_size(pool_config.max_size)
            .build(manager)
            .await
            .map_err(ConnectionError::sql_server_manager)?;

        Ok(SqlServerConnectionPool::from(pool))
    }

    pub(crate) fn sqlserver_config_from_datasource(
        datasource: &DatasourceConfig,
    ) -> CanyonResult<Config> {
        let mut tiberius_config = tiberius::Config::new();

        tiberius_config.host(&datasource.properties.host);
        tiberius_config.port(datasource.get_port_or_default_by_db());
        tiberius_config.database(&datasource.properties.db_name);

        let auth_config = extract_mssql_auth(&datasource.auth)?;
        tiberius_config.authentication(auth_config);

        let (encryption, trust_server_certificate) =
            sqlserver_tls_options(datasource.properties.mssql_tls);
        tiberius_config.encryption(encryption);
        if trust_server_certificate {
            tiberius_config.trust_cert();
        }

        Ok(tiberius_config)
    }

    pub(crate) const fn sqlserver_tls_options(mode: SqlServerTlsMode) -> (EncryptionLevel, bool) {
        match mode {
            SqlServerTlsMode::Required => (EncryptionLevel::Required, false),
            SqlServerTlsMode::TrustServerCertificate => (EncryptionLevel::Required, true),
            SqlServerTlsMode::Disabled => (EncryptionLevel::NotSupported, false),
        }
    }

    pub(crate) fn extract_mssql_auth(auth: &Auth) -> CanyonResult<tiberius::AuthMethod> {
        match auth {
            Auth::SqlServer(sql_server_auth) => match sql_server_auth {
                SqlServerAuth::Basic { username, password } => {
                    Ok(tiberius::AuthMethod::sql_server(username, password))
                }
            },
            #[cfg(any(feature = "postgres", feature = "mysql"))]
            _ => Err(ConfigurationError::InvalidAuthentication {
                backend: DatabaseType::SqlServer,
            }
            .into()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::__impl;
    use crate::connection::datasources::{
        Auth, DatasourceConfig, DatasourceProperties, SqlServerAuth, SqlServerTlsMode,
    };
    use tiberius::{AuthMethod, EncryptionLevel};

    #[test]
    fn test_extract_mssql_auth_basic() {
        let auth = Auth::SqlServer(SqlServerAuth::Basic {
            username: "sa".to_string(),
            password: "password123".to_string(),
        });

        let result = __impl::extract_mssql_auth(&auth).unwrap();

        match result {
            // We can only check the variant, not its internals (private fields)
            AuthMethod::SqlServer(_) => {} // success
            _ => panic!("Expected AuthMethod::SqlServer variant"),
        }
    }

    #[test]
    fn test_sqlserver_config_from_datasource_basic() {
        let datasource = DatasourceConfig {
            name: "test_source".into(),
            properties: DatasourceProperties {
                host: "localhost".into(),
                db_name: "test_db".into(),
                port: None, // default
                migrations: None,
                pool: Default::default(),
                mssql_tls: SqlServerTlsMode::Required,
            },
            auth: Auth::SqlServer(SqlServerAuth::Basic {
                username: "sa".into(),
                password: "pass123".into(),
            }),
        };

        let config = __impl::sqlserver_config_from_datasource(&datasource).unwrap();
        assert_eq!(config.get_addr(), "localhost:1433");
    }

    #[test]
    fn sqlserver_tls_modes_map_to_secure_explicit_options() {
        assert_eq!(
            __impl::sqlserver_tls_options(SqlServerTlsMode::Required),
            (EncryptionLevel::Required, false)
        );
        assert_eq!(
            __impl::sqlserver_tls_options(SqlServerTlsMode::TrustServerCertificate),
            (EncryptionLevel::Required, true)
        );
        assert_eq!(
            __impl::sqlserver_tls_options(SqlServerTlsMode::Disabled),
            (EncryptionLevel::NotSupported, false)
        );
    }
}
