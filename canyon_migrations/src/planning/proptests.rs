//! Properties over the whole planner: random models, a database that drifted from them
//! in random ways, and the plan has to bring it back.

use super::*;
use crate::dialect::DdlDialect;
use crate::model::*;
use canyon_core::types::SqlType;
use proptest::prelude::*;

fn sql_type() -> impl Strategy<Value = SqlType> {
    prop_oneof![
        Just(SqlType::Bool),
        Just(SqlType::SmallInt),
        Just(SqlType::Int),
        Just(SqlType::BigInt),
        Just(SqlType::Text { max_len: None }),
        (1u32..400).prop_map(|n| SqlType::Text { max_len: Some(n) }),
        Just(SqlType::Date),
        (0u8..=6).prop_map(|p| SqlType::Time { precision: p }),
        (0u8..=6).prop_map(|p| SqlType::Timestamp { precision: p }),
    ]
}

prop_compose! {
    fn table_def(index: usize)(
        cols in prop::collection::vec((sql_type(), any::<bool>()), 1..5),
        with_pk in any::<bool>(),
        identity in any::<bool>(),
        in_s1 in any::<bool>(),
    ) -> TableDef {
        let mut b = TableDef::builder(&format!("E{index}"), &format!("t{index}"));
        if in_s1 {
            b = b.schema("s1");
        }
        // A key column can't be nullable (`NullableKey`): only a keyless `id` may be
        b = b.column("id", SqlType::Int, !with_pk);
        if with_pk {
            b = b.primary_key(identity.then_some(Generated::Identity));
        }
        for (i, (ty, nullable)) in cols.into_iter().enumerate() {
            b = b.column(&format!("c{i}"), ty, nullable);
        }
        b.build().unwrap()
    }
}

prop_compose! {
    fn model()(a in table_def(0), b in table_def(1), link in any::<bool>()) -> SchemaModel {
        let mut tables = vec![a, b];
        // `t1.id` references `t0.id`, its key: both `Int`, so the model is valid
        if link && tables[0].primary_key.is_some() {
            let fk = ForeignKey {
                name: None,
                columns: vec![Ident::new("id").unwrap()],
                ref_table: tables[0].name.clone(),
                ref_columns: vec![Ident::new("id").unwrap()],
            };
            tables[1].foreign_keys.push(fk);
        }
        SchemaModel::from_tables(tables).unwrap()
    }
}

#[derive(Debug, Clone)]
enum Mutation {
    DropColumn(usize),
    ChangeType(usize, SqlType),
    FlipNull(usize),
    DropPk,
    ToggleIdentity,
    DropFks,
    DropTable,
    ExtraColumn,
    /// The table in the other schema (`s1` or none).
    MoveSchema,
    /// The table as `<name>_old`.
    RenameTable,
    /// The column as `<name>_old`.
    RenameColumn(usize),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (0usize..6).prop_map(Mutation::DropColumn),
        ((0usize..6), sql_type()).prop_map(|(i, t)| Mutation::ChangeType(i, t)),
        (0usize..6).prop_map(Mutation::FlipNull),
        Just(Mutation::DropPk),
        Just(Mutation::ToggleIdentity),
        Just(Mutation::DropFks),
        Just(Mutation::DropTable),
        Just(Mutation::ExtraColumn),
        Just(Mutation::MoveSchema),
        Just(Mutation::RenameTable),
        (0usize..6).prop_map(Mutation::RenameColumn),
    ]
}

