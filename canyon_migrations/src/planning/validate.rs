//! Checks on the desired model before anything is compared.

use super::conflict::{Conflict, Limit, ModelIssue};
use crate::dialect::{DdlDialect, fk_name};
use crate::model::{Ident, Logical, QualifiedName, SchemaModel, TableDef};
use canyon_core::types::SqlType;
use std::collections::{BTreeMap, btree_map::Entry};

/// Checks on the desired model before anything is compared, with `D`'s limits: table
/// names that differ only in case, nullable key columns, lengths and precisions,
/// foreign keys (target, types, the target's primary key, unique names) and the row
/// size.
pub fn validate<D: DdlDialect>(desired: &SchemaModel, actual: &SchemaModel) -> Vec<Conflict> {
    let mut out = Vec::new();
    let mut push = |issue| out.push(Conflict::InvalidModel(issue));

    // Compares `schema.table`, whole
    let tables: Vec<&TableDef> = desired.tables.values().collect();
    for (i, a) in tables.iter().enumerate() {
        for b in &tables[i + 1..] {
            if a.name.to_string().eq_ignore_ascii_case(&b.name.to_string()) {
                push(ModelIssue::CaseCollision {
                    first: a.name.clone(),
                    second: b.name.clone(),
                });
            }
        }
    }

    for table in &tables {
        let key_columns: Vec<_> = table
            .primary_key
            .iter()
            .flat_map(|pk| pk.columns.iter())
            .chain(table.foreign_keys.iter().flat_map(|fk| fk.columns.iter()))
            .collect();
        // PostgreSQL and MySQL make a key column NOT NULL on their own, SQL Server refuses
        for c in &table.columns {
            if c.nullable
                && table
                    .primary_key
                    .as_ref()
                    .is_some_and(|pk| pk.columns.contains(&c.name))
            {
                push(ModelIssue::NullableKey {
                    table: table.name.clone(),
                    column: c.name.clone(),
                });
            }
        }
        for column in &table.columns {
            let Logical::Known(ty) = column.ty else {
                continue;
            };
            let is_key = key_columns.contains(&&column.name);
            let limit = match ty {
                SqlType::Text { max_len: Some(0) } => Some(Limit::ZeroLength),
                SqlType::Text { max_len: None } if is_key && D::TEXT_KEY_NEEDS_LENGTH => {
                    Some(Limit::TextKeyNeedsLength)
                }
                SqlType::Text { max_len: Some(n) } => (D::MAX_KEY_LENGTH)
                    .filter(|max| is_key && n > *max)
                    .map(Limit::KeyLength)
                    .or((n > D::MAX_LENGTH).then_some(Limit::Length(D::MAX_LENGTH))),
                SqlType::Time { precision } | SqlType::Timestamp { precision }
                    if precision > D::MAX_PRECISION =>
                {
                    Some(Limit::Precision(D::MAX_PRECISION))
                }
                _ => None,
            };
            if let Some(limit) = limit {
                push(ModelIssue::Limit {
                    table: table.name.clone(),
                    column: column.name.clone(),
                    limit,
                    database: D::NAME,
                });
            }
        }
        for fk in &table.foreign_keys {
            let target = desired
                .table(&fk.ref_table)
                .or_else(|| actual.table(&fk.ref_table));
            for (column, target_column) in fk.columns.iter().zip(&fk.ref_columns) {
                let target_col =
                    target.and_then(|t| t.columns.iter().find(|c| &c.name == target_column));
                match (target_col, table.columns.iter().find(|c| &c.name == column)) {
                    (None, _) => push(ModelIssue::ForeignKeyTarget {
                        table: table.name.clone(),
                        column: column.clone(),
                        target: fk.ref_table.clone(),
                        target_column: target_column.clone(),
                    }),
                    (Some(t), Some(c)) if t.ty != c.ty => push(ModelIssue::ForeignKeyType {
                        table: table.name.clone(),
                        column: column.clone(),
                        target: fk.ref_table.clone(),
                        target_column: target_column.clone(),
                    }),
                    _ => {}
                }
            }
            // Canyon has no unique constraints: the primary key is the only key a foreign
            // key can point at. Same columns in any order (the pairs fix the mapping);
            // a missing column is reported above already.
            if let Some(t) = target
                && fk
                    .ref_columns
                    .iter()
                    .all(|r| t.columns.iter().any(|c| &c.name == r))
            {
                let is_key = t.primary_key.as_ref().is_some_and(|pk| {
                    pk.columns.len() == fk.ref_columns.len()
                        && fk.ref_columns.iter().all(|r| pk.columns.contains(r))
                });
                if !is_key {
                    push(ModelIssue::ForeignKeyNotKey {
                        table: table.name.clone(),
                        columns: fk.columns.clone(),
                        target: fk.ref_table.clone(),
                    });
                }
            }
        }
        if let Some(max) = D::MAX_ROW_BYTES {
            let bytes = mysql_row_bytes(table);
            if bytes > max {
                push(ModelIssue::RowTooLarge {
                    table: table.name.clone(),
                    bytes,
                    max,
                    database: D::NAME,
                });
            }
        }
    }

    // Constraint names are unique per schema on SQL Server and per database on MySQL
    let mut fk_names: BTreeMap<(Option<Ident>, Ident), QualifiedName> = BTreeMap::new();
    for table in &tables {
        for fk in &table.foreign_keys {
            let name = fk
                .name
                .clone()
                .unwrap_or_else(|| fk_name(&table.name, &fk.columns));
            match fk_names.entry((table.name.schema.clone(), name.clone())) {
                Entry::Vacant(slot) => {
                    slot.insert(table.name.clone());
                }
                Entry::Occupied(slot) => push(ModelIssue::DuplicateForeignKeyName {
                    name,
                    first: slot.get().clone(),
                    second: table.name.clone(),
                }),
            }
        }
    }
    out
}

