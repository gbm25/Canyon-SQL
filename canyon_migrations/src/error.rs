//! Every way a migration run can fail, with the text the user reads.

use crate::dialect::LowerError;
use crate::introspect::IntrospectError;
use crate::memory::SnapshotError;
use crate::planning::{Conflict, ConflictReport};
use crate::report::DatasourceReport;
use canyon_core::error::CanyonError;
use std::fmt;

#[derive(Debug)]
#[non_exhaustive]
pub enum MigrationError {
    /// Per datasource. Nothing was applied.
    Conflicts(Vec<(String, Vec<Conflict>)>),
    // Boxed: the driver errors are large, and every `Result` carries this type
    Database {
        datasource: String,
        stage: &'static str,
        source: Box<CanyonError>,
    },
    Introspection {
        datasource: String,
        source: Box<IntrospectError>,
    },
    LockTimeout {
        datasource: String,
        waited_secs: u64,
    },
    Snapshot {
        datasource: String,
        source: SnapshotError,
    },
    Apply {
        datasource: String,
        op: String,
        statement: String,
        source: Box<CanyonError>,
        /// The plan ran inside a transaction (false for MySQL).
        transactional: bool,
        /// The transaction was rolled back. Only meaningful when `transactional`.
        rolled_back: bool,
    },
    Lower {
        datasource: String,
        source: LowerError,
    },
    /// The plan ran but its COMMIT failed: whether the database kept it isn't known.
    Commit {
        datasource: String,
        source: Box<CanyonError>,
    },
    /// A datasource `canyon.toml` sets up in a way the run can't use: no name, a name the
    /// history can't store, no connection, or the same database as another datasource.
    Config { datasource: String, detail: String },
    /// `migrate` was stopped before anything ran.
    Aborted,
    /// A datasource failed after others were applied: those stay applied, there is no
    /// transaction across databases. The ones after the failed one were not attempted.
    PartlyApplied {
        applied: Vec<DatasourceReport>,
        not_attempted: Vec<String>,
        error: Box<MigrationError>,
    },
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflicts(per_datasource) => {
                for (i, (datasource, conflicts)) in per_datasource.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(
                        f,
                        "{}",
                        ConflictReport {
                            datasource,
                            conflicts
                        }
                    )?;
                }
                Ok(())
            }
            Self::Database {
                datasource,
                stage,
                source,
            } => write!(
                f,
                "Migrations for datasource `{datasource}` failed while {stage}: {}. Nothing was applied. Check that the database is reachable and the user can run the query, then run again.",
                chain(&**source)
            ),
            Self::Introspection { datasource, source } => write!(
                f,
                "Migrations for datasource `{datasource}` stopped: {}. Nothing was applied. Check that the database is reachable and the user can read its schema, then run again.",
                chain(&**source)
            ),
            Self::LockTimeout {
                datasource,
                waited_secs,
            } => write!(
                f,
                "Another migration is running on `{datasource}`, waited {waited_secs} s. Try again later or raise `lock_timeout_secs`."
            ),
            Self::Snapshot { datasource, source } => {
                write!(
                    f,
                    "Migrations for datasource `{datasource}` failed: {source}."
                )?;
                match source {
                    SnapshotError::Unreadable { .. } => write!(
                        f,
                        " The entities should never hold values read from a database, so this is a Canyon bug: report it with the entity named above. Nothing was applied."
                    ),
                    SnapshotError::UnknownVersion { .. } => write!(
                        f,
                        " It was written by another Canyon version. Run that version, or update Canyon. Nothing was applied."
                    ),
                    _ => write!(
                        f,
                        " It was edited by hand or written by another Canyon version. Run the version that wrote it, or restore the latest row of `canyon_schema_snapshot`. Nothing was applied."
                    ),
                }
            }
            Self::Apply {
                datasource,
                op,
                statement,
                source,
                transactional,
                rolled_back,
            } => write!(
                f,
                "Migrations for datasource `{datasource}` failed at \"{op}\": {}.\n  Statement: {statement}\n  {}",
                chain(&**source),
                match (*transactional, *rolled_back) {
                    (true, true) => format!("`{datasource}` was rolled back."),
                    (true, false) =>
                        "The rollback failed too: check the database before the next run.".to_owned(),
                    (false, _) =>
                        "Statements before this one stay applied (MySQL can't roll back schema changes); the next run continues from here.".to_owned(),
                }
            ),
            Self::Commit { datasource, source } => write!(
                f,
                "Migrations for datasource `{datasource}` ran, but committing them failed: {}. The outcome is unknown: check the database and `canyon_migration_history` before the next run.",
                chain(&**source)
            ),
            Self::Lower { datasource, source } => write!(
                f,
                "Migrations for datasource `{datasource}` can't write the SQL: {source}. Nothing was applied. {}",
                lower_advice(source)
            ),
            Self::Config { datasource, detail } => write!(
                f,
                "Migrations for datasource `{datasource}` can't start: {}. Check the datasource in `canyon.toml`.",
                trim_stop(detail)
            ),
            Self::Aborted => write!(
                f,
                "Migrations stopped before anything ran: nothing was applied. Run `migrate` again to answer the questions."
            ),
            Self::PartlyApplied {
                applied,
                not_attempted,
                error,
            } => {
                write!(f, "{error}")?;
                // A datasource with nothing to run changed nothing worth naming
                let done: Vec<String> = (applied.iter())
                    .filter(|r| r.statements > 0)
                    .map(|r| {
                        let n = r.statements;
                        format!(
                            "`{}` ({n} statement{})",
                            r.datasource,
                            if n == 1 { "" } else { "s" }
                        )
                    })
                    .collect();
                if !done.is_empty() {
                    write!(
                        f,
                        "\n  Already applied and kept (each database commits on its own): {}.",
                        done.join(", ")
                    )?;
                }
                if !not_attempted.is_empty() {
                    let names: Vec<String> =
                        not_attempted.iter().map(|n| format!("`{n}`")).collect();
                    write!(
                        f,
                        "\n  {} {} not attempted.",
                        names.join(", "),
                        if names.len() == 1 { "was" } else { "were" }
                    )?;
                }
                write!(f, "\n  The next run continues with the rest.")
            }
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database { source, .. }
            | Self::Apply { source, .. }
            | Self::Commit { source, .. } => Some(&**source),
            Self::Introspection { source, .. } => Some(&**source),
            Self::Snapshot { source, .. } => Some(source),
            Self::Lower { source, .. } => Some(source),
            Self::PartlyApplied { error, .. } => Some(&**error),
            Self::Conflicts(_) | Self::LockTimeout { .. } | Self::Config { .. } | Self::Aborted => {
                None
            }
        }
    }
}