/// `desired` after `muts`, each on the table `k % tables`.
fn mutate(desired: &SchemaModel, muts: &[Mutation]) -> SchemaModel {
    let mut m = desired.clone();
    for (k, mutation) in muts.iter().enumerate() {
        let names: Vec<QualifiedName> = m.tables.keys().cloned().collect();
        let Some(name) = names.get(k % names.len().max(1)) else {
            break;
        };
        match mutation {
            Mutation::DropTable => {
                if m.tables.len() > 1 {
                    m.tables.remove(name);
                }
                continue;
            }
            Mutation::MoveSchema | Mutation::RenameTable => {
                let to = match mutation {
                    Mutation::MoveSchema => QualifiedName {
                        schema: match name.schema {
                            Some(_) => None,
                            None => Some(Ident::new("s1").unwrap()),
                        },
                        name: name.name.clone(),
                    },
                    _ => QualifiedName {
                        schema: name.schema.clone(),
                        name: Ident::new(&format!("{}_old", name.name)).unwrap(),
                    },
                };
                if m.tables.contains_key(&to) {
                    continue;
                }
                let mut t = m.tables.remove(name).unwrap();
                t.name = to.clone();
                m.tables.insert(to.clone(), t);
                for fk in m.tables.values_mut().flat_map(|t| &mut t.foreign_keys) {
                    if &fk.ref_table == name {
                        fk.ref_table = to.clone();
                    }
                }
                continue;
            }
            Mutation::RenameColumn(i) => {
                let t = m.tables.get_mut(name).unwrap();
                let Some(old) = t.columns.get(*i).map(|c| c.name.clone()) else {
                    continue;
                };
                let new = Ident::new(&format!("{old}_old")).unwrap();
                if t.column(new.as_str()).is_some() {
                    continue;
                }
                let keys = t.primary_key.iter_mut().flat_map(|pk| &mut pk.columns);
                let fks = t.foreign_keys.iter_mut().flat_map(|fk| &mut fk.columns);
                let cols = t.columns.iter_mut().map(|c| &mut c.name);
                for c in cols.chain(keys).chain(fks).filter(|c| **c == old) {
                    *c = new.clone();
                }
                let refs = (m.tables.values_mut())
                    .flat_map(|t| &mut t.foreign_keys)
                    .filter(|fk| &fk.ref_table == name)
                    .flat_map(|fk| &mut fk.ref_columns);
                for c in refs.filter(|c| **c == old) {
                    *c = new.clone();
                }
                continue;
            }
            _ => {}
        }
        let t = m.tables.get_mut(name).unwrap();
        match mutation {
            Mutation::DropColumn(i) if *i < t.columns.len() && t.columns.len() > 1 => {
                let c = t.columns.remove(*i).name;
                if t.primary_key
                    .as_ref()
                    .is_some_and(|pk| pk.columns.contains(&c))
                {
                    t.primary_key = None;
                }
                t.foreign_keys.retain(|fk| !fk.columns.contains(&c));
            }
            Mutation::ChangeType(i, ty) if *i < t.columns.len() => {
                t.columns[*i].ty = Logical::Known(*ty)
            }
            Mutation::FlipNull(i) if *i < t.columns.len() => {
                t.columns[*i].nullable = !t.columns[*i].nullable
            }
            Mutation::DropPk => t.primary_key = None,
            Mutation::ToggleIdentity => {
                if let Some(pk) = t.primary_key.as_mut() {
                    pk.generated = match pk.generated {
                        Some(_) => None,
                        None => Some(Generated::Identity),
                    };
                }
            }
            Mutation::DropFks => t.foreign_keys.clear(),
            // A table can't have the column twice
            Mutation::ExtraColumn if t.column("extra").is_none() => t.columns.push(ColumnDef {
                name: Ident::new("extra").unwrap(),
                ty: Logical::Known(SqlType::Int),
                nullable: true,
                default: None,
                extra: ColumnExtra::default(),
            }),
            _ => {}
        }
    }
    m
}

/// `m` the way introspection reads it back: no structs, every constraint named.
fn introspected(m: &SchemaModel) -> SchemaModel {
    let tables = m.tables.values().cloned().map(test_support::introspected);
    SchemaModel::from_tables(tables.collect()).unwrap()
}

fn schemas_of(m: &SchemaModel) -> Vec<Ident> {
    let mut v: Vec<Ident> = m.tables.keys().filter_map(|n| n.schema.clone()).collect();
    v.sort();
    v.dedup();
    v
}

/// What the previous model is.
#[derive(Debug, Clone, Copy)]
enum Before {
    /// The last run wrote the model the database has now.
    SnapshotOfActual,
    /// The last run wrote `desired`; the database drifted since.
    SnapshotOfDesired,
    /// No snapshot: the database, with old `canyon_memory` rows for every table.
    Database,
}

#[derive(Clone)]
struct Scenario {
    desired: SchemaModel,
    actual: SchemaModel,
    previous: Previous,
}