/// Worst-case row size MySQL checks at CREATE (utf8mb4: 4 bytes per character).
pub fn mysql_row_bytes(table: &TableDef) -> u64 {
    table
        .columns
        .iter()
        .map(|c| match c.ty {
            Logical::Known(SqlType::Bool) => 1,
            Logical::Known(SqlType::SmallInt) => 2,
            Logical::Known(SqlType::Int) => 4,
            Logical::Known(SqlType::BigInt) => 8,
            Logical::Known(SqlType::Text { max_len: Some(n) }) => 4 * u64::from(n) + 2,
            Logical::Known(SqlType::Text { max_len: None }) => 12,
            Logical::Known(SqlType::Date) => 3,
            Logical::Known(SqlType::Time { precision }) => 3 + u64::from(precision).div_ceil(2),
            Logical::Known(SqlType::Timestamp { precision }) => {
                5 + u64::from(precision).div_ceil(2)
            }
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SchemaModel, TableDef};
    #[cfg(feature = "mssql")]
    use canyon_core::query::querybuilder::syntax::dialect::MsSql;
    #[cfg(feature = "mysql")]
    use canyon_core::query::querybuilder::syntax::dialect::MySql;
    #[cfg(feature = "postgres")]
    use canyon_core::query::querybuilder::syntax::dialect::PgDialect;
    use canyon_core::types::SqlType;

    fn model(tables: Vec<TableDef>) -> SchemaModel {
        SchemaModel::from_tables(tables).unwrap()
    }

    fn issues<D: DdlDialect>(m: &SchemaModel) -> Vec<Conflict> {
        validate::<D>(m, &SchemaModel::default())
    }

    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn text_key(len: Option<u32>) -> SchemaModel {
        model(vec![
            TableDef::builder("T", "t")
                .column("slug", SqlType::Text { max_len: len }, false)
                .primary_key(None)
                .build()
                .unwrap(),
        ])
    }

    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn text_column(len: u32) -> SchemaModel {
        model(vec![
            TableDef::builder("T", "t")
                .column("body", SqlType::Text { max_len: Some(len) }, true)
                .build()
                .unwrap(),
        ])
    }

    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn timestamp_column(precision: u8) -> SchemaModel {
        model(vec![
            TableDef::builder("T", "t")
                .column("at", SqlType::Timestamp { precision }, true)
                .build()
                .unwrap(),
        ])
    }

    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn is_limit(found: &[Conflict]) -> bool {
        matches!(found, [Conflict::InvalidModel(ModelIssue::Limit { .. })])
    }

    #[test]
    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn text_key_without_length_is_fine_on_postgres_only() {
        let m = text_key(None);
        assert!(issues::<PgDialect>(&m).is_empty());
        assert!(is_limit(&issues::<MsSql>(&m)));
        assert!(is_limit(&issues::<MySql>(&m)));
    }

    #[test]
    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn key_length_limits() {
        let m = text_key(Some(450));
        assert!(issues::<MsSql>(&m).is_empty());
        assert!(issues::<MySql>(&m).is_empty());
        let m = text_key(Some(451));
        assert!(is_limit(&issues::<MsSql>(&m)));
        assert!(issues::<MySql>(&m).is_empty());
        assert!(issues::<PgDialect>(&m).is_empty());
        let m = text_key(Some(769));
        assert!(is_limit(&issues::<MySql>(&m)));
    }

    #[test]
    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn length_limits() {
        let m = text_column(4000);
        assert!(issues::<MsSql>(&m).is_empty());
        assert!(is_limit(&issues::<MsSql>(&text_column(4001))));
        assert!(issues::<MySql>(&text_column(16_383)).is_empty());
        // A column that long also overflows the MySQL row
        assert!(matches!(
            issues::<MySql>(&text_column(16_384))[..],
            [
                Conflict::InvalidModel(ModelIssue::Limit { .. }),
                Conflict::InvalidModel(ModelIssue::RowTooLarge { .. })
            ]
        ));
        assert!(issues::<PgDialect>(&text_column(16_384)).is_empty());
    }

    #[test]
    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn precision_limits() {
        assert!(issues::<MsSql>(&timestamp_column(7)).is_empty());
        assert!(is_limit(&issues::<MsSql>(&timestamp_column(8))));
        assert!(issues::<PgDialect>(&timestamp_column(6)).is_empty());
        assert!(is_limit(&issues::<PgDialect>(&timestamp_column(7))));
        assert!(is_limit(&issues::<MySql>(&timestamp_column(7))));
    }

    #[test]
    #[cfg(all(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn limit_messages_name_this_backend_and_its_number() {
        let text = |found: Vec<Conflict>| found.iter().map(|c| c.to_string()).collect::<Vec<_>>();
        assert_eq!(
            text(issues::<MsSql>(&text_key(Some(451)))),
            ["t.slug: text keys are limited to 450 characters on SQL Server."]
        );
        assert_eq!(
            text(issues::<MySql>(&text_key(Some(769)))),
            ["t.slug: text keys are limited to 768 characters on MySQL."]
        );
        assert_eq!(
            text(issues::<MsSql>(&text_column(4001))),
            ["t.body: `length` is limited to 4000 on SQL Server; use `String` without `length`."]
        );
        assert_eq!(
            text(issues::<PgDialect>(&timestamp_column(7))),
            ["t.at: `precision` is limited to 6 on PostgreSQL."]
        );
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn zero_length_is_refused() {
        let m = model(vec![
            TableDef::builder("T", "t")
                .column("body", SqlType::Text { max_len: Some(0) }, true)
                .build()
                .unwrap(),
        ]);
        assert_eq!(
            issues::<PgDialect>(&m),
            [Conflict::InvalidModel(ModelIssue::Limit {
                table: QualifiedName::parse("t").unwrap(),
                column: Ident::new("body").unwrap(),
                limit: Limit::ZeroLength,
                database: "PostgreSQL",
            })]
        );
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn a_nullable_key_column_is_refused() {
        let m = model(vec![
            TableDef::builder("T", "t")
                .column("id", SqlType::Int, true)
                .primary_key(None)
                .build()
                .unwrap(),
        ]);
        assert_eq!(
            issues::<PgDialect>(&m),
            [Conflict::InvalidModel(ModelIssue::NullableKey {
                table: QualifiedName::parse("t").unwrap(),
                column: Ident::new("id").unwrap(),
            })]
        );
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn case_collision() {
        let m = model(vec![
            TableDef::builder("A", "player")
                .column("id", SqlType::Int, false)
                .build()
                .unwrap(),
            TableDef::builder("B", "Player")
                .column("id", SqlType::Int, false)
                .build()
                .unwrap(),
        ]);
        assert!(matches!(
            issues::<PgDialect>(&m)[..],
            [Conflict::InvalidModel(ModelIssue::CaseCollision { .. })]
        ));
    }

    #[cfg(feature = "postgres")]
    fn league() -> TableDef {
        TableDef::builder("League", "league")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .build()
            .unwrap()
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn fk_target_missing() {
        let player = TableDef::builder("Player", "player")
            .column("league", SqlType::Int, true)
            .foreign_key("league", "id")
            .build()
            .unwrap();
        let m = model(vec![player.clone()]);
        assert!(matches!(
            issues::<PgDialect>(&m)[..],
            [Conflict::InvalidModel(ModelIssue::ForeignKeyTarget { .. })]
        ));
        // A table only the database has is a valid target
        let actual = model(vec![league()]);
        assert!(validate::<PgDialect>(&m, &actual).is_empty());
        // A missing column on an existing table is not
        let m = model(vec![
            league(),
            TableDef::builder("Player", "player")
                .column("league", SqlType::Int, true)
                .foreign_key("league", "code")
                .build()
                .unwrap(),
        ]);
        assert!(matches!(
            issues::<PgDialect>(&m)[..],
            [Conflict::InvalidModel(ModelIssue::ForeignKeyTarget { .. })]
        ));
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn fk_must_reference_the_primary_key() {
        let player = |target: &str| {
            TableDef::builder("Player", "player")
                .column("league", SqlType::Int, true)
                .foreign_key("league", target)
                .build()
                .unwrap()
        };
        let keyed = TableDef::builder("League", "league")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .column("code", SqlType::Int, false)
            .build()
            .unwrap();
        let m = model(vec![keyed.clone(), player("code")]);
        assert!(matches!(
            issues::<PgDialect>(&m)[..],
            [Conflict::InvalidModel(ModelIssue::ForeignKeyNotKey { .. })]
        ));
        assert!(issues::<PgDialect>(&model(vec![keyed.clone(), player("id")])).is_empty());
        // No primary key at all; the target may come from the database
        let keyless = TableDef::builder("League", "league")
            .column("id", SqlType::Int, false)
            .build()
            .unwrap();
        let m = model(vec![player("id")]);
        assert!(matches!(
            validate::<PgDialect>(&m, &model(vec![keyless]))[..],
            [Conflict::InvalidModel(ModelIssue::ForeignKeyNotKey { .. })]
        ));
        assert!(validate::<PgDialect>(&m, &model(vec![keyed])).is_empty());
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn fk_type_mismatch() {
        let m = model(vec![
            league(),
            TableDef::builder("Player", "player")
                .column("league", SqlType::BigInt, true)
                .foreign_key("league", "id")
                .build()
                .unwrap(),
        ]);
        assert!(matches!(
            issues::<PgDialect>(&m)[..],
            [Conflict::InvalidModel(ModelIssue::ForeignKeyType { .. })]
        ));
    }

    #[test]
    #[cfg(feature = "mysql")]
    fn mysql_row_size() {
        let mut b = TableDef::builder("T", "t");
        for i in 0..5 {
            b = b.column(
                &format!("c{i}"),
                SqlType::Text {
                    max_len: Some(4000),
                },
                true,
            );
        }
        let m = model(vec![b.build().unwrap()]);
        assert!(matches!(
            issues::<MySql>(&m)[..],
            [Conflict::InvalidModel(ModelIssue::RowTooLarge { .. })]
        ));
        assert_eq!(
            mysql_row_bytes(&m.tables.values().next().unwrap().clone()),
            5 * 16_002
        );
    }

    #[cfg(feature = "postgres")]
    fn self_reference(table: &str, column: &str, schema: Option<&str>) -> TableDef {
        let mut b = TableDef::builder("E", table);
        if let Some(s) = schema {
            b = b.schema(s);
        }
        let target = match schema {
            Some(s) => format!("{s}.{table}"),
            None => table.to_owned(),
        };
        b.column("id", SqlType::Int, false)
            .primary_key(None)
            .column(column, SqlType::Int, true)
            .foreign_key(&target, "id")
            .build()
            .unwrap()
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn duplicate_fk_name_in_one_schema() {
        // `a_b`.`c` and `a`.`b_c` both give `a_b_c_fkey`
        let m = model(vec![
            self_reference("a_b", "c", None),
            self_reference("a", "b_c", None),
        ]);
        assert!(matches!(
            issues::<PgDialect>(&m)[..],
            [Conflict::InvalidModel(
                ModelIssue::DuplicateForeignKeyName { .. }
            )]
        ));
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn same_fk_name_in_two_schemas_is_fine() {
        let m = model(vec![
            self_reference("a", "b_c", Some("one")),
            self_reference("a", "b_c", Some("two")),
        ]);
        assert!(issues::<PgDialect>(&m).is_empty());
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn valid_model_has_no_issues_postgres() {
        assert!(issues::<PgDialect>(&valid()).is_empty());
    }

    #[test]
    #[cfg(feature = "mssql")]
    fn valid_model_has_no_issues_mssql() {
        assert!(issues::<MsSql>(&valid()).is_empty());
    }

    #[test]
    #[cfg(feature = "mysql")]
    fn valid_model_has_no_issues_mysql() {
        assert!(issues::<MySql>(&valid()).is_empty());
    }

    #[cfg(any(feature = "postgres", feature = "mssql", feature = "mysql"))]
    fn valid() -> SchemaModel {
        model(vec![
            TableDef::builder("League", "league")
                .column("id", SqlType::Int, false)
                .primary_key(None)
                .column("name", SqlType::Text { max_len: Some(100) }, false)
                .build()
                .unwrap(),
            TableDef::builder("Player", "player")
                .column("id", SqlType::Int, false)
                .primary_key(None)
                .column("league", SqlType::Int, true)
                .foreign_key("league", "id")
                .column("seen", SqlType::Timestamp { precision: 3 }, true)
                .build()
                .unwrap(),
        ])
    }
}
