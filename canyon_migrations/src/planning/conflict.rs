//! Everything that stops a plan, plus the lines a plan shows without stopping.

use crate::model::{
    ColumnDef, Ident, Logical, ModelError, QualifiedName, sql_type_words, type_words,
};
use crate::op::Op;
use canyon_core::types::SqlType;
use std::fmt;

fn names(list: &[Ident]) -> String {
    list.iter()
        .map(|i| format!("`{i}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Which limit a column goes over. The numbers are the backend's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Limit {
    /// `length = 0`.
    ZeroLength,
    /// A text key column without `length`.
    TextKeyNeedsLength,
    /// The longest `length` of a text key column.
    KeyLength(u32),
    /// The longest `length` of a text column.
    Length(u32),
    /// The most fractional-second digits.
    Precision(u8),
}

/// A desired model that can't be turned into a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelIssue {
    Model(ModelError),
    CaseCollision {
        first: QualifiedName,
        second: QualifiedName,
    },
    ForeignKeyTarget {
        table: QualifiedName,
        column: Ident,
        target: QualifiedName,
        target_column: Ident,
    },
    ForeignKeyType {
        table: QualifiedName,
        column: Ident,
        target: QualifiedName,
        target_column: Ident,
    },
    ForeignKeyNotKey {
        table: QualifiedName,
        columns: Vec<Ident>,
        target: QualifiedName,
    },
    DuplicateForeignKeyName {
        name: Ident,
        first: QualifiedName,
        second: QualifiedName,
    },
    /// A column over a limit of `database`.
    Limit {
        table: QualifiedName,
        column: Ident,
        limit: Limit,
        database: &'static str,
    },
    RowTooLarge {
        table: QualifiedName,
        bytes: u64,
        max: u64,
        database: &'static str,
    },
    /// A primary key column that allows NULL.
    NullableKey {
        table: QualifiedName,
        column: Ident,
    },
    /// A schema written like the default one, in another case.
    SchemaCase {
        schema: Ident,
        default: String,
    },
}

impl fmt::Display for ModelIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Model(e) => write!(f, "{e}."),
            Self::CaseCollision { first, second } => write!(
                f,
                "tables `{first}` and `{second}` only differ in case.\n     SQL Server and MySQL on Windows/macOS treat them as one table. Rename one of them."
            ),
            Self::ForeignKeyTarget {
                table,
                column,
                target,
                target_column,
            } => write!(
                f,
                "{table}: `{column}` references `{target}.{target_column}`, which is neither an entity nor a table in the database.\n     Check `foreign_key(references = ...)`."
            ),
            Self::ForeignKeyType {
                table,
                column,
                target,
                target_column,
            } => write!(
                f,
                "{table}: `{column}` and the column it references, `{target}.{target_column}`, have different types.\n     Use the same type (and `length`) on both fields."
            ),
            Self::ForeignKeyNotKey {
                table,
                columns,
                target,
            } => write!(
                f,
                "{table}: the foreign key ({}) must reference the primary key of `{target}`.\n     Point it at the key column(s) of that table.",
                names(columns)
            ),
            Self::DuplicateForeignKeyName {
                name,
                first,
                second,
            } => write!(
                f,
                "foreign keys on `{first}` and `{second}` both get the name `{name}`.\n     SQL Server and MySQL need it unique in the schema. Rename one of the tables or columns."
            ),
            Self::Limit {
                table,
                column,
                limit,
                database,
            } => match limit {
                Limit::ZeroLength => write!(f, "{table}.{column}: `length` must be at least 1."),
                Limit::TextKeyNeedsLength => write!(
                    f,
                    "{table}.{column}: a text key needs `length` on {database}."
                ),
                Limit::KeyLength(max) => write!(
                    f,
                    "{table}.{column}: text keys are limited to {max} characters on {database}."
                ),
                Limit::Length(max) => write!(
                    f,
                    "{table}.{column}: `length` is limited to {max} on {database}; use `String` without `length`."
                ),
                Limit::Precision(max) => write!(
                    f,
                    "{table}.{column}: `precision` is limited to {max} on {database}."
                ),
            },
            Self::RowTooLarge {
                table,
                bytes,
                max,
                database,
            } => write!(
                f,
                "{table}: a row can take {bytes} bytes, {database}'s limit is {max}.\n     Lower some `length` values, or use `String` without `length` for long text."
            ),
            Self::NullableKey { table, column } => write!(
                f,
                "{table}: `{column}` is in the primary key, so it can't be nullable.\n     Use a type that isn't an `Option` for that field."
            ),
            Self::SchemaCase { schema, default } => write!(
                f,
                "schema `{schema}` only differs in case from the default schema `{default}`.\n     Write it as `{default}`, or leave `schema` out."
            ),
        }
    }
}

