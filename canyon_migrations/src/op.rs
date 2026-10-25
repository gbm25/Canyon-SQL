//! Neutral schema changes, the statements a dialect turns them into, and what a dialect may look at.

use crate::model::{
    ColumnDef, ForeignKey, Ident, PrimaryKey, QualifiedName, SchemaModel, TableDef, type_words,
};
use canyon_core::query::parameters::QueryParameter;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    CreateSchema {
        schema: Ident,
    },
    CreateTable(TableDef),
    /// A new name in the same schema (`MoveTable` changes the schema).
    RenameTable {
        from: QualifiedName,
        to: Ident,
    },
    DropTable(QualifiedName),
    AddColumn {
        table: QualifiedName,
        column: ColumnDef,
    },
    RenameColumn {
        table: QualifiedName,
        from: Ident,
        to: Ident,
    },
    DropColumn {
        table: QualifiedName,
        column: Ident,
    },
    AlterColumnType {
        table: QualifiedName,
        column: ColumnDef,
    },
    SetNotNull {
        table: QualifiedName,
        column: Ident,
    },
    DropNotNull {
        table: QualifiedName,
        column: Ident,
    },
    AddPrimaryKey {
        table: QualifiedName,
        key: PrimaryKey,
    },
    DropPrimaryKey {
        table: QualifiedName,
        name: Ident,
    },
    AddIdentity {
        table: QualifiedName,
        column: Ident,
    },
    DropIdentity {
        table: QualifiedName,
        column: Ident,
    },
    AddForeignKey {
        table: QualifiedName,
        key: ForeignKey,
    },
    DropForeignKey {
        table: QualifiedName,
        name: Ident,
    },
    /// SQL Server only (`DdlDialect::needs_rebuild`): a new table, the rows copied, the
    /// old one dropped and the new one renamed. Other tables' foreign keys to it must be
    /// dropped before and added back after.
    RebuildTable {
        /// The table as it is now (with its defaults).
        from: TableDef,
        to: TableDef,
        /// `(column in from, column in to)` for every column whose rows are copied.
        column_map: Vec<(Ident, Ident)>,
    },
    /// Same table name, another schema (`None` = the default one).
    MoveTable {
        from: QualifiedName,
        to_schema: Option<Ident>,
    },
    /// `UPDATE … SET column = value WHERE column IS NULL`, before a `SetNotNull`.
    FillColumn {
        table: QualifiedName,
        column: Ident,
        value: Param,
    },
}

