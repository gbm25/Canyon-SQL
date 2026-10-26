//! What to change to bring a database to the model, and what stops it.

pub mod conflict;
pub mod decisions;
pub mod diff;
pub mod plan;
pub mod rename;
pub mod simulate;
pub mod validate;

#[cfg(test)]
mod proptests;

/// Helpers the planning tests share.
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) use crate::dialect::fixtures::{id, q};
    use crate::dialect::{fk_name, pk_name};
    use crate::model::TableDef;

    /// The table as introspection returns it: no entity, every constraint named.
    pub(crate) fn introspected(mut t: TableDef) -> TableDef {
        t.entity = None;
        if let Some(pk) = t.primary_key.as_mut() {
            pk.name.get_or_insert_with(|| pk_name(&t.name));
        }
        for fk in &mut t.foreign_keys {
            fk.name.get_or_insert_with(|| fk_name(&t.name, &fk.columns));
        }
        t
    }
}

pub use crate::model::{sql_type_words, type_words};
pub use conflict::{Conflict, ConflictReport, Info, Limit, ModelIssue, Warning};
pub use decisions::Decisions;
pub use diff::{Change, DiffOutput, KeyValue, Precheck, Risk, diff};
pub use plan::{Plan, PlannedOp, PrecheckResults, finalize, phase};
pub use rename::{
    LegacyRow, Previous, RENAME_TABLE_MIN_SHARED, Renames, previous_from_database, resolve,
};
pub use simulate::{apply_in_memory, satisfies};
pub use validate::{mysql_row_bytes, validate};

use crate::dialect::DdlDialect;
use crate::model::{Ident, ModelError, QualifiedName, SchemaModel};
use crate::op::Op;
use conflict::push_new;

/// What `prepare` found: the diff, the conflicts so far and the checks to run on the database.
#[derive(Debug)]
pub struct Prepared {
    pub diff: DiffOutput,
    /// From validation, the renames and the diff. Pass them to `finalize` as they are.
    pub conflicts: Vec<Conflict>,
    pub prechecks: Vec<Precheck>,
    /// The desired model with the default schema as `None`: write this to the snapshot.
    pub desired: SchemaModel,
    /// The decisions with the default schema as `None`, as the diff used them. Pass
    /// these to `finalize`.
    pub decisions: Decisions,
}

/// Validate, resolve the renames and diff. Collects every conflict instead of stopping
/// at the first, so the report has all of them.
///
/// `default_schema` is the connection's (`public`, `dbo`, the MySQL database): a table
/// in it is the same as one with no schema, in the desired model, the previous one and
/// the decisions. `existing_schemas` are the ones the database has; the others the
/// model uses get a `CreateSchema`.
pub fn prepare<D: DdlDialect>(
    desired: &SchemaModel,
    actual: &SchemaModel,
    previous: &Previous,
    decisions: &Decisions,
    default_schema: &str,
    existing_schemas: &[Ident],
) -> Prepared {
    let decisions = normalise_decisions(decisions, default_schema);
    let stop = |conflicts, decisions| Prepared {
        diff: DiffOutput::default(),
        conflicts,
        prechecks: Vec::new(),
        desired: desired.clone(),
        decisions,
    };
    let case = schema_case(desired, default_schema);
    if !case.is_empty() {
        return stop(case, decisions);
    }
    let desired = match normalise(desired, default_schema) {
        Ok(m) => m,
        Err(e) => {
            return stop(
                vec![Conflict::InvalidModel(ModelIssue::Model(e))],
                decisions,
            );
        }
    };
    let previous = match normalise(previous.model(), default_schema) {
        Ok(m) if previous.is_snapshot() => Previous::Snapshot(m),
        Ok(m) => Previous::Synthesized(m),
        Err(e) => {
            let detail =
                format!("the previous model, with `{default_schema}` as the default schema: {e}");
            return stop(vec![Conflict::Internal(detail)], decisions);
        }
    };
    let mut conflicts = validate::<D>(&desired, actual);
    let (renames, from_renames) = resolve(previous.model(), &desired, &decisions);
    conflicts.extend(from_renames);
    let mut diff = diff(&desired, actual, &previous, &renames, &decisions);
    let mut missing: Vec<&Ident> = (desired.tables.keys())
        .filter_map(|n| n.schema.as_ref())
        .filter(|s| !existing_schemas.contains(s))
        .collect();
    missing.sort();
    missing.dedup();
    let create: Vec<Change> = (missing.into_iter())
        .map(|schema| {
            Change::safe(Op::CreateSchema {
                schema: schema.clone(),
            })
        })
        .collect();
    diff.changes.splice(0..0, create);
    push_new(&mut conflicts, std::mem::take(&mut diff.conflicts));
    let mut prechecks: Vec<Precheck> = diff
        .changes
        .iter()
        .flat_map(|c| c.prechecks.iter().cloned())
        .collect();
    prechecks.sort();
    prechecks.dedup();
    Prepared {
        diff,
        conflicts,
        prechecks,
        desired,
        decisions,
    }
}

