//! Native SQL for each backend: types both ways and DDL for every [`crate::op::Op`].

mod naming;
mod writer;

#[cfg(feature = "mssql")]
mod mssql;
#[cfg(feature = "mysql")]
pub(crate) mod mysql;
#[cfg(feature = "postgres")]
mod pg;

#[cfg(test)]
pub(crate) mod fixtures;

pub use naming::{fk_name, pk_name};
pub use writer::{DdlWriter, TypeSql};

use crate::model::{CatalogSql, ColumnDef, ColumnDefault, ForeignKey, Ident, Logical};
use crate::model::{NativeType, PrimaryKey};
use crate::model::{QualifiedName, TableDef};
use crate::op::{Ctx, Op, Param, Statement};
use crate::planning::diff::{KeyValue, Precheck};
use canyon_core::query::querybuilder::syntax::dialect::SqlDialect;
use canyon_core::types::SqlType;
use std::fmt;

/// DDL for one backend. Implemented for the query builder's dialects.
pub trait DdlDialect: SqlDialect + Sized {
    /// The backend's name, for messages.
    const NAME: &'static str;

    /// DDL can run inside the plan's transaction (MySQL commits it implicitly).
    const TRANSACTIONAL_DDL: bool;

    /// A text column in a key needs a `length`.
    const TEXT_KEY_NEEDS_LENGTH: bool = false;

    /// The longest `length` of a text column in a key.
    const MAX_KEY_LENGTH: Option<u32> = None;

    /// The longest `length` of a text column.
    const MAX_LENGTH: u32;

    /// The most fractional-second digits of a time or timestamp.
    const MAX_PRECISION: u8;

    /// The biggest row `CREATE TABLE` takes, as `mysql_row_bytes` counts it (MySQL).
    const MAX_ROW_BYTES: Option<u64> = None;

    /// `ALTER COLUMN` is refused on a primary key column (SQL Server): the key is dropped
    /// and added back around a type change.
    const KEY_COLUMNS_BLOCK_TYPE_CHANGE: bool = false;

    /// Changing a column's type or nullability restates the whole column (SQL Server
    /// `ALTER COLUMN`, MySQL `MODIFY`), which a generated column doesn't survive.
    const REWRITES_COLUMN: bool = false;

    /// A column an FK uses, on either side, can't gain or lose its identity while the FK
    /// exists (MySQL): the FKs are dropped and added back around it.
    const FK_COLUMNS_BLOCK_IDENTITY_CHANGE: bool = false;

    /// The type of a column a generated column uses can't change (PostgreSQL, SQL Server).
    const GENERATED_BLOCKS_BASE_TYPE_CHANGE: bool = false;

    /// Catalog type names (`NativeType::name`) that `to_logical` leaves as `Other` but a
    /// `Param::Text` binds to, so a fill value can still be given for them.
    const TEXT_PARAM_TYPES: &'static [&'static str] = &[];

    /// The column type Canyon creates for `ty`.
    fn type_sql(ty: SqlType) -> Result<TypeSql, LowerError>;

    /// A catalog type back to Canyon's types; `Other` when there is no mapping.
    fn to_logical(native: &NativeType) -> Logical;

    /// Ops this backend can only do by rebuilding the table.
    fn needs_rebuild(_op: &Op) -> bool {
        false
    }

    /// `value` as an escaped string literal of this backend.
    fn text_literal(value: &str) -> String;

    /// The SQL after `DEFAULT` for a default read from this database.
    fn default_sql(w: &mut DdlWriter<Self>, default: &ColumnDefault) {
        match default {
            ColumnDefault::Expr { sql, .. } => w.catalog_sql(sql),
            ColumnDefault::Literal { value, .. } => w.literal(value),
        };
    }

    /// The statements for `op`, in order.
    fn lower(op: &Op, ctx: &Ctx<'_>) -> Result<Vec<Statement>, LowerError>;

    /// `value` as a bound integer to compare with a column of any integer width.
    fn bigint_param(w: &mut DdlWriter<Self>, value: i64) {
        w.param(Param::BigInt(value));
    }

    /// One row, one `BIGINT` column: 1 when the data the check looks for is there, 0 when
    /// not; for `RowCount`, the rows up to 2; for `RowEstimate`, the estimated row count. `default_schema` is the schema of
    /// a name that has none.
    fn precheck(p: &Precheck, default_schema: &str) -> Statement;

    /// One row, one `BIGINT` column: 1 when the migration lock was taken. Doesn't wait.
    /// Held by the session that took it, until `unlock` or the end of the session.
    fn try_lock() -> Statement;

    /// Frees the lock `try_lock` took. Only after a `try_lock` that returned 1.
    /// What it answers differs per backend (a bool on PostgreSQL, no rows on SQL Server,
    /// an int or NULL on MySQL): don't decode it, run it with `query_rows` or `execute`.
    fn unlock() -> Statement;

    /// `model` of the newest snapshot, one row; no row when there is none.
    fn latest_snapshot() -> Statement;
}