/// One line per table: `s1.t0 (id Int!, c0 Text?) pk[id]+ fk[id -> t0]`.
fn show(m: &SchemaModel) -> String {
    let table = |t: &TableDef| {
        let cols: Vec<String> = (t.columns.iter())
            .map(|c| {
                let ty = match &c.ty {
                    Logical::Known(ty) => format!("{ty:?}"),
                    Logical::Other(n) => n.raw.as_str().to_owned(),
                };
                format!("{} {ty}{}", c.name, if c.nullable { "?" } else { "!" })
            })
            .collect();
        let pk = t.primary_key.as_ref().map(|pk| {
            let cols: Vec<&str> = pk.columns.iter().map(Ident::as_str).collect();
            let ident = if pk.generated.is_some() { "+" } else { "" };
            format!(
                " pk[{}]{ident} {:?}",
                cols.join(","),
                pk.name.as_ref().map(Ident::as_str)
            )
        });
        let fks: Vec<String> = (t.foreign_keys.iter())
            .map(|fk| {
                let cols: Vec<&str> = fk.columns.iter().map(Ident::as_str).collect();
                format!(
                    " fk[{} -> {}] {:?}",
                    cols.join(","),
                    fk.ref_table,
                    fk.name.as_ref().map(Ident::as_str)
                )
            })
            .collect();
        format!(
            "\n    {} {:?} ({}){}{}",
            t.name,
            t.entity,
            cols.join(", "),
            pk.unwrap_or_default(),
            fks.concat()
        )
    };
    m.tables.values().map(table).collect()
}

impl std::fmt::Debug for Scenario {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let previous = match &self.previous {
            Previous::Snapshot(m) => format!("snapshot{}", show(m)),
            Previous::Synthesized(m) => format!("database{}", show(m)),
        };
        write!(
            f,
            "\n  desired{}\n  actual{}\n  previous {previous}",
            show(&self.desired),
            show(&self.actual)
        )
    }
}

prop_compose! {
    fn scenario()(
        desired in model(),
        muts in prop::collection::vec(mutation(), 0..6),
        before in prop_oneof![
            Just(Before::SnapshotOfActual),
            Just(Before::SnapshotOfDesired),
            Just(Before::Database),
        ],
    ) -> Scenario {
        let drifted = mutate(&desired, &muts);
        let actual = introspected(&drifted);
        let previous = match before {
            Before::SnapshotOfActual => Previous::Snapshot(drifted),
            Before::SnapshotOfDesired => Previous::Snapshot(desired.clone()),
            Before::Database => {
                let legacy: Vec<LegacyRow> = (drifted.tables.values())
                    .map(|t| LegacyRow {
                        struct_name: t.entity.clone().unwrap(),
                        table: t.name.name.to_string(),
                    })
                    .collect();
                previous_from_database(&actual, &legacy)
            }
        };
        Scenario { desired, actual, previous }
    }
}

fn everything_allowed() -> Decisions {
    Decisions::from_config(true, u64::MAX)
}

/// Answers "not a rename" to every ambiguous rename in `conflicts`. `false` when there
/// was none.
fn not_renames(conflicts: &[Conflict], decisions: &mut Decisions) -> bool {
    let mut answered = false;
    for c in conflicts {
        match c {
            Conflict::AmbiguousTableRename { old, .. } => {
                answered |= decisions.table_renames.insert(old.clone(), None).is_none();
            }
            Conflict::AmbiguousColumnRename { table, removed, .. } => {
                for r in removed {
                    let key = (table.clone(), r.name.clone());
                    answered |= decisions.column_renames.insert(key, None).is_none();
                }
            }
            _ => {}
        }
    }
    answered
}

/// `prepare` then `finalize`, answering ambiguous renames with "not a rename". `results`
/// gives each precheck's count.
fn run<D: DdlDialect>(
    s: &Scenario,
    mut decisions: Decisions,
    results: impl Fn(&Precheck) -> u64,
) -> Result<Plan, Vec<Conflict>> {
    loop {
        let prepared = prepare::<D>(
            &s.desired,
            &s.actual,
            &s.previous,
            &decisions,
            "public",
            &schemas_of(&s.actual),
        );
        if not_renames(&prepared.conflicts, &mut decisions) {
            continue;
        }
        let results = PrecheckResults(
            (prepared.prechecks.iter())
                .map(|p| (p.clone(), results(p)))
                .collect(),
        );
        return finalize::<D>(
            prepared.diff,
            &results,
            &prepared.decisions,
            prepared.conflicts,
        );
    }
}

fn ops(plan: &Plan) -> Vec<Op> {
    plan.ops.iter().map(|p| p.op.clone()).collect()
}