/// Something that stops a plan until the user decides or fixes it.
// Conflicts are few and short-lived: an `Op` inline is simpler than a `Box`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Conflict {
    InvalidModel(ModelIssue),
    AmbiguousTableRename {
        old: QualifiedName,
        candidates: Vec<QualifiedName>,
    },
    AmbiguousColumnRename {
        table: QualifiedName,
        removed: Vec<ColumnDef>,
        added: Vec<ColumnDef>,
    },
    /// Renames that take each other's names (a swap, or longer).
    RenameCycle {
        tables: Vec<QualifiedName>,
    },
    /// A rename onto the name of a table that stays.
    RenameTargetTaken {
        old: QualifiedName,
        new: QualifiedName,
    },
    /// A column rename onto the name of a column the table has already.
    ColumnRenameTargetTaken {
        table: QualifiedName,
        old: Ident,
        new: Ident,
    },
    Destructive {
        op: Op,
    },
    NotNullOnTableWithRows {
        table: QualifiedName,
        column: Ident,
    },
    NullsBlockNotNull {
        table: QualifiedName,
        column: Ident,
    },
    ValuesOutOfRange {
        table: QualifiedName,
        column: Ident,
        to: SqlType,
    },
    ValuesTooLong {
        table: QualifiedName,
        column: Ident,
        max_len: u32,
    },
    DuplicateKeyValues {
        table: QualifiedName,
        columns: Vec<Ident>,
    },
    OrphanRows {
        table: QualifiedName,
        columns: Vec<Ident>,
        target: QualifiedName,
    },
    LargeRebuild {
        table: QualifiedName,
        estimated_rows: u64,
    },
    /// `op` is the op's `describe()` text.
    GeneratedColumn {
        table: QualifiedName,
        column: Ident,
        op: String,
    },
    GeneratedColumnDependency {
        table: QualifiedName,
        column: Ident,
        generated: Vec<Ident>,
    },
    DependedOnByGenerated {
        table: QualifiedName,
        column: Ident,
        generated: Vec<Ident>,
    },
    /// The value given for the existing rows doesn't fit the column's type.
    FillValueType {
        table: QualifiedName,
        column: Ident,
        expected: Logical,
    },
    /// NULLs block NOT NULL on a column whose type no fill value binds to.
    FillByHand {
        table: QualifiedName,
        column: Ident,
        column_type: Logical,
    },
    /// The planner broke its own rules: a bug, not something the user can decide.
    Internal(String),
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidModel(issue) => write!(f, "{issue}"),
            Self::AmbiguousTableRename { old, candidates } => {
                let list: Vec<String> = candidates.iter().map(|c| format!("`{c}`")).collect();
                write!(
                    f,
                    "table `{old}` is gone and {} could be its new name.\n     Run `migrate` to choose.",
                    list.join(" or ")
                )
            }
            Self::AmbiguousColumnRename {
                table,
                removed,
                added,
            } => {
                let fmt_cols = |cols: &[ColumnDef]| {
                    cols.iter()
                        .map(|c| format!("`{}` ({})", c.name, type_words(&c.ty)))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                write!(
                    f,
                    "{table}: column {} was removed and {} was added.\n     Is it a rename? Run `migrate` to choose, or do it in two steps.",
                    fmt_cols(removed),
                    fmt_cols(added)
                )
            }
            Self::RenameCycle { tables } => {
                let list: Vec<String> = tables.iter().map(|t| format!("`{t}`")).collect();
                let what = if tables.len() == 2 {
                    "swap names"
                } else {
                    "take each other's names"
                };
                write!(
                    f,
                    "tables {} {what}. Canyon can't do that in one run.\n     Rename one of them to a temporary name first.",
                    list.join(", ")
                )
            }
            Self::RenameTargetTaken { old, new } => write!(
                f,
                "table `{old}` should be renamed to `{new}`, but `{new}` already exists in the database.\n     Drop or rename `{new}` by hand first, or run `migrate` to choose."
            ),
            Self::ColumnRenameTargetTaken { table, old, new } => write!(
                f,
                "{table}: column `{old}` should be renamed to `{new}`, but `{new}` already exists in the database.\n     Drop or rename `{new}` by hand first, or run `migrate` to choose."
            ),
            Self::Destructive { op } => write!(
                f,
                "{} would lose data.\n     Run `migrate` to confirm it, or set `allow_destructive = true` for this datasource.",
                capitalize(&op.describe())
            ),
            Self::NotNullOnTableWithRows { table, column } => write!(
                f,
                "{table}: new column `{column}` is NOT NULL and the table has rows.\n     Make the field an `Option`, or run `migrate` to give a value for the existing rows."
            ),
            Self::NullsBlockNotNull { table, column } => write!(
                f,
                "{table}: `{column}` can't become NOT NULL, some rows have no value.\n     Fill them first, or run `migrate` to give a value."
            ),
            Self::ValuesOutOfRange { table, column, to } => write!(
                f,
                "{table}: some values of `{column}` don't fit in {}.\n     Keep the wider type, or fix those rows first.",
                sql_type_words(*to)
            ),
            Self::ValuesTooLong {
                table,
                column,
                max_len,
            } => write!(
                f,
                "{table}: some values of `{column}` are longer than {max_len} characters.\n     Raise `length`, or shorten those rows first."
            ),
            Self::DuplicateKeyValues { table, columns } => write!(
                f,
                "{table}: the new primary key ({}) has repeated values.\n     Fix those rows first.",
                names(columns)
            ),
            Self::OrphanRows {
                table,
                columns,
                target,
            } => write!(
                f,
                "{table}: some rows of {} point to rows that don't exist in `{target}`.\n     Fix or delete those rows first.",
                names(columns)
            ),
            Self::LargeRebuild {
                table,
                estimated_rows,
            } => write!(
                f,
                "{table}: SQL Server has to rebuild the table (about {estimated_rows} rows) to change its identity column.\n     Run `migrate` to confirm it, or raise `rebuild_confirm_rows`."
            ),
            Self::GeneratedColumn { table, column, op } => write!(
                f,
                "{table}: `{column}` is computed from other columns, and \"{op}\" would rewrite it.\n     Canyon doesn't change generated columns: change it by hand, or make the field match the database."
            ),
            Self::GeneratedColumnDependency {
                table,
                column,
                generated,
            } => write!(
                f,
                "{table}: `{column}` changes type, and the table has generated columns ({}) that may use it.\n     The database refuses that change: drop or change those columns by hand first.",
                names(generated)
            ),
            Self::DependedOnByGenerated {
                table,
                column,
                generated,
            } => write!(
                f,
                "{table}: `{column}` would be dropped, and the table has generated columns ({}) that may use it.\n     Drop or change those columns by hand first.",
                names(generated)
            ),
            Self::FillValueType {
                table,
                column,
                expected,
            } => write!(
                f,
                "{table}: the value given for the existing rows of `{column}` doesn't fit its type, {}.\n     Run `migrate` again and give a {} value.",
                type_words(expected),
                type_words(expected)
            ),
            Self::FillByHand {
                table,
                column,
                column_type,
            } => write!(
                f,
                "{table}: `{column}` can't become NOT NULL, some rows have no value, and Canyon can't write a value of its type, {}.\n     Fill those rows by hand first.",
                type_words(column_type)
            ),
            Self::Internal(detail) => write!(
                f,
                "Canyon couldn't plan this change, this is a bug: {detail}.\n     Please report it with this message."
            ),
        }
    }
}

