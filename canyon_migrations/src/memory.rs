//! Canyon's own tables: the schema snapshot and the migration history, and the
//! `canyon_memory` table of the old engine, which is only read.
//!
//! The snapshot holds the model the code asked for, never these tables.

use crate::dialect::{DdlDialect, DdlWriter, SNAPSHOT_TABLE};
use crate::introspect::CANYON_TABLES;
use crate::model::{
    ColumnDefault, Generated, Ident, Logical, ModelError, QualifiedName, SchemaModel, TableDef,
    TableDefBuilder,
};
use crate::op::{Param, Statement};
use crate::planning::LegacyRow;
use canyon_core::connection::contracts::DbConnection;
use canyon_core::error::CanyonResult;
use canyon_core::error::{CanyonError, MappingError};
use canyon_core::rows::CanyonRows;
use canyon_core::types::SqlType;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The layout of [`SnapshotDoc`]. Bump it when the JSON changes in a way old readers
/// can't take.
pub const SNAPSHOT_FORMAT_VERSION: i32 = 1;

pub(crate) const HISTORY_TABLE: &str = "canyon_migration_history";
/// The old engine's table, only read.
pub(crate) const LEGACY_TABLE: &str = "canyon_memory";
const OP_MAX_LEN: usize = 200;
/// The longest datasource name the history table takes.
pub(crate) const DATASOURCE_MAX_LEN: usize = 100;

/// The tables Canyon creates for itself, to be created through the engine like any other.
///
/// `canyon_migration_history` has a row per statement a run executed. On a failure its
/// `error` column keeps only Canyon's message and the database's error code (for example
/// `PostgreSQL query failed (SQLSTATE 23505)`), never the database's text, which may quote
/// row values. The full text is in the error the run returns.
/// # Errors
/// A definition the model refuses; the unit test builds them all.
pub fn canyon_tables() -> Result<Vec<TableDef>, ModelError> {
    use SqlType::*;
    let id = |b: TableDefBuilder| {
        b.column("id", BigInt, false)
            .primary_key(Some(Generated::Identity))
    };
    let text = |len| Text { max_len: len };
    Ok(vec![
        id(TableDef::builder("Canyon", SNAPSHOT_TABLE))
            .column("run_id", text(Some(36)), false)
            .column("created_at", Timestamp { precision: 6 }, false)
            .column("canyon_version", text(Some(20)), false)
            .column("format_version", Int, false)
            .column("model", text(None), false)
            .build()?,
        id(TableDef::builder("Canyon", HISTORY_TABLE))
            .column("run_id", text(Some(36)), false)
            .column("datasource", text(Some(DATASOURCE_MAX_LEN as u32)), false)
            .column("canyon_version", text(Some(20)), false)
            .column("started_at", Timestamp { precision: 6 }, false)
            .column("duration_ms", BigInt, false)
            .column("op", text(Some(OP_MAX_LEN as u32)), false)
            .column("statement", text(None), false)
            .column("status", text(Some(20)), false)
            .column("error", text(None), true)
            .build()?,
    ])
}

/// A column change the user said no to, so it isn't offered again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declined {
    pub table: QualifiedName,
    pub column: Ident,
    pub column_type: SqlType,
}

/// What a run leaves in `canyon_schema_snapshot.model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDoc {
    pub format_version: i32,
    pub tables: SchemaModel,
    #[serde(default)]
    pub declined_tighten: Vec<Declined>,
}