/// With every destructive op allowed and an empty database there is always a plan, it
/// applies and its result meets `desired`.
fn reaches<D: DdlDialect>(s: &Scenario) -> Result<Plan, TestCaseError> {
    let plan = run::<D>(s, everything_allowed(), |_| 0)
        .map_err(|c| TestCaseError::fail(format!("unexpected conflicts: {c:?}")))?;
    let result = apply_in_memory(&s.actual, &ops(&plan)).map_err(TestCaseError::fail)?;
    satisfies(&result, &s.desired).map_err(TestCaseError::fail)?;
    let phases: Vec<u8> = plan.ops.iter().map(|p| p.phase).collect();
    prop_assert!(phases.is_sorted(), "phases go backwards: {phases:?}");
    Ok(plan)
}

/// Planning again against the database the plan left finds nothing to do.
fn idempotent<D: DdlDialect>(s: &Scenario) -> Result<(), TestCaseError> {
    let plan = reaches::<D>(s)?;
    let result = apply_in_memory(&s.actual, &ops(&plan)).map_err(TestCaseError::fail)?;
    let again = Scenario {
        desired: s.desired.clone(),
        actual: introspected(&result),
        previous: Previous::Snapshot(s.desired.clone()),
    };
    let prepared = prepare::<D>(
        &again.desired,
        &again.actual,
        &again.previous,
        &Decisions::default(),
        "public",
        &schemas_of(&again.actual),
    );
    prop_assert!(
        prepared.conflicts.is_empty(),
        "conflicts: {:?}",
        prepared.conflicts
    );
    let left: Vec<String> = (prepared.diff.changes.iter())
        .map(|c| c.op.describe())
        .collect();
    prop_assert!(left.is_empty(), "left to do: {left:?}");
    Ok(())
}

/// With any precheck results and either flag value, `finalize` gives conflicts or a
/// whole plan, never a part of one.
fn whole_or_nothing<D: DdlDialect>(
    s: &Scenario,
    allow: bool,
    counts: &[u64],
) -> Result<(), TestCaseError> {
    let decisions = Decisions::from_config(allow, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS);
    // The same precheck gets the same count; the counts repeat over the prechecks
    let count = |p: &Precheck| {
        let key = format!("{p:?}");
        let h = key
            .bytes()
            .fold(0usize, |h, b| h.wrapping_mul(31) + b as usize);
        counts[h % counts.len()]
    };
    match run::<D>(s, decisions, count) {
        Ok(plan) => {
            let result = apply_in_memory(&s.actual, &ops(&plan)).map_err(TestCaseError::fail)?;
            satisfies(&result, &s.desired).map_err(TestCaseError::fail)?;
            if !allow {
                prop_assert!(
                    plan.ops.iter().all(|p| p.risk != Risk::Destructive),
                    "destructive op without the flag: {:?}",
                    plan.ops
                );
            }
        }
        Err(conflicts) => {
            prop_assert!(!conflicts.is_empty());
            prop_assert!(
                conflicts
                    .iter()
                    .all(|c| !matches!(c, Conflict::Internal(_))),
                "{conflicts:?}"
            );
        }
    }
    Ok(())
}

/// Every precheck a change needs is in `Prepared.prechecks`, the list the runner asks
/// the database: a missing one would count as 0 rows and let the change through.
fn every_precheck_is_asked<D: DdlDialect>(s: &Scenario) -> Result<(), TestCaseError> {
    let prepared = prepare::<D>(
        &s.desired,
        &s.actual,
        &s.previous,
        &everything_allowed(),
        "public",
        &schemas_of(&s.actual),
    );
    for change in &prepared.diff.changes {
        for p in &change.prechecks {
            prop_assert!(
                prepared.prechecks.contains(p),
                "{p:?} of \"{}\" isn't asked",
                change.op.describe()
            );
        }
    }
    Ok(())
}

fn counts() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(prop_oneof![3 => Just(0u64), 1 => 1u64..1_000_000], 1..6)
}