/// The table Canyon keeps its schema snapshots in.
pub(crate) const SNAPSHOT_TABLE: &str = "canyon_schema_snapshot";

/// The inner SELECT of the prechecks that look the same everywhere; `false` for the others.
pub(crate) fn exists_body<D: DdlDialect>(w: &mut DdlWriter<D>, p: &Precheck) -> bool {
    match p {
        Precheck::NonNull { columns, .. } | Precheck::Duplicates { columns, .. } => {
            debug_assert!(!columns.is_empty(), "a precheck needs at least one column");
        }
        Precheck::Orphans {
            columns,
            target_columns,
            ..
        } => {
            debug_assert!(!columns.is_empty(), "a precheck needs at least one column");
            debug_assert_eq!(
                columns.len(),
                target_columns.len(),
                "a foreign key pairs each column with a target column"
            );
        }
        Precheck::FilledOrphans { columns, .. } => {
            debug_assert!(
                columns
                    .iter()
                    .any(|(v, _)| !matches!(v, KeyValue::Column(_))),
                "a filled key needs at least one filled column"
            );
        }
        _ => {}
    }
    match p {
        Precheck::TableHasRows(t) => {
            w.raw("SELECT 1 FROM ").qualified(t);
        }
        Precheck::NonNull { table, columns } => {
            w.raw("SELECT 1 FROM ").qualified(table).raw(" WHERE ");
            for (i, c) in columns.iter().enumerate() {
                if i > 0 {
                    w.raw(" AND ");
                }
                w.ident(c).raw(" IS NOT NULL");
            }
        }
        Precheck::HasNulls { table, column } => {
            w.raw("SELECT 1 FROM ")
                .qualified(table)
                .raw(" WHERE ")
                .ident(column)
                .raw(" IS NULL");
        }
        Precheck::OutOfRange {
            table,
            column,
            min,
            max,
        } => {
            w.raw("SELECT 1 FROM ")
                .qualified(table)
                .raw(" WHERE ")
                .ident(column)
                .raw(" < ");
            D::bigint_param(w, *min);
            w.raw(" OR ").ident(column).raw(" > ");
            D::bigint_param(w, *max);
        }
        Precheck::Duplicates { table, columns } => {
            w.raw("SELECT 1 FROM ")
                .qualified(table)
                .raw(" GROUP BY ")
                .idents(columns)
                .raw(" HAVING COUNT(*) > 1");
        }
        Precheck::Orphans {
            table,
            columns,
            target,
            target_columns,
        } => {
            w.raw("SELECT 1 FROM ")
                .qualified(table)
                .raw(" AS canyon_c WHERE ");
            for c in columns {
                w.raw("canyon_c.").ident(c).raw(" IS NOT NULL AND ");
            }
            w.raw("NOT EXISTS (SELECT 1 FROM ")
                .qualified(target)
                .raw(" AS canyon_p WHERE ");
            for (i, (c, t)) in columns.iter().zip(target_columns).enumerate() {
                if i > 0 {
                    w.raw(" AND ");
                }
                w.raw("canyon_p.").ident(t).raw(" = canyon_c.").ident(c);
            }
            w.raw(")");
        }
        Precheck::FilledOrphans {
            table,
            target,
            columns,
        } => {
            w.raw("SELECT 1 FROM ")
                .qualified(table)
                .raw(" AS canyon_c WHERE ");
            for (value, _) in columns {
                if let KeyValue::Column(c) = value {
                    w.raw("canyon_c.").ident(c).raw(" IS NOT NULL AND ");
                }
            }
            w.raw("NOT EXISTS (SELECT 1 FROM ")
                .qualified(target)
                .raw(" AS canyon_p WHERE ");
            for (i, (value, t)) in columns.iter().enumerate() {
                if i > 0 {
                    w.raw(" AND ");
                }
                w.raw("canyon_p.").ident(t).raw(" = ");
                match value {
                    KeyValue::Column(c) => {
                        w.raw("canyon_c.").ident(c);
                    }
                    KeyValue::Fill(v) => fill_param(w, v),
                    KeyValue::ColumnOrFill(c, v) => {
                        w.raw("COALESCE(canyon_c.").ident(c).raw(", ");
                        fill_param(w, v);
                        w.raw(")");
                    }
                }
            }
            w.raw(")");
        }
        // they differ per backend: each dialect writes them
        Precheck::LongerThan { .. } | Precheck::RowCount(_) | Precheck::RowEstimate(_) => {
            return false;
        }
    }
    true
}