impl Op {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CreateSchema { .. } => "CreateSchema",
            Self::CreateTable(_) => "CreateTable",
            Self::RenameTable { .. } => "RenameTable",
            Self::DropTable(_) => "DropTable",
            Self::AddColumn { .. } => "AddColumn",
            Self::RenameColumn { .. } => "RenameColumn",
            Self::DropColumn { .. } => "DropColumn",
            Self::AlterColumnType { .. } => "AlterColumnType",
            Self::SetNotNull { .. } => "SetNotNull",
            Self::DropNotNull { .. } => "DropNotNull",
            Self::AddPrimaryKey { .. } => "AddPrimaryKey",
            Self::DropPrimaryKey { .. } => "DropPrimaryKey",
            Self::AddIdentity { .. } => "AddIdentity",
            Self::DropIdentity { .. } => "DropIdentity",
            Self::AddForeignKey { .. } => "AddForeignKey",
            Self::DropForeignKey { .. } => "DropForeignKey",
            Self::RebuildTable { .. } => "RebuildTable",
            Self::MoveTable { .. } => "MoveTable",
            Self::FillColumn { .. } => "FillColumn",
        }
    }

    /// The table the op changes (for renames and moves, the old name). `None` for
    /// schema ops.
    pub fn table(&self) -> Option<&QualifiedName> {
        Some(match self {
            Self::CreateSchema { .. } => return None,
            Self::CreateTable(t) => &t.name,
            Self::RenameTable { from, .. } | Self::MoveTable { from, .. } => from,
            Self::DropTable(t) => t,
            Self::RebuildTable { from, .. } => &from.name,
            Self::AddColumn { table, .. }
            | Self::RenameColumn { table, .. }
            | Self::DropColumn { table, .. }
            | Self::AlterColumnType { table, .. }
            | Self::SetNotNull { table, .. }
            | Self::DropNotNull { table, .. }
            | Self::AddPrimaryKey { table, .. }
            | Self::DropPrimaryKey { table, .. }
            | Self::AddIdentity { table, .. }
            | Self::DropIdentity { table, .. }
            | Self::AddForeignKey { table, .. }
            | Self::DropForeignKey { table, .. }
            | Self::FillColumn { table, .. } => table,
        })
    }

    /// What a confirmation of this op names (`Decisions::confirmed`).
    pub fn confirm_key(&self) -> ConfirmKey {
        let name = match self {
            Self::CreateSchema { schema } => Some(schema),
            Self::RenameTable { to, .. } => Some(to),
            Self::MoveTable { to_schema, .. } => to_schema.as_ref(),
            Self::RenameColumn { from: column, .. }
            | Self::DropColumn { column, .. }
            | Self::SetNotNull { column, .. }
            | Self::DropNotNull { column, .. }
            | Self::AddIdentity { column, .. }
            | Self::DropIdentity { column, .. }
            | Self::FillColumn { column, .. } => Some(column),
            Self::AddColumn { column, .. } | Self::AlterColumnType { column, .. } => {
                Some(&column.name)
            }
            Self::DropForeignKey { name, .. } => Some(name),
            Self::CreateTable(_)
            | Self::DropTable(_)
            | Self::AddPrimaryKey { .. }
            | Self::DropPrimaryKey { .. }
            | Self::AddForeignKey { .. }
            | Self::RebuildTable { .. } => None,
        };
        ConfirmKey {
            kind: self.kind(),
            table: self.table().cloned(),
            name: name.cloned(),
        }
    }

    /// One line for messages and history rows.
    pub fn describe(&self) -> String {
        match self {
            Self::CreateSchema { schema } => format!("create schema {schema}"),
            Self::CreateTable(t) => format!("create table {}", t.name),
            Self::RenameTable { from, to } => format!("rename table {from} to {to}"),
            Self::DropTable(t) => format!("drop table {t}"),
            Self::AddColumn { table, column } => format!(
                "add column {table}.{} ({})",
                column.name,
                type_words(&column.ty)
            ),
            Self::RenameColumn { table, from, to } => {
                format!("rename column {table}.{from} to {to}")
            }
            Self::DropColumn { table, column } => format!("drop column {table}.{column}"),
            Self::AlterColumnType { table, column } => format!(
                "change type of {table}.{} to {}",
                column.name,
                type_words(&column.ty)
            ),
            Self::SetNotNull { table, column } => format!("make {table}.{column} NOT NULL"),
            Self::DropNotNull { table, column } => format!("make {table}.{column} nullable"),
            Self::AddPrimaryKey { table, key } => {
                format!("add primary key to {table} ({})", join(&key.columns))
            }
            Self::DropPrimaryKey { table, .. } => format!("drop primary key of {table}"),
            Self::AddIdentity { table, column } => {
                format!("make {table}.{column} generated by the database")
            }
            Self::DropIdentity { table, column } => format!("stop generating {table}.{column}"),
            Self::AddForeignKey { table, key } => format!(
                "add foreign key {table} ({}) to {}",
                join(&key.columns),
                key.ref_table
            ),
            Self::DropForeignKey { table, name } => {
                format!("drop foreign key {name} of {table}")
            }
            Self::RebuildTable { from, .. } => rebuild_description(&from.name),
            Self::MoveTable {
                from,
                to_schema: Some(schema),
            } => format!("move table {from} to schema {schema}"),
            Self::MoveTable {
                from,
                to_schema: None,
            } => format!("move table {from} to the default schema"),
            Self::FillColumn { table, column, .. } => {
                format!("fill empty values of {table}.{column}")
            }
        }
    }
}

/// `describe()` of a rebuild of `table`, for a rebuild not planned yet.
pub(crate) fn rebuild_description(table: &QualifiedName) -> String {
    format!("rebuild table {table}")
}

/// A confirmed op, by kind and by what it changes: unlike the `describe()` line, two
/// ops never share one, and rewording a message doesn't lose a confirmation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfirmKey {
    /// `Op::kind()`.
    pub(crate) kind: &'static str,
    /// `Op::table()`.
    pub(crate) table: Option<QualifiedName>,
    /// The column, constraint or schema the op is about, when there is one.
    pub(crate) name: Option<Ident>,
}

impl ConfirmKey {
    /// The confirmation of a rebuild of `table` (`Op::RebuildTable`), asked before the
    /// planner turns the ops that need it into one.
    pub fn rebuild(table: &QualifiedName) -> Self {
        Self {
            kind: "RebuildTable",
            table: Some(table.clone()),
            name: None,
        }
    }
}