fn no_changes_for_an_identical_model<D: DdlDialect>(m: &SchemaModel) -> Result<(), TestCaseError> {
    let actual = introspected(m);
    let prepared = prepare::<D>(
        m,
        &actual,
        &Previous::Snapshot(m.clone()),
        &Decisions::default(),
        "public",
        &schemas_of(&actual),
    );
    prop_assert!(prepared.conflicts.is_empty(), "{:?}", prepared.conflicts);
    prop_assert!(
        prepared.diff.changes.is_empty(),
        "{:?}",
        prepared.diff.changes
    );
    Ok(())
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use canyon_core::query::querybuilder::syntax::dialect::PgDialect as D;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn no_changes_for_an_identical_model(m in model()) {
            super::no_changes_for_an_identical_model::<D>(&m)?;
        }

        #[test]
        fn plan_reaches_the_desired_model(s in scenario()) {
            reaches::<D>(&s)?;
        }

        #[test]
        fn planning_again_finds_nothing(s in scenario()) {
            idempotent::<D>(&s)?;
        }

        #[test]
        fn every_precheck_is_asked(s in scenario()) {
            super::every_precheck_is_asked::<D>(&s)?;
        }

        #[test]
        fn conflicts_or_a_whole_plan(s in scenario(), allow in any::<bool>(), counts in counts()) {
            whole_or_nothing::<D>(&s, allow, &counts)?;
        }
    }
}

#[cfg(feature = "mssql")]
mod sql_server {
    use super::*;
    use canyon_core::query::querybuilder::syntax::dialect::MsSql as D;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn no_changes_for_an_identical_model(m in model()) {
            super::no_changes_for_an_identical_model::<D>(&m)?;
        }

        #[test]
        fn plan_reaches_the_desired_model(s in scenario()) {
            let plan = reaches::<D>(&s)?;
            // SQL Server can't add or drop an identity: the table is rebuilt instead
            let identity = |p: &PlannedOp| matches!(p.op, Op::AddIdentity { .. } | Op::DropIdentity { .. });
            prop_assert!(!plan.ops.iter().any(identity), "{:?}", plan.ops);
        }

        #[test]
        fn planning_again_finds_nothing(s in scenario()) {
            idempotent::<D>(&s)?;
        }

        #[test]
        fn every_precheck_is_asked(s in scenario()) {
            super::every_precheck_is_asked::<D>(&s)?;
        }

        #[test]
        fn conflicts_or_a_whole_plan(s in scenario(), allow in any::<bool>(), counts in counts()) {
            whole_or_nothing::<D>(&s, allow, &counts)?;
        }
    }
}

#[cfg(feature = "mysql")]
mod mysql {
    use super::*;
    use canyon_core::query::querybuilder::syntax::dialect::MySql as D;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn no_changes_for_an_identical_model(m in model()) {
            super::no_changes_for_an_identical_model::<D>(&m)?;
        }

        #[test]
        fn plan_reaches_the_desired_model(s in scenario()) {
            reaches::<D>(&s)?;
        }

        #[test]
        fn planning_again_finds_nothing(s in scenario()) {
            idempotent::<D>(&s)?;
        }

        #[test]
        fn every_precheck_is_asked(s in scenario()) {
            super::every_precheck_is_asked::<D>(&s)?;
        }

        #[test]
        fn conflicts_or_a_whole_plan(s in scenario(), allow in any::<bool>(), counts in counts()) {
            whole_or_nothing::<D>(&s, allow, &counts)?;
        }
    }
}

// Shrunk cases the properties found, kept as plain tests.

#[cfg(feature = "mssql")]
fn table(name: &str, cols: &[(&str, SqlType, bool)], pk: Option<Option<Generated>>) -> TableDef {
    let mut b = TableDef::builder(&name.replace('t', "E"), name);
    for (c, ty, nullable) in cols {
        b = b.column(c, *ty, *nullable);
        if *c == "id"
            && let Some(generated) = pk
        {
            b = b.primary_key(generated);
        }
    }
    b.build().unwrap()
}

/// SQL Server: a key held back by duplicates left its identity to the rebuild, which
/// then reported a planner bug on top of the real conflict.
#[cfg(feature = "mssql")]
#[test]
fn a_held_back_key_is_not_a_planner_bug() {
    use canyon_core::query::querybuilder::syntax::dialect::MsSql;
    let cols = [("id", SqlType::Int, false), ("c0", SqlType::Bool, false)];
    let desired =
        SchemaModel::from_tables(vec![table("t1", &cols, Some(Some(Generated::Identity)))])
            .unwrap();
    let before = SchemaModel::from_tables(vec![table("t1", &cols, None)]).unwrap();
    let s = Scenario {
        actual: introspected(&before),
        previous: Previous::Snapshot(before),
        desired,
    };
    let err = run::<MsSql>(&s, Decisions::default(), |_| 1).unwrap_err();
    assert!(
        matches!(&err[..], [Conflict::DuplicateKeyValues { .. }]),
        "{err:?}"
    );
}