/// A fill value compared with a target column, which may be of another integer width.
fn fill_param<D: DdlDialect>(w: &mut DdlWriter<D>, value: &Param) {
    match value {
        Param::SmallInt(v) => D::bigint_param(w, i64::from(*v)),
        Param::Int(v) => D::bigint_param(w, i64::from(*v)),
        Param::BigInt(v) => D::bigint_param(w, *v),
        other => {
            w.param(other.clone());
        }
    }
}

/// A length bound as the `int` the databases' length functions return.
pub(crate) fn length_param(max_len: u32) -> Param {
    Param::Int(i32::try_from(max_len).unwrap_or(i32::MAX))
}

/// A request the planner should never make; returned instead of panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LowerError {
    NotForThisDialect(&'static str),
    MissingActual(String),
    DesiredTypeUnknown(String),
    /// The column is computed from others; Canyon doesn't rewrite it.
    GeneratedColumn(String),
    UnsupportedType(SqlType),
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotForThisDialect(op) => write!(f, "`{op}` is not used by this database"),
            Self::MissingActual(what) => write!(f, "{what} is not in the database model"),
            Self::DesiredTypeUnknown(column) => write!(f, "column `{column}` has no Canyon type"),
            Self::GeneratedColumn(column) => write!(
                f,
                "column `{column}` is generated from other columns: Canyon doesn't change it"
            ),
            Self::UnsupportedType(ty) => {
                write!(f, "{ty:?} has no column type on this database yet")
            }
        }
    }
}

impl std::error::Error for LowerError {}

pub(crate) fn one<D: DdlDialect>(w: DdlWriter<D>) -> Result<Vec<Statement>, LowerError> {
    Ok(vec![w.finish()])
}

pub(crate) fn known(column: &ColumnDef) -> Result<SqlType, LowerError> {
    match &column.ty {
        Logical::Known(ty) => Ok(*ty),
        Logical::Other(_) => Err(LowerError::DesiredTypeUnknown(column.name.to_string())),
    }
}

/// The type a desired column asks for.
pub(crate) fn desired_type<D: DdlDialect>(column: &ColumnDef) -> Result<TypeSql, LowerError> {
    D::type_sql(known(column)?)
}

/// The column's type as the database has it, so a change that restates the column
/// keeps a "compatible" type as it is.
#[cfg(any(feature = "mssql", feature = "mysql"))]
pub(crate) fn actual_type<D: DdlDialect>(column: &ColumnDef) -> Result<TypeSql, LowerError> {
    match &column.ty {
        Logical::Known(ty) => D::type_sql(*ty),
        Logical::Other(native) => Ok(TypeSql::from_catalog(&native.raw)),
    }
}

#[cfg(any(feature = "mssql", feature = "mysql"))]
pub(crate) fn actual_column<'a>(
    ctx: &'a Ctx<'_>,
    table: &QualifiedName,
    column: &Ident,
) -> Result<(&'a TableDef, &'a ColumnDef), LowerError> {
    let missing = || LowerError::MissingActual(format!("column {table}.{column}"));
    let t = ctx.actual_table(table).ok_or_else(missing)?;
    let c = t
        .columns
        .iter()
        .find(|c| &c.name == column)
        .ok_or_else(missing)?;
    Ok((t, c))
}