fn join(columns: &[Ident]) -> String {
    columns
        .iter()
        .map(Ident::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A value bound to a statement, never written into its text. One variant per integer
/// width: the drivers decode strictly, so the width has to match the column.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Param {
    Text(String),
    SmallInt(i16),
    Int(i32),
    BigInt(i64),
    Bool(bool),
    Date(NaiveDate),
    Time(NaiveTime),
    Timestamp(NaiveDateTime),
}

impl Param {
    pub fn as_query_param(&self) -> &dyn QueryParameter {
        match self {
            Self::Text(value) => value,
            Self::SmallInt(value) => value,
            Self::Int(value) => value,
            Self::BigInt(value) => value,
            Self::Bool(value) => value,
            Self::Date(value) => value,
            Self::Time(value) => value,
            Self::Timestamp(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Param>,
    /// Can run inside the plan's transaction (false for MySQL DDL, which commits implicitly).
    pub transactional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

/// What a dialect may look at while lowering an op.
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    pub server: ServerVersion,
    /// The connection's default schema (MySQL: its database), for ops on unqualified names.
    pub default_schema: &'a str,
    /// MySQL's `NO_BACKSLASH_ESCAPES` sql_mode, as introspection read it: a comment is
    /// escaped for it. Assumed the same on every pooled connection.
    pub no_backslash_escapes: bool,
    pub actual: &'a SchemaModel,
}

impl Ctx<'_> {
    pub fn actual_table(&self, table: &QualifiedName) -> Option<&TableDef> {
        self.actual.table(table)
    }

    pub fn actual_column(&self, table: &QualifiedName, column: &Ident) -> Option<&ColumnDef> {
        self.actual_table(table)?
            .columns
            .iter()
            .find(|c| &c.name == column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_kind_and_table() {
        let t = QualifiedName::parse("player").unwrap();
        let op = Op::DropColumn {
            table: t.clone(),
            column: Ident::new("age").unwrap(),
        };
        assert_eq!(op.kind(), "DropColumn");
        assert_eq!(op.table(), Some(&t));
        let schema = Op::CreateSchema {
            schema: Ident::new("s").unwrap(),
        };
        assert_eq!(schema.kind(), "CreateSchema");
        assert_eq!(schema.table(), None);
    }

    #[test]
    fn server_versions_compare() {
        let v = |a, b, c| ServerVersion {
            major: a,
            minor: b,
            patch: c,
        };
        assert!(v(8, 0, 16) < v(8, 4, 0));
        assert!(v(14, 0, 0) < v(17, 2, 0));
    }

    #[test]
    fn describe_is_stable_and_readable() {
        let t = QualifiedName::parse("player").unwrap();
        let age = Ident::new("age").unwrap();
        assert_eq!(
            Op::DropColumn {
                table: t.clone(),
                column: age.clone()
            }
            .describe(),
            "drop column player.age"
        );
        assert_eq!(Op::DropTable(t.clone()).describe(), "drop table player");
        assert_eq!(
            Op::FillColumn {
                table: t.clone(),
                column: age,
                value: Param::Int(0)
            }
            .describe(),
            "fill empty values of player.age"
        );
    }

    #[test]
    fn every_description() {
        let mut ops = crate::dialect::fixtures::all_ops();
        ops.push(("rebuild_table", crate::dialect::fixtures::rebuild_op()));
        let lines: Vec<String> = (ops.into_iter())
            .map(|(name, op)| format!("{name}: {}", op.describe()))
            .collect();
        insta::assert_snapshot!(lines.join("\n"));
    }

    #[test]
    fn confirm_keys_tell_apart_ops_that_read_alike() {
        // Catalog names aren't checked: both read "drop column t.x.y"
        let a = Op::DropColumn {
            table: QualifiedName::parse("t").unwrap(),
            column: Ident::from_catalog("x.y"),
        };
        let b = Op::DropColumn {
            table: QualifiedName {
                schema: None,
                name: Ident::from_catalog("t.x"),
            },
            column: Ident::from_catalog("y"),
        };
        assert_eq!(a.describe(), b.describe());
        assert_ne!(a.confirm_key(), b.confirm_key());
    }

    #[test]
    fn a_rebuild_is_confirmed_by_its_table() {
        let op = crate::dialect::fixtures::rebuild_op();
        let Op::RebuildTable { from, .. } = &op else {
            panic!()
        };
        assert_eq!(op.confirm_key(), ConfirmKey::rebuild(&from.name));
    }

    #[test]
    fn params_bind_their_own_value() {
        let text = Param::Text("x".into());
        let int = Param::Int(7);
        assert_eq!(format!("{:?}", text.as_query_param()), r#""x""#);
        assert_eq!(format!("{:?}", int.as_query_param()), "7");
    }
}