/// `n` with the default schema as `None`.
fn local(n: &QualifiedName, default_schema: &str) -> QualifiedName {
    QualifiedName {
        schema: n.schema.clone().filter(|s| s.as_str() != default_schema),
        name: n.name.clone(),
    }
}

/// The default schema becomes `None`, in table names and foreign key targets: a table
/// in it reads the same with or without the schema.
/// `public.player` and `player` both declared are the same table twice.
fn normalise(model: &SchemaModel, default_schema: &str) -> Result<SchemaModel, ModelError> {
    let tables = model.tables.values().cloned().map(|mut t| {
        t.name = local(&t.name, default_schema);
        for fk in &mut t.foreign_keys {
            fk.ref_table = local(&fk.ref_table, default_schema);
        }
        t
    });
    SchemaModel::from_tables(tables.collect())
}

/// Schemas written like the default one but in another case: one database would take
/// them as the default schema, another as a new one.
fn schema_case(model: &SchemaModel, default_schema: &str) -> Vec<Conflict> {
    let mut found: Vec<&Ident> = (model.tables.values())
        .flat_map(|t| {
            let targets = t.foreign_keys.iter().map(|fk| &fk.ref_table);
            std::iter::once(&t.name).chain(targets)
        })
        .filter_map(|n| n.schema.as_ref())
        .filter(|s| s.as_str() != default_schema && s.as_str().eq_ignore_ascii_case(default_schema))
        .collect();
    found.sort();
    found.dedup();
    found
        .into_iter()
        .map(|schema| {
            Conflict::InvalidModel(ModelIssue::SchemaCase {
                schema: schema.clone(),
                default: default_schema.to_owned(),
            })
        })
        .collect()
}