fn lower_advice(error: &LowerError) -> &'static str {
    match error {
        LowerError::GeneratedColumn(_) => {
            "Change the column by hand, or make the field match the database."
        }
        LowerError::DesiredTypeUnknown(_) => "Use a field type Canyon supports.",
        LowerError::UnsupportedType(_) => {
            "Create or change that column by hand, or use another field type."
        }
        _ => "This is a Canyon bug: report it with the entities involved.",
    }
}

/// One closing "." less, so the message can add its own.
fn trim_stop(text: &str) -> &str {
    text.strip_suffix('.').unwrap_or(text)
}

/// The error and every `source()` below it, joined with ": ". Canyon's own errors only say
/// "SQL Server query failed"; the driver's reason is further down the chain.
pub(crate) fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut next = error.source();
    while let Some(e) = next {
        let part = e.to_string();
        if !text.ends_with(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        next = e.source();
    }
    trim_stop(&text).to_string()
}

/// What the history table keeps of a failed statement: Canyon's own message and the
/// database's error code, never the server's text (it can quote row values). Only
/// Canyon's message when the database didn't answer with an error.
pub(crate) fn history_error(error: &(dyn std::error::Error + 'static)) -> String {
    let own = error.to_string();
    let own = trim_stop(&own);
    match server_code(error) {
        Some(code) => format!("{own} ({code})"),
        None => own.to_owned(),
    }
}

/// The code of the first server error down the chain.
fn server_code(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut next = Some(error);
    while let Some(e) = next {
        #[cfg(feature = "postgres")]
        if let Some(db) = (e.downcast_ref::<tokio_postgres::Error>()).and_then(|e| e.as_db_error())
        {
            return Some(format!("SQLSTATE {}", db.code().code()));
        }
        #[cfg(feature = "mssql")]
        if let Some(tiberius::error::Error::Server(t)) = e.downcast_ref() {
            return Some(format!("SQL Server error {}", t.code()));
        }
        #[cfg(feature = "mysql")]
        if let Some(mysql_async::Error::Server(m)) = e.downcast_ref() {
            return Some(format!("MySQL error {}", m.code));
        }
        next = e.source();
    }
    None
}

impl From<MigrationError> for CanyonError {
    fn from(error: MigrationError) -> Self {
        CanyonError::Migration(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planning::test_support::q;
    use canyon_core::error::{ConfigurationError, QueryError};
    use std::error::Error;

    /// An error with a reason further down its chain.
    fn driver_error() -> CanyonError {
        ConfigurationError::Read {
            path: "canyon.toml".into(),
            source: std::io::Error::other("permission denied"),
        }
        .into()
    }

    fn all() -> Vec<MigrationError> {
        let ds = || "postgres_docker".to_string();
        vec![
            MigrationError::Conflicts(vec![
                (
                    ds(),
                    vec![Conflict::RenameTargetTaken {
                        old: q("gamer"),
                        new: q("player"),
                    }],
                ),
                (
                    "mysql_docker".into(),
                    vec![Conflict::RenameCycle {
                        tables: vec![q("a"), q("b")],
                    }],
                ),
            ]),
            MigrationError::Database {
                datasource: ds(),
                stage: "checking the data",
                source: Box::new(QueryError::NoRows.into()),
            },
            MigrationError::Introspection {
                datasource: ds(),
                source: Box::new(IntrospectError::Query {
                    stage: "columns",
                    source: driver_error(),
                }),
            },
            MigrationError::LockTimeout {
                datasource: ds(),
                waited_secs: 30,
            },
            MigrationError::Snapshot {
                datasource: ds(),
                source: SnapshotError::UnknownVersion { found: 9 },
            },
            MigrationError::Snapshot {
                datasource: ds(),
                source: SnapshotError::Unreadable {
                    table: "player".into(),
                    column: "age".into(),
                    what: "a default read from the database",
                },
            },
            MigrationError::Apply {
                datasource: ds(),
                op: "alter column player.age".into(),
                statement: "ALTER TABLE \"player\" ALTER COLUMN \"age\" TYPE bigint".into(),
                source: Box::new(driver_error()),
                transactional: true,
                rolled_back: true,
            },
            MigrationError::Apply {
                datasource: ds(),
                op: "alter column player.age".into(),
                statement: "ALTER TABLE \"player\" ALTER COLUMN \"age\" TYPE bigint".into(),
                source: Box::new(driver_error()),
                transactional: true,
                rolled_back: false,
            },
            MigrationError::Apply {
                datasource: "mysql_docker".into(),
                op: "add column player.age".into(),
                statement: "ALTER TABLE `player` ADD COLUMN `age` int".into(),
                source: Box::new(driver_error()),
                transactional: false,
                rolled_back: false,
            },
            MigrationError::Commit {
                datasource: ds(),
                source: Box::new(driver_error()),
            },
            MigrationError::Lower {
                datasource: ds(),
                source: LowerError::GeneratedColumn("total".into()),
            },
            MigrationError::Config {
                datasource: ds(),
                detail: "the datasource has no connection".into(),
            },
            MigrationError::Aborted,
            MigrationError::PartlyApplied {
                applied: vec![DatasourceReport {
                    datasource: ds(),
                    run_id: "r".into(),
                    statements: 2,
                    duration: std::time::Duration::ZERO,
                    infos: Vec::new(),
                    warnings: Vec::new(),
                }],
                not_attempted: vec!["sqlserver_docker".into()],
                error: Box::new(MigrationError::Apply {
                    datasource: "mysql_docker".into(),
                    op: "add column player.age".into(),
                    statement: "ALTER TABLE `player` ADD COLUMN `age` int".into(),
                    source: Box::new(driver_error()),
                    transactional: false,
                    rolled_back: false,
                }),
            },
        ]
    }

    #[test]
    fn messages() {
        let text: Vec<String> = all().iter().map(ToString::to_string).collect();
        insta::assert_snapshot!(text.join("\n---\n"));
    }

    #[test]
    fn source_chains() {
        for error in all() {
            let has_source = matches!(
                error,
                MigrationError::Database { .. }
                    | MigrationError::Introspection { .. }
                    | MigrationError::Snapshot { .. }
                    | MigrationError::Apply { .. }
                    | MigrationError::Commit { .. }
                    | MigrationError::Lower { .. }
                    | MigrationError::PartlyApplied { .. }
            );
            assert_eq!(error.source().is_some(), has_source, "{error:?}");
        }
        // The driver's reason, two levels down, reaches the message once.
        let text = all()[6].to_string();
        assert_eq!(text.matches("permission denied").count(), 1, "{text}");
    }

    #[test]
    fn no_doubled_stop() {
        let error = MigrationError::Config {
            datasource: "x".into(),
            detail: "no connection.".into(),
        };
        assert!(!error.to_string().contains(".."), "{error}");
        let driver = std::io::Error::other("lost connection.");
        assert_eq!(chain(&driver), "lost connection");
    }

    #[test]
    fn converts_to_canyon_error() {
        let error: CanyonError = MigrationError::LockTimeout {
            datasource: "x".into(),
            waited_secs: 1,
        }
        .into();
        assert!(matches!(error, CanyonError::Migration(_)));
        assert!(error.to_string().starts_with("Another migration"));
    }

    #[test]
    fn history_error_without_a_server_error_is_canyons_message_only() {
        let source = driver_error();
        assert_eq!(
            history_error(&source),
            r#"failed to read Canyon configuration at "canyon.toml""#
        );
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn history_error_keeps_the_mysql_code_not_the_text() {
        let server = mysql_async::Error::Server(mysql_async::ServerError {
            code: 1062,
            message: "Duplicate entry 'alice@example.com' for key 'PRIMARY'".into(),
            state: "23000".into(),
        });
        let source: CanyonError = QueryError::mysql(server).into();
        let stored = history_error(&source);
        assert_eq!(stored, "MySQL query failed (MySQL error 1062)");
        assert!(!stored.contains("alice"), "{stored}");
        // The returned error keeps the whole text
        assert!(chain(&source).contains("alice@example.com"));
    }

    #[cfg(feature = "mssql")]
    #[test]
    fn history_error_of_a_driver_failure_has_no_code() {
        let source: CanyonError =
            QueryError::sql_server(tiberius::error::Error::Protocol("bad packet".into())).into();
        assert_eq!(history_error(&source), "SQL Server query failed");
    }
}