/// A snapshot that can't be used.
#[derive(Debug)]
#[non_exhaustive]
pub enum SnapshotError {
    /// Written by a version of Canyon with another layout.
    UnknownVersion { found: i32 },
    /// Not a snapshot document.
    Invalid(serde_json::Error),
    /// The model holds SQL text read from a database, which a snapshot never gives
    /// back: storing it would make every later read fail.
    Unreadable {
        table: String,
        column: String,
        what: &'static str,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownVersion { found } => write!(
                f,
                "the stored schema snapshot has format version {found}, this Canyon reads \
                 version {SNAPSHOT_FORMAT_VERSION}"
            ),
            Self::Unreadable {
                table,
                column,
                what,
            } => write!(
                f,
                "{table}.{column} has {what}, which a schema snapshot can't hold"
            ),
            Self::Invalid(source) => {
                write!(f, "the stored schema snapshot can't be read: {source}")
            }
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(source) => Some(source),
            Self::UnknownVersion { .. } | Self::Unreadable { .. } => None,
        }
    }
}

impl SnapshotDoc {
    /// Canyon's own tables are dropped from `tables`: the next run would see them as
    /// tables the user's model doesn't have.
    pub fn new(mut tables: SchemaModel, declined_tighten: Vec<Declined>) -> Self {
        tables.tables.retain(|name, _| {
            name.schema.is_some() || !CANYON_TABLES.contains(&name.name.as_str())
        });
        Self {
            format_version: SNAPSHOT_FORMAT_VERSION,
            tables,
            declined_tighten,
        }
    }

    /// Refuses a model with a column type, default, collation or `ON UPDATE` that came
    /// from a database: it serializes but never deserializes.
    ///
    /// # Errors
    /// [`SnapshotError::Unreadable`], naming the column.
    pub fn check_readable(&self) -> Result<(), SnapshotError> {
        for table in self.tables.tables.values() {
            for column in &table.columns {
                let what = if matches!(column.ty, Logical::Other(_)) {
                    Some("a type Canyon has no mapping for")
                } else if matches!(column.default, Some(ColumnDefault::Expr { .. })) {
                    Some("a default expression read from the database")
                } else if column.extra.collation.is_some() {
                    Some("a collation read from the database")
                } else if column.extra.on_update.is_some() {
                    Some("an ON UPDATE expression read from the database")
                } else {
                    None
                };
                if let Some(what) = what {
                    return Err(SnapshotError::Unreadable {
                        table: table.name.to_string(),
                        column: column.name.to_string(),
                        what,
                    });
                }
            }
        }
        Ok(())
    }

    /// Reads the stored JSON. The version is checked first, so a newer layout reports
    /// its version instead of a parse error.
    ///
    /// # Errors
    /// The text isn't a snapshot, or its version isn't [`SNAPSHOT_FORMAT_VERSION`].
    pub fn from_json(text: &str) -> Result<Self, SnapshotError> {
        #[derive(Deserialize)]
        struct Head {
            format_version: i32,
        }
        let head: Head = serde_json::from_str(text).map_err(SnapshotError::Invalid)?;
        if head.format_version != SNAPSHOT_FORMAT_VERSION {
            return Err(SnapshotError::UnknownVersion {
                found: head.format_version,
            });
        }
        serde_json::from_str(text).map_err(SnapshotError::Invalid)
    }
}

/// One statement a run executed (or failed to).
pub struct HistoryRow<'a> {
    pub run_id: &'a str,
    pub datasource: &'a str,
    pub started_at: NaiveDateTime,
    pub duration_ms: i64,
    pub op: &'a str,
    pub statement: &'a str,
    pub status: &'static str,
    pub error: Option<&'a str>,
}