/// `actual` as a statement that changes its type to `new_type` has to write it again:
/// - the collation only for text (SQL Server refuses `COLLATE` on an `int`);
/// - a `CURRENT_TIMESTAMP` default and `ON UPDATE` moved to a timestamp's new precision
///   (MySQL wants one precision in the whole column), `ON UPDATE` dropped on any other
///   type (MySQL allows it only on timestamps);
/// - the comment and the rest as they are.
pub(crate) fn restated(actual: &ColumnDef, new_type: SqlType) -> ColumnDef {
    let precision = match new_type {
        SqlType::Timestamp { precision } => Some(precision),
        _ => None,
    };
    let mut column = actual.clone();
    column.ty = Logical::Known(new_type);
    if !matches!(new_type, SqlType::Text { .. }) {
        column.extra.collation = None;
    }
    column.extra.on_update = actual
        .extra
        .on_update
        .as_ref()
        .and(precision)
        .map(current_timestamp);
    if let (Some(ColumnDefault::Expr { constraint, sql }), Some(precision)) =
        (&actual.default, precision)
        && is_current_timestamp(sql.as_str())
    {
        column.default = Some(ColumnDefault::Expr {
            constraint: constraint.clone(),
            sql: current_timestamp(precision),
        });
    }
    column
}

/// `CURRENT_TIMESTAMP` or `CURRENT_TIMESTAMP(n)`, as the catalogs report it.
pub(crate) fn is_current_timestamp(sql: &str) -> bool {
    const KEYWORD: &str = "CURRENT_TIMESTAMP";
    let Some(rest) = sql
        .get(..KEYWORD.len())
        .filter(|head| head.eq_ignore_ascii_case(KEYWORD))
        .map(|_| &sql[KEYWORD.len()..])
    else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix('(')
            .and_then(|r| r.strip_suffix(')'))
            .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
}

/// `CURRENT_TIMESTAMP` with `precision` fractional digits, written the way MySQL reports it.
fn current_timestamp(precision: u8) -> CatalogSql {
    CatalogSql::new(match precision {
        0 => "CURRENT_TIMESTAMP".to_owned(),
        p => format!("CURRENT_TIMESTAMP({p})"),
    })
}

/// Rewriting a generated column would turn it into a plain one (MySQL) or fail (SQL Server).
#[cfg(any(feature = "mssql", feature = "mysql"))]
pub(crate) fn rewritable(column: &ColumnDef) -> Result<(), LowerError> {
    if column.extra.generated {
        Err(LowerError::GeneratedColumn(column.name.to_string()))
    } else {
        Ok(())
    }
}

pub(crate) fn add_primary_key<D: DdlDialect>(
    table: &QualifiedName,
    key: &PrimaryKey,
) -> DdlWriter<D> {
    let name = key.name.clone().unwrap_or_else(|| pk_name(table));
    let mut w = DdlWriter::new();
    w.raw("ALTER TABLE ")
        .qualified(table)
        .raw(" ADD CONSTRAINT ")
        .ident(&name)
        .raw(" PRIMARY KEY (")
        .idents(&key.columns)
        .raw(")");
    w
}

/// `UPDATE … SET column = value WHERE column IS NULL`, the value bound.
pub(crate) fn fill_column<D: DdlDialect>(
    table: &QualifiedName,
    column: &Ident,
    value: &Param,
) -> DdlWriter<D> {
    let mut w = DdlWriter::new();
    w.raw("UPDATE ")
        .qualified(table)
        .raw(" SET ")
        .ident(column)
        .raw(" = ")
        .param(value.clone())
        .raw(" WHERE ")
        .ident(column)
        .raw(" IS NULL");
    w
}

pub(crate) fn add_foreign_key<D: DdlDialect>(
    table: &QualifiedName,
    name: &Ident,
    key: &ForeignKey,
    ref_table: &QualifiedName,
) -> DdlWriter<D> {
    let mut w = DdlWriter::new();
    w.raw("ALTER TABLE ")
        .qualified(table)
        .raw(" ADD CONSTRAINT ")
        .ident(name)
        .raw(" FOREIGN KEY (")
        .idents(&key.columns)
        .raw(") REFERENCES ")
        .qualified(ref_table)
        .raw(" (")
        .idents(&key.ref_columns)
        .raw(")");
    w
}

pub(crate) fn drop_constraint<D: DdlDialect>(table: &QualifiedName, name: &Ident) -> DdlWriter<D> {
    let mut w = DdlWriter::new();
    w.raw("ALTER TABLE ")
        .qualified(table)
        .raw(" DROP CONSTRAINT ")
        .ident(name);
    w
}

/// The constraints of `actual` that carry the name Canyon gives them, with the name
/// they get once the table is called `to`. Constraints named by hand keep their name.
pub(crate) struct RenamedConstraints<'a> {
    pub primary_key: Option<(Ident, Ident)>,
    pub foreign_keys: Vec<(&'a ForeignKey, Ident, Ident)>,
}