/// The table names in the answers, with the default schema as `None`.
fn normalise_decisions(decisions: &Decisions, default_schema: &str) -> Decisions {
    let l = |n: &QualifiedName| local(n, default_schema);
    let mut d = decisions.clone();
    d.confirmed = (decisions.confirmed.iter())
        .map(|key| {
            let mut key = key.clone();
            key.table = key.table.as_ref().map(l);
            // A move's target schema
            if key.kind == "MoveTable" {
                key.name = key.name.filter(|s| s.as_str() != default_schema);
            }
            key
        })
        .collect();
    d.table_renames = (decisions.table_renames.iter())
        .map(|(old, new)| (l(old), new.as_ref().map(l)))
        .collect();
    d.column_renames = (decisions.column_renames.iter())
        .map(|((t, c), new)| ((l(t), c.clone()), new.clone()))
        .collect();
    d.fill_values = (decisions.fill_values.iter())
        .map(|((t, c), v)| ((l(t), c.clone()), v.clone()))
        .collect();
    d.tighten = (decisions.tighten.iter())
        .map(|(t, c)| (l(t), c.clone()))
        .collect();
    d
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use super::test_support::q;
    use super::*;
    use crate::model::{Generated, TableDef};
    use canyon_core::query::querybuilder::syntax::dialect::PgDialect;
    use canyon_core::types::SqlType::*;

    #[test]
    fn prepare_then_finalize_returns_every_conflict() {
        let actual = SchemaModel::from_tables(vec![
            TableDef::builder("", "player")
                .column("id", Int, false)
                .primary_key(Some(Generated::Identity))
                .column("born", Date, true)
                .build()
                .unwrap(),
        ])
        .unwrap();
        let legacy = [LegacyRow {
            struct_name: "Player".into(),
            table: "player".into(),
        }];
        let previous = previous_from_database(&actual, &legacy);
        let desired = SchemaModel::from_tables(vec![
            TableDef::builder("Player", "player")
                .column("id", Int, false)
                .primary_key(Some(Generated::Identity))
                .column("score", Int, false)
                .build()
                .unwrap(),
            TableDef::builder("Match", "match")
                .column("id", Int, false)
                .primary_key(Some(Generated::Identity))
                .column("ghost", Int, true)
                .foreign_key("ghost", "id")
                .build()
                .unwrap(),
        ])
        .unwrap();
        let decisions = Decisions::from_config(true, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS);

        let prepared =
            prepare::<PgDialect>(&desired, &actual, &previous, &decisions, "public", &[]);
        assert_eq!(prepared.conflicts.len(), 2, "{:?}", prepared.conflicts);
        assert!(matches!(
            prepared.conflicts[0],
            Conflict::InvalidModel(ModelIssue::ForeignKeyTarget { .. })
        ));
        assert!(matches!(
            prepared.conflicts[1],
            Conflict::AmbiguousColumnRename { .. }
        ));
        assert_eq!(prepared.prechecks, [Precheck::TableHasRows(q("player"))]);

        let results = PrecheckResults([(Precheck::TableHasRows(q("player")), 3)].into());
        let conflicts = prepared.conflicts.clone();
        let err = finalize::<PgDialect>(prepared.diff, &results, &prepared.decisions, conflicts)
            .unwrap_err();
        assert_eq!(err.len(), 3, "{err:?}");
        assert_eq!(err[..2], prepared.conflicts[..]);
        assert!(matches!(err[2], Conflict::NotNullOnTableWithRows { .. }));
    }
    fn player(schema: Option<&str>) -> TableDef {
        let mut b = TableDef::builder("Player", "player");
        if let Some(s) = schema {
            b = b.schema(s);
        }
        b.column("id", Int, false)
            .primary_key(Some(Generated::Identity))
            .build()
            .unwrap()
    }
    fn prepare_pg(desired: &SchemaModel, actual: &SchemaModel, existing: &[Ident]) -> Prepared {
        prepare::<PgDialect>(
            desired,
            actual,
            &previous_from_database(actual, &[]),
            &Decisions::default(),
            "public",
            existing,
        )
    }

    #[test]
    fn the_default_schema_is_the_same_as_none() {
        let mut actual = player(None);
        actual.primary_key.as_mut().unwrap().name = Some(crate::dialect::pk_name(&actual.name));
        actual.entity = None;
        let actual = SchemaModel::from_tables(vec![actual]).unwrap();
        let desired = SchemaModel::from_tables(vec![player(Some("public"))]).unwrap();
        let prepared = prepare_pg(&desired, &actual, &[]);
        assert!(prepared.conflicts.is_empty(), "{:?}", prepared.conflicts);
        assert!(
            prepared.diff.changes.is_empty(),
            "{:?}",
            prepared.diff.changes
        );
    }

    #[test]
    fn a_table_with_and_without_the_default_schema_is_declared_twice() {
        // Another entity: `from_tables` already rejects one table name twice, not two names
        let mut other = player(None);
        other.entity = Some("Other".into());
        let desired = SchemaModel::from_tables(vec![player(Some("public")), other]).unwrap();
        let prepared = prepare_pg(&desired, &SchemaModel::default(), &[]);
        assert!(prepared.diff.changes.is_empty());
        assert!(
            matches!(
                &prepared.conflicts[..],
                [Conflict::InvalidModel(ModelIssue::Model(
                    crate::model::ModelError::DuplicateTable(t)
                ))] if t == "player"
            ),
            "{:?}",
            prepared.conflicts
        );
    }

    #[test]
    fn a_missing_schema_is_created_before_its_tables() {
        let desired = SchemaModel::from_tables(vec![
            player(Some("s")),
            TableDef::builder("League", "league")
                .schema("a")
                .column("id", Int, false)
                .build()
                .unwrap(),
            TableDef::builder("Team", "team")
                .schema("old")
                .column("id", Int, false)
                .build()
                .unwrap(),
        ])
        .unwrap();
        let actual = SchemaModel::default();
        let prepared = prepare_pg(&desired, &actual, &[Ident::new("old").unwrap()]);
        assert!(prepared.conflicts.is_empty(), "{:?}", prepared.conflicts);
        let plan = finalize::<PgDialect>(
            prepared.diff,
            &PrecheckResults::default(),
            &prepared.decisions,
            prepared.conflicts,
        )
        .unwrap();
        let lines: Vec<(u8, String)> = plan
            .ops
            .iter()
            .map(|p| (p.phase, p.op.describe()))
            .collect();
        assert_eq!(
            lines,
            [
                (0, "create schema a".to_string()),
                (0, "create schema s".to_string()),
                (3, "create table a.league".to_string()),
                (3, "create table old.team".to_string()),
                (3, "create table s.player".to_string()),
            ]
        );
    }
    fn nick(schema: Option<&str>, column: &str) -> TableDef {
        let mut b = TableDef::builder("Player", "player");
        if let Some(s) = schema {
            b = b.schema(s);
        }
        b.column("id", Int, false)
            .primary_key(None)
            .column(column, Text { max_len: None }, true)
            .build()
            .unwrap()
    }

    #[test]
    fn a_snapshot_with_the_default_schema_is_normalised_too() {
        let previous = Previous::Snapshot(
            SchemaModel::from_tables(vec![nick(Some("public"), "nick")]).unwrap(),
        );
        let mut have = nick(None, "nick");
        have.entity = None;
        have.primary_key.as_mut().unwrap().name = Some(crate::dialect::pk_name(&have.name));
        let actual = SchemaModel::from_tables(vec![have]).unwrap();
        let desired = SchemaModel::from_tables(vec![nick(None, "name")]).unwrap();
        let prepared = prepare::<PgDialect>(
            &desired,
            &actual,
            &previous,
            &Decisions::default(),
            "public",
            &[],
        );
        assert!(prepared.conflicts.is_empty(), "{:?}", prepared.conflicts);
        let ops: Vec<String> = (prepared.diff.changes.iter())
            .map(|c| c.op.describe())
            .collect();
        assert_eq!(ops, ["rename column player.nick to name"]);
    }

    #[test]
    fn prepared_desired_has_the_default_schema_as_none() {
        let desired = SchemaModel::from_tables(vec![player(Some("public"))]).unwrap();
        let prepared = prepare_pg(&desired, &SchemaModel::default(), &[]);
        assert_eq!(
            prepared.desired.tables.keys().collect::<Vec<_>>(),
            [&q("player")]
        );
    }

    #[test]
    fn the_default_schema_in_another_case_is_a_conflict() {
        let desired = SchemaModel::from_tables(vec![player(Some("PUBLIC"))]).unwrap();
        let prepared = prepare_pg(&desired, &SchemaModel::default(), &[]);
        assert!(prepared.diff.changes.is_empty());
        assert_eq!(
            prepared.conflicts,
            [Conflict::InvalidModel(ModelIssue::SchemaCase {
                schema: Ident::new("PUBLIC").unwrap(),
                default: "public".into(),
            })]
        );
    }

    #[test]
    fn answers_with_the_default_schema_are_normalised() {
        // Nothing in common: only the answer makes it a rename
        let a = TableDef::builder("A", "a")
            .column("x", Int, true)
            .build()
            .unwrap();
        let b = TableDef::builder("B", "b")
            .column("y", Date, true)
            .build()
            .unwrap();
        let mut have = a.clone();
        have.entity = None;
        let actual = SchemaModel::from_tables(vec![have]).unwrap();
        let previous = Previous::Snapshot(SchemaModel::from_tables(vec![a]).unwrap());
        let desired = SchemaModel::from_tables(vec![b]).unwrap();
        let mut decisions = Decisions::from_config(true, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS);
        decisions
            .table_renames
            .insert(q("public.a"), Some(q("public.b")));
        decisions.column_renames.insert(
            (q("public.b"), Ident::new("x").unwrap()),
            Some(Ident::new("y").unwrap()),
        );
        let prepared =
            prepare::<PgDialect>(&desired, &actual, &previous, &decisions, "public", &[]);
        let ops: Vec<String> = (prepared.diff.changes.iter())
            .map(|c| c.op.describe())
            .collect();
        assert_eq!(
            ops[..2],
            ["rename table a to b", "rename column b.x to y"],
            "{ops:?}"
        );
    }

    #[test]
    fn confirmations_with_the_default_schema_are_normalised() {
        let mut have = player(None);
        have.entity = None;
        have.primary_key.as_mut().unwrap().name = Some(crate::dialect::pk_name(&have.name));
        have.columns.push(nick(None, "nick").columns.remove(1));
        let actual = SchemaModel::from_tables(vec![have]).unwrap();
        let desired = SchemaModel::from_tables(vec![player(None)]).unwrap();
        let drop = Op::DropColumn {
            table: q("public.player"),
            column: Ident::new("nick").unwrap(),
        };
        let mut decisions = Decisions::default();
        decisions.confirmed.insert(drop.confirm_key());
        let prepared = prepare::<PgDialect>(
            &desired,
            &actual,
            &previous_from_database(&actual, &[]),
            &decisions,
            "public",
            &[],
        );
        let local = Op::DropColumn {
            table: q("player"),
            column: Ident::new("nick").unwrap(),
        };
        assert!(prepared.decisions.allows(&local));
        let plan = finalize::<PgDialect>(
            prepared.diff,
            &PrecheckResults::default(),
            &prepared.decisions,
            prepared.conflicts,
        )
        .unwrap_or_else(|c| panic!("{c:?}"));
        let ops: Vec<String> = plan.ops.iter().map(|p| p.op.describe()).collect();
        assert_eq!(ops, ["drop column player.nick"]);
    }
}