fn table<D: DdlDialect>(w: &mut DdlWriter<D>, name: &'static str) {
    let name = QualifiedName {
        schema: None,
        name: Ident::from_catalog(name),
    };
    w.qualified(&name);
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Records one statement. The error column is left out when there is none, so it stays
/// NULL.
pub fn insert_history<D: DdlDialect>(row: &HistoryRow<'_>) -> Statement {
    let mut w = DdlWriter::<D>::new();
    w.raw("INSERT INTO ");
    table(&mut w, HISTORY_TABLE);
    w.raw(" (run_id, datasource, canyon_version, started_at, duration_ms, op, statement, status");
    if row.error.is_some() {
        w.raw(", error");
    }
    w.raw(") VALUES (")
        .param(Param::Text(row.run_id.into()))
        .raw(", ")
        .param(Param::Text(row.datasource.into()))
        .raw(", ")
        .param(Param::Text(env!("CARGO_PKG_VERSION").into()))
        .raw(", ")
        .param(Param::Timestamp(row.started_at))
        .raw(", ")
        .param(Param::BigInt(row.duration_ms))
        .raw(", ")
        .param(Param::Text(truncate(row.op, OP_MAX_LEN)))
        .raw(", ")
        .param(Param::Text(row.statement.into()))
        .raw(", ")
        .param(Param::Text(row.status.into()));
    if let Some(error) = row.error {
        w.raw(", ").param(Param::Text(error.into()));
    }
    w.raw(")");
    w.finish_with(true)
}

/// Stores `doc` as the newest snapshot.
///
/// # Errors
/// `doc` holds something a snapshot can't read back (a column type or default taken from
/// a database), or can't be serialized.
pub fn insert_snapshot<D: DdlDialect>(
    run_id: &str,
    doc: &SnapshotDoc,
) -> Result<Statement, SnapshotError> {
    doc.check_readable()?;
    let json = serde_json::to_string(doc).map_err(SnapshotError::Invalid)?;
    // What gets stored must come back
    SnapshotDoc::from_json(&json)?;
    let mut w = DdlWriter::<D>::new();
    w.raw("INSERT INTO ");
    table(&mut w, SNAPSHOT_TABLE);
    w.raw(" (run_id, created_at, canyon_version, format_version, model) VALUES (")
        .param(Param::Text(run_id.into()))
        .raw(", ")
        .param(Param::Timestamp(chrono::Utc::now().naive_utc()))
        .raw(", ")
        .param(Param::Text(env!("CARGO_PKG_VERSION").into()))
        .raw(", ")
        .param(Param::Int(doc.format_version))
        .raw(", ")
        .param(Param::Text(json))
        .raw(")");
    Ok(w.finish_with(true))
}

fn decode_error(detail: impl fmt::Display) -> CanyonError {
    MappingError::custom(format!("reading Canyon's tables failed: {detail}")).into()
}

/// One column of every row as text; NULL stays `None`.
fn text_column(rows: &CanyonRows, column: usize) -> CanyonResult<Vec<Option<String>>> {
    match rows {
        #[cfg(feature = "postgres")]
        CanyonRows::Postgres(rows) => rows
            .iter()
            .map(|r| {
                r.try_get::<usize, Option<String>>(column)
                    .map_err(decode_error)
            })
            .collect(),
        #[cfg(feature = "mssql")]
        CanyonRows::Tiberius(rows) => rows
            .iter()
            .map(|r| {
                r.try_get::<&str, _>(column)
                    .map(|v| v.map(str::to_owned))
                    .map_err(decode_error)
            })
            .collect(),
        #[cfg(feature = "mysql")]
        CanyonRows::MySQL(rows) => rows
            .iter()
            .map(|r| match r.get_opt::<Option<String>, usize>(column) {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => Err(decode_error(e)),
                None => Err(decode_error(format_args!("no column {column}"))),
            })
            .collect(),
    }
}

/// The JSON of the newest snapshot, `None` when there is none.
///
/// # Errors
/// The query fails.
pub async fn read_snapshot<D: DdlDialect>(
    conn: &impl DbConnection,
) -> CanyonResult<Option<String>> {
    let statement = D::latest_snapshot();
    let rows = conn
        .query_rows(&statement.sql, &statement.query_params())
        .await?;
    Ok(text_column(&rows, 0)?.into_iter().next().flatten())
}

/// The struct and table names the old engine kept in `canyon_memory`.
///
/// Call it only when `Catalog::canyon_tables` lists `canyon_memory`: with no such table
/// the query fails. MySQL's old engine never made the table, so there it answers empty.
///
/// # Errors
/// The query fails or a row can't be read.
pub async fn read_legacy(conn: &impl DbConnection) -> CanyonResult<Vec<LegacyRow>> {
    #[cfg(feature = "mysql")]
    if conn.get_database_type()? == canyon_core::connection::database_type::DatabaseType::MySQL {
        return Ok(Vec::new());
    }
    let rows = conn
        .query_rows(
            "SELECT struct_name, declared_table_name FROM canyon_memory ORDER BY id",
            &[],
        )
        .await?;
    let structs = text_column(&rows, 0)?;
    let tables = text_column(&rows, 1)?;
    Ok(structs
        .into_iter()
        .zip(tables)
        .filter_map(|(s, t)| {
            Some(LegacyRow {
                struct_name: s?,
                table: t?,
            })
        })
        .collect())
}

/// `<unix millis in hex>-<8 hex digits>`: sortable by time, and told apart from a run
/// in the same millisecond.
pub fn new_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let hash = std::collections::hash_map::RandomState::new().hash_one((n, millis));
    format!("{millis:x}-{:08x}", hash as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_that_cant_be_read_is_a_mapping_error() {
        let error = decode_error("no column 3");
        assert!(matches!(error, CanyonError::Mapping(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "reading Canyon's tables failed: no column 3"
        );
    }

    fn small_doc() -> SnapshotDoc {
        let table = TableDef::builder("Player", "player")
            .column("id", SqlType::Int, false)
            .primary_key(Some(Generated::Identity))
            .column("name", SqlType::Text { max_len: Some(20) }, true)
            .build()
            .expect("a valid table");
        let model = SchemaModel::from_tables(vec![table]).expect("a valid model");
        let declined = Declined {
            table: QualifiedName::parse("player").expect("a valid name"),
            column: Ident::new("name").expect("a valid name"),
            column_type: SqlType::Text { max_len: Some(10) },
        };
        SnapshotDoc::new(model, vec![declined])
    }

    #[test]
    fn canyon_tables_build() {
        let names: Vec<_> = canyon_tables()
            .expect("builds")
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            names,
            ["canyon_schema_snapshot", "canyon_migration_history"]
        );
        for name in &names {
            assert!(CANYON_TABLES.contains(&name.as_str()));
        }
    }

    #[test]
    fn snapshot_round_trips() {
        let doc = small_doc();
        let json = serde_json::to_string(&doc).expect("serializes");
        assert_eq!(SnapshotDoc::from_json(&json).expect("reads back"), doc);
    }

    // The stored layout: a rename here would make old snapshots unreadable
    #[test]
    fn snapshot_json_is_stable() {
        let json = serde_json::to_string(&small_doc()).expect("serializes");
        assert_eq!(json, GOLDEN);
        assert_eq!(
            SnapshotDoc::from_json(GOLDEN).expect("reads back"),
            small_doc()
        );
    }

    #[test]
    fn unknown_version_is_named() {
        let json = GOLDEN.replace("\"format_version\":1", "\"format_version\":9");
        let err = SnapshotDoc::from_json(&json).expect_err("a newer layout");
        assert!(matches!(err, SnapshotError::UnknownVersion { found: 9 }));
        assert!(err.to_string().contains("version 9"), "{err}");
        // even when the rest of it is of a layout this version can't read
        let err = SnapshotDoc::from_json("{\"format_version\":2,\"other\":true}")
            .expect_err("a newer layout");
        assert!(matches!(err, SnapshotError::UnknownVersion { found: 2 }));
    }

    #[test]
    fn invalid_names_and_documents_fail() {
        let json = GOLDEN.replace("\"name\":\"player\"", "\"name\":\"a-b\"");
        assert_ne!(json, GOLDEN);
        assert!(matches!(
            SnapshotDoc::from_json(&json),
            Err(SnapshotError::Invalid(_))
        ));
        assert!(matches!(
            SnapshotDoc::from_json("not json"),
            Err(SnapshotError::Invalid(_))
        ));
    }

    #[test]
    fn snapshot_never_holds_canyon_tables() {
        let mut tables = canyon_tables().expect("builds");
        tables.extend(small_doc().tables.tables.into_values());
        let model = SchemaModel::from_tables(tables).expect("a valid model");
        assert_eq!(model.tables.len(), 3);
        let doc = SnapshotDoc::new(model, Vec::new());
        assert_eq!(doc.tables.tables.len(), 1);
        let json = serde_json::to_string(&doc).expect("serializes");
        for name in CANYON_TABLES {
            assert!(!json.contains(name), "{name}");
        }
    }

    #[test]
    fn unreadable_models_are_refused() {
        use crate::model::NativeType;
        let mut doc = small_doc();
        let table = doc.tables.tables.values_mut().next().expect("a table");
        table.columns[1].ty = Logical::Other(NativeType::named("xml", "xml"));
        let err = doc.check_readable().expect_err("an Other column");
        assert!(err.to_string().contains("player.name"), "{err}");
        assert!(serde_json::to_string(&doc).is_ok(), "it does serialize");
        #[cfg(feature = "postgres")]
        {
            use canyon_core::query::querybuilder::syntax::dialect::PgDialect;
            assert!(matches!(
                insert_snapshot::<PgDialect>("run", &doc),
                Err(SnapshotError::Unreadable { .. })
            ));
        }
    }

    #[test]
    fn run_ids_fit_and_differ() {
        let (a, b) = (new_run_id(), new_run_id());
        assert!(a.len() <= 36 && b.len() <= 36);
        assert_ne!(a, b);
    }

    #[cfg(feature = "postgres")]
    mod pg {
        use super::*;
        use canyon_core::query::querybuilder::syntax::dialect::PgDialect;

        fn row<'a>(error: Option<&'a str>) -> HistoryRow<'a> {
            HistoryRow {
                run_id: "run",
                datasource: "ds",
                started_at: NaiveDateTime::default(),
                duration_ms: 12,
                op: "it's an op",
                statement: "DROP TABLE x; -- '",
                status: "ok",
                error,
            }
        }

        #[test]
        fn history_sql_has_only_placeholders() {
            let ok = insert_history::<PgDialect>(&row(None));
            let failed = insert_history::<PgDialect>(&row(Some("boom")));
            insta::assert_snapshot!(format!("{}\n{}", ok.sql, failed.sql));
            assert_eq!(ok.params.len(), 8);
            assert_eq!(failed.params.len(), 9);
        }

        #[test]
        fn long_ops_are_cut() {
            let long = "é".repeat(300);
            let mut r = row(None);
            r.op = &long;
            let st = insert_history::<PgDialect>(&r);
            assert_eq!(st.params[5], Param::Text("é".repeat(200)));
        }

        #[test]
        fn snapshot_sql_binds_the_document() {
            let st = insert_snapshot::<PgDialect>("run", &small_doc()).expect("serializes");
            assert!(!st.sql.contains("player"));
            assert!(st.params.contains(&Param::Text(GOLDEN.to_owned())));
        }
    }

    const GOLDEN: &str = r#"{"format_version":1,"tables":[{"entity":"Player","name":{"schema":null,"name":"player"},"columns":[{"name":"id","ty":{"Known":{"type":"int"}},"nullable":false,"default":null},{"name":"name","ty":{"Known":{"type":"text","max_len":20}},"nullable":true,"default":null}],"primary_key":{"name":null,"columns":["id"],"generated":"Identity"},"foreign_keys":[]}],"declined_tighten":[{"table":{"schema":null,"name":"player"},"column":"name","column_type":{"type":"text","max_len":10}}]}"#;
}