pub(crate) fn renamed_constraints<'a>(
    actual: Option<&'a TableDef>,
    from: &QualifiedName,
    to: &QualifiedName,
) -> RenamedConstraints<'a> {
    let Some(actual) = actual else {
        return RenamedConstraints {
            primary_key: None,
            foreign_keys: Vec::new(),
        };
    };
    let primary_key = actual
        .primary_key
        .as_ref()
        .and_then(|pk| pk.name.clone())
        .filter(|name| *name == pk_name(from))
        .map(|name| (name, pk_name(to)));
    let foreign_keys = actual
        .foreign_keys
        .iter()
        .filter_map(|fk| {
            let name = fk.name.clone()?;
            (name == fk_name(from, &fk.columns)).then(|| (fk, name, fk_name(to, &fk.columns)))
        })
        .collect();
    RenamedConstraints {
        primary_key,
        foreign_keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CatalogSql, ColumnExtra, TableDef};

    fn stamp(default: &str, on_update: Option<&str>) -> ColumnDef {
        let mut column = TableDef::builder("T", "t")
            .column("ts", SqlType::Timestamp { precision: 6 }, true)
            .build()
            .unwrap()
            .columns
            .remove(0);
        column.default = Some(ColumnDefault::Expr {
            constraint: None,
            sql: CatalogSql::new(default),
        });
        column.extra = ColumnExtra {
            collation: Some(CatalogSql::new("latin1_bin")),
            comment: Some("kept".into()),
            on_update: on_update.map(CatalogSql::new),
            ..ColumnExtra::default()
        };
        column
    }

    fn default_sql(c: &ColumnDef) -> Option<&str> {
        match &c.default {
            Some(ColumnDefault::Expr { sql, .. }) => Some(sql.as_str()),
            _ => None,
        }
    }

    #[test]
    fn a_new_timestamp_precision_moves_current_timestamp_along() {
        let actual = stamp("CURRENT_TIMESTAMP(6)", Some("CURRENT_TIMESTAMP(6)"));
        let to3 = restated(&actual, SqlType::Timestamp { precision: 3 });
        assert_eq!(default_sql(&to3), Some("CURRENT_TIMESTAMP(3)"));
        assert_eq!(
            to3.extra.on_update.unwrap().as_str(),
            "CURRENT_TIMESTAMP(3)"
        );
        let to0 = restated(&actual, SqlType::Timestamp { precision: 0 });
        assert_eq!(default_sql(&to0), Some("CURRENT_TIMESTAMP"));
        let legacy = stamp("CURRENT_TIMESTAMP", Some("CURRENT_TIMESTAMP"));
        let to6 = restated(&legacy, SqlType::Timestamp { precision: 6 });
        assert_eq!(default_sql(&to6), Some("CURRENT_TIMESTAMP(6)"));
        assert_eq!(
            to6.extra.on_update.unwrap().as_str(),
            "CURRENT_TIMESTAMP(6)"
        );
    }

    #[test]
    fn other_types_keep_what_they_can_hold() {
        let actual = stamp("CURRENT_TIMESTAMP(6)", Some("CURRENT_TIMESTAMP(6)"));
        let text = restated(&actual, SqlType::Text { max_len: Some(40) });
        assert_eq!(
            default_sql(&text),
            Some("CURRENT_TIMESTAMP(6)"),
            "the dialect decides"
        );
        assert_eq!(text.extra.on_update, None);
        assert!(text.extra.collation.is_some());
        let int = restated(&actual, SqlType::BigInt);
        assert_eq!(int.extra.collation, None);
        assert_eq!(int.extra.comment.as_deref(), Some("kept"));
        assert_eq!(int.ty, Logical::Known(SqlType::BigInt));
        let plain = stamp("now()", None);
        let to3 = restated(&plain, SqlType::Timestamp { precision: 3 });
        assert_eq!(default_sql(&to3), Some("now()"));
        assert_eq!(to3.extra.on_update, None);
    }

    #[test]
    fn current_timestamp_forms() {
        for (sql, expected) in [
            ("CURRENT_TIMESTAMP", true),
            ("CURRENT_TIMESTAMP(6)", true),
            ("current_timestamp(3)", true),
            ("CURRENT_TIMESTAMP()", true),
            ("CURRENT_TIMESTAMP(6) + INTERVAL 1 DAY", false),
            ("now(6)", false),
            ("CURRENT_TIMESTAMPX", false),
            ("", false),
        ] {
            assert_eq!(is_current_timestamp(sql), expected, "{sql}");
        }
    }
}