/// Adds the conflicts of `more` that `out` doesn't have yet.
pub(crate) fn push_new(out: &mut Vec<Conflict>, more: impl IntoIterator<Item = Conflict>) {
    for c in more {
        if !out.contains(&c) {
            out.push(c);
        }
    }
}

/// The text `run()` fails with and startup prints.
pub struct ConflictReport<'a> {
    pub datasource: &'a str,
    pub conflicts: &'a [Conflict],
}

impl fmt::Display for ConflictReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.conflicts.len();
        writeln!(
            f,
            "Migrations for datasource `{}` stopped: {n} conflict{}, nothing was applied.",
            self.datasource,
            if n == 1 { "" } else { "s" }
        )?;
        for (i, conflict) in self.conflicts.iter().enumerate() {
            write!(f, "\n  {}. {conflict}\n", i + 1)?;
        }
        Ok(())
    }
}

/// A line a plan shows without stopping.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Info {
    Tightenable {
        table: QualifiedName,
        column: Ident,
        column_type: Logical,
        canonical: SqlType,
    },
    Unmanaged {
        table: QualifiedName,
    },
}

impl fmt::Display for Info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tightenable {
                table,
                column,
                column_type,
                canonical,
            } => write!(
                f,
                "info: {table}.{column} is {}, the field needs {}. It works; `migrate` can tighten it.",
                type_words(column_type),
                sql_type_words(*canonical)
            ),
            Self::Unmanaged { table } => write!(
                f,
                "info: table `{table}` has no entity anymore. Canyon left it as it is; `migrate` can drop it."
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Warning {
    Narrower {
        table: QualifiedName,
        column: Ident,
        column_type: Logical,
        field: SqlType,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Narrower {
                table,
                column,
                column_type,
                field,
            } => write!(
                f,
                "warning: {table}.{column} is {}, smaller than {} for the field. Values that don't fit will fail on insert.",
                type_words(column_type),
                sql_type_words(*field)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, ColumnExtra, Logical, NativeType};
    use crate::planning::test_support::{id, q};

    fn col(name: &str, ty: SqlType) -> ColumnDef {
        ColumnDef {
            name: id(name),
            ty: Logical::Known(ty),
            nullable: true,
            default: None,
            extra: ColumnExtra::default(),
        }
    }

    fn every_conflict() -> Vec<Conflict> {
        vec![
            Conflict::InvalidModel(ModelIssue::CaseCollision {
                first: q("Player"),
                second: q("player"),
            }),
            Conflict::InvalidModel(ModelIssue::ForeignKeyTarget {
                table: q("player"),
                column: id("league"),
                target: q("league"),
                target_column: id("id"),
            }),
            Conflict::InvalidModel(ModelIssue::ForeignKeyType {
                table: q("player"),
                column: id("league"),
                target: q("league"),
                target_column: id("id"),
            }),
            Conflict::InvalidModel(ModelIssue::ForeignKeyNotKey {
                table: q("player"),
                columns: vec![id("league"), id("season")],
                target: q("league"),
            }),
            Conflict::InvalidModel(ModelIssue::DuplicateForeignKeyName {
                name: id("a_b_c_fkey"),
                first: q("a_b"),
                second: q("a.b_c"),
            }),
            Conflict::InvalidModel(ModelIssue::Limit {
                table: q("player"),
                column: id("slug"),
                limit: Limit::TextKeyNeedsLength,
                database: "SQL Server",
            }),
            Conflict::InvalidModel(ModelIssue::Limit {
                table: q("player"),
                column: id("slug"),
                limit: Limit::KeyLength(768),
                database: "MySQL",
            }),
            Conflict::InvalidModel(ModelIssue::Limit {
                table: q("player"),
                column: id("bio"),
                limit: Limit::Length(4000),
                database: "SQL Server",
            }),
            Conflict::InvalidModel(ModelIssue::Limit {
                table: q("player"),
                column: id("bio"),
                limit: Limit::ZeroLength,
                database: "PostgreSQL",
            }),
            Conflict::InvalidModel(ModelIssue::Limit {
                table: q("player"),
                column: id("seen"),
                limit: Limit::Precision(6),
                database: "PostgreSQL",
            }),
            Conflict::InvalidModel(ModelIssue::RowTooLarge {
                table: q("player"),
                bytes: 80_008,
                max: 65_535,
                database: "MySQL",
            }),
            Conflict::InvalidModel(ModelIssue::NullableKey {
                table: q("player"),
                column: id("id"),
            }),
            Conflict::InvalidModel(ModelIssue::SchemaCase {
                schema: id("DBO"),
                default: "dbo".into(),
            }),
            Conflict::AmbiguousTableRename {
                old: q("player"),
                candidates: vec![q("gamer"), q("athlete")],
            },
            Conflict::AmbiguousColumnRename {
                table: q("player"),
                removed: vec![col("age", SqlType::Int)],
                added: vec![col("birth_date", SqlType::Date)],
            },
            Conflict::RenameCycle {
                tables: vec![q("gamer"), q("player")],
            },
            Conflict::RenameCycle {
                tables: vec![q("a"), q("b"), q("c")],
            },
            Conflict::RenameTargetTaken {
                old: q("player"),
                new: q("gamer"),
            },
            Conflict::ColumnRenameTargetTaken {
                table: q("player"),
                old: id("nick"),
                new: id("name"),
            },
            Conflict::Destructive {
                op: Op::DropColumn {
                    table: q("player"),
                    column: id("nick"),
                },
            },
            Conflict::NotNullOnTableWithRows {
                table: q("league"),
                column: id("region"),
            },
            Conflict::NullsBlockNotNull {
                table: q("league"),
                column: id("region"),
            },
            Conflict::ValuesOutOfRange {
                table: q("league"),
                column: id("points"),
                to: SqlType::SmallInt,
            },
            Conflict::ValuesTooLong {
                table: q("league"),
                column: id("slug"),
                max_len: 50,
            },
            Conflict::DuplicateKeyValues {
                table: q("league"),
                columns: vec![id("slug")],
            },
            Conflict::OrphanRows {
                table: q("player"),
                columns: vec![id("league")],
                target: q("league"),
            },
            Conflict::LargeRebuild {
                table: q("league"),
                estimated_rows: 2_500_000,
            },
            Conflict::GeneratedColumn {
                table: q("league"),
                column: id("total"),
                op: "rebuild table league".into(),
            },
            Conflict::GeneratedColumnDependency {
                table: q("league"),
                column: id("points"),
                generated: vec![id("total"), id("double")],
            },
            Conflict::DependedOnByGenerated {
                table: q("league"),
                column: id("points"),
                generated: vec![id("total")],
            },
            Conflict::FillValueType {
                table: q("league"),
                column: id("points"),
                expected: Logical::Known(SqlType::SmallInt),
            },
            Conflict::FillByHand {
                table: q("league"),
                column: id("founded"),
                column_type: Logical::Other(NativeType::named("datetime", "datetime")),
            },
            Conflict::Internal("no such table `league`".into()),
        ]
    }

    #[test]
    fn conflict_messages() {
        let conflicts = every_conflict();
        insta::assert_snapshot!(
            ConflictReport {
                datasource: "postgres_docker",
                conflicts: &conflicts
            }
            .to_string()
        );
    }

    #[test]
    fn info_and_warning_messages() {
        let i = Info::Tightenable {
            table: q("t"),
            column: id("c"),
            column_type: Logical::Known(SqlType::Text { max_len: None }),
            canonical: SqlType::Text { max_len: Some(10) },
        };
        let w = Warning::Narrower {
            table: q("t"),
            column: id("c"),
            column_type: Logical::Known(SqlType::Text { max_len: Some(100) }),
            field: SqlType::Text { max_len: None },
        };
        insta::assert_snapshot!(format!(
            "{i}\n{w}\n{}",
            Info::Unmanaged { table: q("old_t") }
        ));
    }

    #[test]
    fn a_single_conflict_reads_in_the_singular() {
        let one = [Conflict::Internal("x".into())];
        let text = ConflictReport {
            datasource: "d",
            conflicts: &one,
        }
        .to_string();
        assert!(text.contains("1 conflict,"), "{text}");
    }
}
