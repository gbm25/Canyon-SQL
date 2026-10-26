//! Changes + precheck results + decisions to an ordered plan, or every conflict.

use super::conflict::{Conflict, Info, Warning, push_new};
use super::decisions::Decisions;
use super::diff::{Change, DiffOutput, Precheck, Risk, name_in_db};
use super::simulate::apply_in_memory;
use crate::dialect::{DdlDialect, fk_name, pk_name};
use crate::model::{
    ColumnDef, ForeignKey, Generated, Ident, Length, Logical, PrimaryKey, QualifiedName,
    SchemaModel,
};
use crate::op::{ConfirmKey, Op, Param, rebuild_description};
use canyon_core::types::SqlType;
use std::collections::{BTreeMap, BTreeSet};

/// What each precheck found: rows (0 = fine), the rows up to 2 for `RowCount`, or the
/// estimated rows for `RowEstimate`.
#[derive(Debug, Clone, Default)]
pub struct PrecheckResults(pub BTreeMap<Precheck, u64>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedOp {
    pub phase: u8,
    pub op: Op,
    pub risk: Risk,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub ops: Vec<PlannedOp>,
    pub infos: Vec<Info>,
    pub warnings: Vec<Warning>,
}

impl PrecheckResults {
    fn value(&self, p: &Precheck) -> u64 {
        self.0.get(p).copied().unwrap_or(0)
    }
}

/// The phase an op runs in. Each phase only needs what the ones before it did:
///
/// 0. create schemas
/// 1. drop foreign keys (by the names the database has now)
/// 2. move tables, rename tables, rename columns
/// 3. create tables
/// 4. add columns, fill NULLs
/// 5. drop identities
/// 6. drop primary keys
/// 7. change types and nullability
/// 8. add primary keys
/// 9. add identities, rebuild tables (SQL Server)
/// 10. drop columns and tables
/// 11. add foreign keys
pub fn phase(op: &Op) -> u8 {
    match op {
        Op::CreateSchema { .. } => 0,
        Op::DropForeignKey { .. } => 1,
        Op::MoveTable { .. } | Op::RenameTable { .. } | Op::RenameColumn { .. } => 2,
        Op::CreateTable(_) => 3,
        Op::AddColumn { .. } | Op::FillColumn { .. } => 4,
        Op::DropIdentity { .. } => 5,
        Op::DropPrimaryKey { .. } => 6,
        Op::AlterColumnType { .. } | Op::SetNotNull { .. } | Op::DropNotNull { .. } => 7,
        Op::AddPrimaryKey { .. } => 8,
        Op::AddIdentity { .. } | Op::RebuildTable { .. } => 9,
        Op::DropColumn { .. } | Op::DropTable(_) => 10,
        Op::AddForeignKey { .. } => 11,
    }
}

fn rank(op: &Op) -> u8 {
    match op {
        Op::RenameTable { .. } | Op::FillColumn { .. } => 1,
        Op::RenameColumn { .. } => 2,
        _ => 0,
    }
}

/// Phase, rank, then table. Phase 2 keeps the diff's order: a rename that frees a name
/// runs before the one that takes it.
fn order(op: &Op) -> (u8, u8, Option<&QualifiedName>) {
    let p = phase(op);
    (p, rank(op), if p == 2 { None } else { op.table() })
}

type Ops = Vec<(Op, Risk)>;

/// Turns the diff's changes into an ordered plan, or returns every conflict: `conflicts`
/// (from validation and renames), the diff's own, then the planner's. Never a partial plan.
pub fn finalize<D: DdlDialect>(
    diff: DiffOutput,
    results: &PrecheckResults,
    decisions: &Decisions,
    mut conflicts: Vec<Conflict>,
) -> Result<Plan, Vec<Conflict>> {
    let DiffOutput {
        changes,
        infos,
        warnings,
        after_renames,
        actual,
        old_names,
        conflicts: from_diff,
    } = diff;
    push_new(&mut conflicts, from_diff);
    let names = Names {
        actual: &actual,
        old_names: &old_names,
    };

    let mut ops = Ops::new();
    let mut planner = Decide {
        results,
        decisions,
        after_renames: &after_renames,
        conflicts: &mut conflicts,
        large: BTreeSet::new(),
        filled: BTreeSet::new(),
        rows: BTreeMap::new(),
        blocked: Ops::new(),
    };
    for change in changes {
        planner.change::<D>(change, &mut ops);
    }
    planner.repeated_fill(&ops);
    let blocked = planner.blocked;

    add_key_dependents::<D>(&after_renames, names, &mut ops);
    // An op held back by a conflict (a key with duplicates) can leave a rebuild without
    // what it needs: only a bug when nothing else stops the plan
    if let Err(detail) = substitute_rebuilds::<D>(names, &mut ops)
        && conflicts.is_empty()
    {
        conflicts.push(Conflict::Internal(detail));
    }
    generated_conflicts::<D>(&after_renames, &ops, &blocked, &mut conflicts);

    if !conflicts.is_empty() {
        return Err(conflicts);
    }
    ops.sort_by(|(a, _), (b, _)| order(a).cmp(&order(b)));
    // Every op has to be valid when it runs
    let plan: Vec<Op> = ops.iter().map(|(op, _)| op.clone()).collect();
    if let Err(detail) = apply_in_memory(&actual, &plan) {
        return Err(vec![Conflict::Internal(detail)]);
    }
    let ops = ops
        .into_iter()
        .map(|(op, risk)| PlannedOp {
            phase: phase(&op),
            op,
            risk,
        })
        .collect();
    Ok(Plan {
        ops,
        infos,
        warnings,
    })
}

/// Step 1: each change and its precheck results become ops or conflicts.
struct Decide<'a> {
    results: &'a PrecheckResults,
    decisions: &'a Decisions,
    after_renames: &'a SchemaModel,
    conflicts: &'a mut Vec<Conflict>,
    /// Tables already reported as `LargeRebuild`.
    large: BTreeSet<QualifiedName>,
    /// Columns that get a fill value: new ones on a table with rows, and existing ones
    /// made NOT NULL over their NULLs.
    filled: BTreeSet<(QualifiedName, Ident)>,
    /// `RowCount` of each table a new key is added to, by its name after the renames.
    rows: BTreeMap<QualifiedName, u64>,
    /// Ops held back until the user decides. Their generated-column conflicts are still
    /// reported, so one round shows every hard conflict.
    blocked: Ops,
}

impl Decide<'_> {
    fn change<D: DdlDialect>(&mut self, change: Change, ops: &mut Ops) {
        let Change {
            op,
            risk,
            prechecks,
        } = change;
        if risk == Risk::Destructive && !self.decisions.allows(&op) {
            self.conflicts
                .push(Conflict::Destructive { op: op.clone() });
            self.blocked.push((op, risk));
            return;
        }
        let mut keep = true;
        let mut held = false;
        let mut replaced = Ops::new();
        // Prechecks carry the names before the renames: every subject comes from the op
        for p in &prechecks {
            let found = self.results.value(p);
            let conflict = match (p, &op) {
                (Precheck::RowEstimate(_), _) => {
                    let Some(table) = op.table().filter(|_| D::needs_rebuild(&op)) else {
                        continue;
                    };
                    let confirmed =
                        (self.decisions.confirmed).contains(&ConfirmKey::rebuild(table));
                    if found <= self.decisions.rebuild_confirm_rows || confirmed {
                        continue;
                    }
                    keep = false;
                    held = true;
                    if !self.large.insert(table.clone()) {
                        continue;
                    }
                    Conflict::LargeRebuild {
                        table: table.clone(),
                        estimated_rows: found,
                    }
                }
                (Precheck::RowCount(_), Op::AddPrimaryKey { table, .. }) => {
                    self.rows.insert(table.clone(), found);
                    continue;
                }
                _ if found == 0 => continue,
                (Precheck::TableHasRows(_), Op::AddColumn { table, column }) => {
                    keep = false;
                    let missing = || Conflict::NotNullOnTableWithRows {
                        table: table.clone(),
                        column: column.name.clone(),
                    };
                    let value = match self.fill_value::<D>(table, &column.name, &column.ty, missing)
                    {
                        Ok(value) => value,
                        Err(conflict) => {
                            self.conflicts.push(conflict);
                            continue;
                        }
                    };
                    let nullable = ColumnDef {
                        nullable: true,
                        ..column.clone()
                    };
                    self.filled.insert((table.clone(), column.name.clone()));
                    replaced.push((
                        Op::AddColumn {
                            table: table.clone(),
                            column: nullable,
                        },
                        Risk::Safe,
                    ));
                    replaced.push((fill(table, &column.name, value), Risk::Checked));
                    replaced.push((
                        Op::SetNotNull {
                            table: table.clone(),
                            column: column.name.clone(),
                        },
                        Risk::Checked,
                    ));
                    continue;
                }
                (
                    Precheck::TableHasRows(_)
                    | Precheck::NonNull { .. }
                    | Precheck::Orphans { .. }
                    | Precheck::FilledOrphans { .. },
                    Op::AddForeignKey { table, key },
                ) => {
                    keep = false;
                    Conflict::OrphanRows {
                        table: table.clone(),
                        columns: key.columns.clone(),
                        target: key.ref_table.clone(),
                    }
                }
                (Precheck::HasNulls { .. }, Op::SetNotNull { table, column }) => {
                    let ty = (self.after_renames.table(table))
                        .and_then(|t| t.columns.iter().find(|c| &c.name == column))
                        .map(|c| c.ty.clone());
                    let missing = || Conflict::NullsBlockNotNull {
                        table: table.clone(),
                        column: column.clone(),
                    };
                    let value = match ty {
                        Some(ty) => self.fill_value::<D>(table, column, &ty, missing),
                        None => Err(unexpected(p, &op)),
                    };
                    match value {
                        Ok(value) => {
                            self.filled.insert((table.clone(), column.clone()));
                            replaced.push((fill(table, column, value), Risk::Checked));
                            continue;
                        }
                        Err(conflict) => {
                            keep = false;
                            conflict
                        }
                    }
                }
                (Precheck::OutOfRange { .. }, Op::AlterColumnType { table, column }) => {
                    keep = false;
                    match column.ty {
                        Logical::Known(to) => Conflict::ValuesOutOfRange {
                            table: table.clone(),
                            column: column.name.clone(),
                            to,
                        },
                        Logical::Other(_) => unexpected(p, &op),
                    }
                }
                (Precheck::LongerThan { max_len, .. }, Op::AlterColumnType { table, column }) => {
                    keep = false;
                    Conflict::ValuesTooLong {
                        table: table.clone(),
                        column: column.name.clone(),
                        max_len: *max_len,
                    }
                }
                (Precheck::Duplicates { .. }, Op::AddPrimaryKey { table, key }) => {
                    keep = false;
                    Conflict::DuplicateKeyValues {
                        table: table.clone(),
                        columns: key.columns.clone(),
                    }
                }
                _ => {
                    keep = false;
                    unexpected(p, &op)
                }
            };
            self.conflicts.push(conflict);
        }
        ops.append(&mut replaced);
        if keep {
            ops.push((op, risk));
        } else if held {
            self.blocked.push((op, risk));
        }
    }

    /// The user's value for `table.column`, if there is one and it fits the column
    /// (PostgreSQL decodes strictly). A column no `Param` binds to has to be filled by hand.
    // Short-lived, like `Conflict` itself: no need to box it
    #[allow(clippy::result_large_err)]
    fn fill_value<D: DdlDialect>(
        &self,
        table: &QualifiedName,
        column: &Ident,
        ty: &Logical,
        missing: impl FnOnce() -> Conflict,
    ) -> Result<Param, Conflict> {
        if let Logical::Other(native) = ty
            && !D::TEXT_PARAM_TYPES.contains(&native.name.as_str())
        {
            return Err(Conflict::FillByHand {
                table: table.clone(),
                column: column.clone(),
                column_type: ty.clone(),
            });
        }
        let key = (table.clone(), column.clone());
        let value = self.decisions.fill_values.get(&key).ok_or_else(missing)?;
        if fits(value, ty) {
            return Ok(value.clone());
        }
        Err(Conflict::FillValueType {
            table: table.clone(),
            column: column.clone(),
            expected: ty.clone(),
        })
    }

    /// A fill writes one value on every row it touches, so a new key with any filled
    /// column can repeat as soon as there are two rows (the `Duplicates` precheck ran on
    /// the rows before the fill). One row is fine; a key without a `RowCount` counts as
    /// many rows.
    fn repeated_fill(&mut self, ops: &Ops) {
        let found = ops.iter().filter_map(|(op, _)| match op {
            Op::AddPrimaryKey { table, key }
                if self.rows.get(table).is_none_or(|&n| n > 1)
                    && (key.columns.iter())
                        .any(|c| self.filled.contains(&(table.clone(), c.clone()))) =>
            {
                Some(Conflict::DuplicateKeyValues {
                    table: table.clone(),
                    columns: key.columns.clone(),
                })
            }
            _ => None,
        });
        push_new(self.conflicts, found.collect::<Vec<_>>());
    }
}

fn fill(table: &QualifiedName, column: &Ident, value: Param) -> Op {
    Op::FillColumn {
        table: table.clone(),
        column: column.clone(),
        value,
    }
}

fn unexpected(p: &Precheck, op: &Op) -> Conflict {
    Conflict::Internal(format!(
        "precheck {p:?} doesn't belong to \"{}\"",
        op.describe()
    ))
}

/// The value binds to the column as it is when `FillColumn` runs (phase 4, before any
/// type change). An `Other` column here is one of the dialect's `TEXT_PARAM_TYPES`.
pub(super) fn fits(value: &Param, ty: &Logical) -> bool {
    let within =
        |s: &str, max_len: Option<u32>| max_len.is_none_or(|n| s.chars().count() <= n as usize);
    let ty = match ty {
        Logical::Known(ty) => ty,
        Logical::Other(native) => {
            let max_len = match native.length {
                Some(Length::Chars(n)) => Some(n),
                _ => None,
            };
            return matches!(value, Param::Text(s) if within(s, max_len));
        }
    };
    match (value, ty) {
        (Param::Text(s), SqlType::Text { max_len }) => within(s, *max_len),
        (Param::SmallInt(_), SqlType::SmallInt)
        | (Param::Int(_), SqlType::Int)
        | (Param::BigInt(_), SqlType::BigInt)
        | (Param::Bool(_), SqlType::Bool)
        | (Param::Date(_), SqlType::Date)
        | (Param::Time(_), SqlType::Time { .. })
        | (Param::Timestamp(_), SqlType::Timestamp { .. }) => true,
        _ => false,
    }
}

/// What the database calls things before the plan, for phase-1 drops.
#[derive(Clone, Copy)]
struct Names<'a> {
    actual: &'a SchemaModel,
    old_names: &'a BTreeMap<QualifiedName, QualifiedName>,
}

impl Names<'_> {
    fn old_table(&self, t: &QualifiedName) -> QualifiedName {
        name_in_db(self.old_names, t)
    }

    /// The name a key of `t` (`name` after the renames, as the dialect renames a Canyon
    /// name with its table) has in the database now.
    fn actual_fk_name(&self, t: &QualifiedName, name: &Ident) -> Ident {
        let old_t = self.old_table(t);
        let found = self.actual.table(&old_t).and_then(|have| {
            have.foreign_keys.iter().find_map(|k| {
                let n = k.name.as_ref()?;
                let renamed = old_t != *t
                    && *n == fk_name(&old_t, &k.columns)
                    && *name == fk_name(t, &k.columns);
                (n == name || renamed).then(|| n.clone())
            })
        });
        found.unwrap_or_else(|| name.clone())
    }

    /// `None` for a name Canyon gave (the dialect names it again), the name otherwise.
    fn kept_fk_name(
        &self,
        t: &QualifiedName,
        actual_name: &Ident,
        fk: &ForeignKey,
    ) -> Option<Ident> {
        let old_t = self.old_table(t);
        if *actual_name == fk_name(&old_t, &fk.columns) || *actual_name == fk_name(t, &fk.columns) {
            return None;
        }
        let canyon = (self.actual.table(&old_t))
            .and_then(|have| {
                (have.foreign_keys.iter()).find(|k| k.name.as_ref() == Some(actual_name))
            })
            .is_some_and(|k| *actual_name == fk_name(&old_t, &k.columns));
        (!canyon).then(|| actual_name.clone())
    }

    /// Drops `fk` of `t` in phase 1 and adds it back in phase 11, unless a drop of it
    /// is planned already. `name` is the key's name in the database now.
    fn cycle_fk(&self, t: &QualifiedName, name: Ident, fk: &ForeignKey, ops: &mut Ops) {
        let old_t = self.old_table(t);
        let dropped = has(
            ops,
            |op| matches!(op, Op::DropForeignKey { table, name: n } if *table == old_t && *n == name),
        );
        if dropped {
            return;
        }
        let key = ForeignKey {
            name: self.kept_fk_name(t, &name, fk),
            ..fk.clone()
        };
        ops.push((Op::DropForeignKey { table: old_t, name }, Risk::Safe));
        let added = has(ops, |op| {
            matches!(op, Op::AddForeignKey { table, key: k } if table == t
                && k.columns == key.columns && k.ref_table == key.ref_table && k.ref_columns == key.ref_columns)
        });
        if !added {
            ops.push((
                Op::AddForeignKey {
                    table: t.clone(),
                    key,
                },
                Risk::Safe,
            ));
        }
    }
}

fn has(ops: &Ops, f: impl Fn(&Op) -> bool) -> bool {
    ops.iter().any(|(op, _)| f(op))
}

/// Step 2. FK sides must keep equal types while they change (MySQL), MySQL won't add or
/// drop an identity on a column an FK uses, SQL Server refuses `ALTER COLUMN` on a key
/// column, and no database drops a key an FK uses: drop those keys first and add them
/// back after.
fn add_key_dependents<D: DdlDialect>(model: &SchemaModel, names: Names<'_>, ops: &mut Ops) {
    // The bool is a type change (not an identity one)
    let changed: Vec<(QualifiedName, Ident, bool)> = (ops.iter())
        .filter_map(|(op, _)| match op {
            Op::AlterColumnType { table, column } => {
                Some((table.clone(), column.name.clone(), true))
            }
            Op::AddIdentity { table, column } | Op::DropIdentity { table, column }
                if D::FK_COLUMNS_BLOCK_IDENTITY_CHANGE =>
            {
                Some((table.clone(), column.clone(), false))
            }
            _ => None,
        })
        .collect();
    for (table, column, type_change) in &changed {
        if *type_change
            && D::KEY_COLUMNS_BLOCK_TYPE_CHANGE
            && let Some(pk) = model.table(table).and_then(|t| t.primary_key.as_ref())
            && pk.columns.contains(column)
            && !has(
                ops,
                |op| matches!(op, Op::DropPrimaryKey { table: t, .. } if t == table),
            )
        {
            let old_t = names.old_table(table);
            let canyon = |n: &Ident| *n == pk_name(&old_t) || *n == pk_name(table);
            // Phase 6 runs after the renames: a Canyon name followed the table
            let name = match &pk.name {
                Some(n) if !canyon(n) => n.clone(),
                _ => pk_name(table),
            };
            ops.push((
                Op::DropPrimaryKey {
                    table: table.clone(),
                    name,
                },
                Risk::Safe,
            ));
            // The current key, identity included: the column keeps it in the database
            let key = PrimaryKey {
                name: pk.name.clone().filter(|n| !canyon(n)),
                ..pk.clone()
            };
            ops.push((
                Op::AddPrimaryKey {
                    table: table.clone(),
                    key,
                },
                Risk::Safe,
            ));
        }
        for t in model.tables.values() {
            for fk in &t.foreign_keys {
                let on_it = t.name == *table && fk.columns.contains(column);
                let to_it = fk.ref_table == *table && fk.ref_columns.contains(column);
                if on_it || to_it {
                    let name = fk_ident(names, &t.name, fk);
                    names.cycle_fk(&t.name, name, fk, ops);
                }
            }
        }
    }

    // Keys pointing at a primary key that is dropped go in phase 1, before the drop
    let dropped: Vec<QualifiedName> = (ops.iter())
        .filter_map(|(op, _)| match op {
            Op::DropPrimaryKey { table, .. } => Some(table.clone()),
            _ => None,
        })
        .collect();
    for table in &dropped {
        let Some(pk) = model.table(table).and_then(|t| t.primary_key.as_ref()) else {
            continue;
        };
        for t in model.tables.values() {
            for fk in &t.foreign_keys {
                if fk.ref_table == *table && fk.ref_columns.iter().all(|c| pk.columns.contains(c)) {
                    let name = fk_ident(names, &t.name, fk);
                    names.cycle_fk(&t.name, name, fk, ops);
                }
            }
        }
    }
}

/// A key's name in the database now. `after_renames` keeps the real names.
fn fk_ident(names: Names<'_>, t: &QualifiedName, fk: &ForeignKey) -> Ident {
    fk.name
        .clone()
        .unwrap_or_else(|| fk_name(&names.old_table(t), &fk.columns))
}

/// Step 3: ops the dialect can only do by rebuilding (SQL Server identity) become one
/// `RebuildTable` per table, from the table as it is after phase 8. Every FK on it or
/// pointing at it is dropped before and added back after.
fn substitute_rebuilds<D: DdlDialect>(names: Names<'_>, ops: &mut Ops) -> Result<(), String> {
    let (removed, kept): (Ops, Ops) = ops.drain(..).partition(|(op, _)| D::needs_rebuild(op));
    *ops = kept;
    let mut tables: Vec<QualifiedName> = removed
        .iter()
        .filter_map(|(op, _)| op.table().cloned())
        .collect();
    tables.sort();
    tables.dedup();
    if tables.is_empty() {
        return Ok(());
    }
    let mut early: Vec<&Op> = (ops.iter())
        .map(|(op, _)| op)
        .filter(|op| phase(op) <= 8)
        .collect();
    early.sort_by(|a, b| order(a).cmp(&order(b)));
    let early: Vec<Op> = early.into_iter().cloned().collect();
    // From the database as it is: `after_renames` would apply the renames twice
    let state = apply_in_memory(names.actual, &early)?;
    // The first table that can't be rebuilt; the others still are, for the conflicts
    let mut failed = None;
    for name in tables {
        let Some(mut from) = state.table(&name).cloned() else {
            failed.get_or_insert_with(|| format!("table `{name}` is gone before its rebuild"));
            continue;
        };
        for t in state.tables.values() {
            for fk in &t.foreign_keys {
                if fk.ref_table == name || t.name == name {
                    let state_name = fk
                        .name
                        .clone()
                        .unwrap_or_else(|| fk_name(&t.name, &fk.columns));
                    let actual_name = names.actual_fk_name(&t.name, &state_name);
                    names.cycle_fk(&t.name, actual_name, fk, ops);
                }
            }
        }
        from.foreign_keys.clear();
        // The identity the removed ops ask for: on if any adds it (a key moving to another
        // column drops it from one and adds it to the other)
        let want = removed
            .iter()
            .any(|(op, _)| matches!(op, Op::AddIdentity { table, .. } if *table == name));
        let mut to = from.clone();
        match to.primary_key.as_mut() {
            Some(pk) => pk.generated = want.then_some(Generated::Identity),
            None if want => {
                failed.get_or_insert_with(|| {
                    format!("`{name}` gets an identity without a primary key")
                });
                continue;
            }
            None => {}
        }
        let column_map = (from.columns.iter())
            .map(|c| (c.name.clone(), c.name.clone()))
            .collect();
        ops.push((
            Op::RebuildTable {
                from,
                to,
                column_map,
            },
            Risk::Checked,
        ));
    }
    failed.map_or(Ok(()), Err)
}

/// Generated columns, with the table as the unit (the model doesn't know which columns
/// one uses): never let through a change the database would refuse.
fn generated_conflicts<D: DdlDialect>(
    state: &SchemaModel,
    ops: &Ops,
    blocked: &Ops,
    out: &mut Vec<Conflict>,
) {
    let mut found = Vec::new();
    for (op, _) in blocked {
        // A rebuild the user hasn't confirmed yet: it would hit the same wall
        if D::needs_rebuild(op)
            && let Some(table) = op.table()
            && let Some(c) =
                (state.table(table)).and_then(|t| t.columns.iter().find(|c| c.extra.generated))
        {
            found.push(Conflict::GeneratedColumn {
                table: table.clone(),
                column: c.name.clone(),
                op: rebuild_description(table),
            });
        }
    }
    for (op, _) in ops.iter().chain(blocked) {
        let (table, column) = match op {
            Op::RebuildTable { from, .. } => {
                if let Some(c) = from.columns.iter().find(|c| c.extra.generated) {
                    found.push(Conflict::GeneratedColumn {
                        table: from.name.clone(),
                        column: c.name.clone(),
                        op: op.describe(),
                    });
                }
                continue;
            }
            Op::AlterColumnType { table, column } => (table, &column.name),
            Op::FillColumn { table, column, .. }
            | Op::SetNotNull { table, column }
            | Op::DropNotNull { table, column }
            | Op::AddIdentity { table, column }
            | Op::DropIdentity { table, column }
            | Op::DropColumn { table, column } => (table, column),
            _ => continue,
        };
        let Some(t) = state.table(table) else {
            continue;
        };
        let Some(c) = t.columns.iter().find(|c| &c.name == column) else {
            continue;
        };
        let generated: Vec<Ident> = (t.columns.iter())
            .filter(|c| c.extra.generated)
            .map(|c| c.name.clone())
            .collect();
        let (table, column) = (table.clone(), column.clone());
        match op {
            Op::DropColumn { .. } if !c.extra.generated && !generated.is_empty() => {
                found.push(Conflict::DependedOnByGenerated {
                    table,
                    column,
                    generated,
                });
            }
            Op::DropColumn { .. } => {}
            // An UPDATE can't write a generated column on any of the three
            Op::FillColumn { .. } if c.extra.generated => found.push(Conflict::GeneratedColumn {
                table,
                column,
                op: op.describe(),
            }),
            _ if c.extra.generated && D::REWRITES_COLUMN => found.push(Conflict::GeneratedColumn {
                table,
                column,
                op: op.describe(),
            }),
            Op::AlterColumnType { .. }
                if !c.extra.generated
                    && D::GENERATED_BLOCKS_BASE_TYPE_CHANGE
                    && !generated.is_empty() =>
            {
                found.push(Conflict::GeneratedColumnDependency {
                    table,
                    column,
                    generated,
                });
            }
            _ => {}
        }
    }
    push_new(out, found);
}

#[cfg(all(test, feature = "postgres", feature = "mssql", feature = "mysql"))]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::planning::diff::{KeyValue, diff};
    use crate::planning::rename::{Previous, Renames};
    use crate::planning::test_support::{id, introspected, q};
    use canyon_core::query::querybuilder::syntax::dialect::{MsSql, MySql, PgDialect};
    use canyon_core::types::SqlType::{self, *};

    fn models(tables: Vec<TableDef>) -> SchemaModel {
        SchemaModel::from_tables(tables).unwrap()
    }
    fn league_with(id_ty: SqlType, generated: Option<Generated>) -> TableDef {
        TableDef::builder("League", "league")
            .column("id", id_ty, false)
            .primary_key(generated)
            .column("name", Text { max_len: Some(100) }, false)
            .build()
            .unwrap()
    }
    fn league() -> TableDef {
        league_with(Int, Some(Generated::Identity))
    }
    fn player_with(table: &str, league_ty: SqlType) -> TableDef {
        TableDef::builder("Player", table)
            .column("id", Int, false)
            .primary_key(Some(Generated::Identity))
            .column("name", Text { max_len: None }, false)
            .column("league", league_ty, true)
            .foreign_key("league", "id")
            .build()
            .unwrap()
    }
    fn player() -> TableDef {
        player_with("player", Int)
    }
    fn plain(name: &str, cols: &[(&str, SqlType, bool)]) -> TableDef {
        let mut b = TableDef::builder("", name);
        for (c, ty, nullable) in cols {
            b = b.column(c, *ty, *nullable);
        }
        b.build().unwrap()
    }
    fn generated(mut t: TableDef, column: &str) -> TableDef {
        t.columns
            .iter_mut()
            .find(|c| c.name.as_str() == column)
            .unwrap()
            .extra
            .generated = true;
        t
    }
    fn results(found: &[(Precheck, u64)]) -> PrecheckResults {
        PrecheckResults(found.iter().cloned().collect())
    }
    fn diff_of(desired: &SchemaModel, actual: &SchemaModel, renames: &Renames) -> DiffOutput {
        diff(
            desired,
            actual,
            &Previous::Synthesized(actual.clone()),
            renames,
            &Decisions::default(),
        )
    }
    fn run<D: DdlDialect>(
        desired: &SchemaModel,
        actual: &SchemaModel,
        found: &PrecheckResults,
        decisions: &Decisions,
    ) -> Result<Plan, Vec<Conflict>> {
        let out = diff(
            desired,
            actual,
            &Previous::Synthesized(actual.clone()),
            &Renames::default(),
            decisions,
        );
        finalize::<D>(out, found, decisions, Vec::new())
    }
    fn ok<D: DdlDialect>(desired: &SchemaModel, actual: &SchemaModel) -> Plan {
        run::<D>(
            desired,
            actual,
            &PrecheckResults::default(),
            &Decisions::default(),
        )
        .unwrap_or_else(|c| panic!("{c:?}"))
    }
    /// `<phase> <describe>` per op, with the order checked.
    fn lines(plan: &Plan) -> Vec<String> {
        let phases: Vec<u8> = plan.ops.iter().map(|p| p.phase).collect();
        assert!(phases.is_sorted(), "{phases:?}");
        plan.ops
            .iter()
            .map(|p| format!("{} {}", p.phase, p.op.describe()))
            .collect()
    }
    fn with_fill(table: &str, column: &str, value: Param) -> Decisions {
        let mut d = Decisions::default();
        d.fill_values.insert((q(table), id(column)), value);
        d
    }
    fn rebuilds(plan: &Plan) -> Vec<(&TableDef, &TableDef)> {
        plan.ops
            .iter()
            .filter_map(|p| match &p.op {
                Op::RebuildTable { from, to, .. } => Some((from, to)),
                _ => None,
            })
            .collect()
    }

    // --- phases and order ---

    #[test]
    fn every_op_kind_lands_in_its_phase() {
        let t = q("t");
        let c = id("c");
        let col = plain("t", &[("c", Int, true)]).columns.remove(0);
        let table = plain("t", &[("c", Int, true)]);
        let pk = PrimaryKey {
            name: None,
            columns: vec![c.clone()],
            generated: None,
        };
        let fk = ForeignKey {
            name: None,
            columns: vec![c.clone()],
            ref_table: t.clone(),
            ref_columns: vec![c.clone()],
        };
        let cases = [
            (Op::CreateSchema { schema: id("s") }, 0),
            (
                Op::DropForeignKey {
                    table: t.clone(),
                    name: c.clone(),
                },
                1,
            ),
            (
                Op::MoveTable {
                    from: t.clone(),
                    to_schema: None,
                },
                2,
            ),
            (
                Op::RenameTable {
                    from: t.clone(),
                    to: c.clone(),
                },
                2,
            ),
            (
                Op::RenameColumn {
                    table: t.clone(),
                    from: c.clone(),
                    to: c.clone(),
                },
                2,
            ),
            (Op::CreateTable(table.clone()), 3),
            (
                Op::AddColumn {
                    table: t.clone(),
                    column: col.clone(),
                },
                4,
            ),
            (
                Op::FillColumn {
                    table: t.clone(),
                    column: c.clone(),
                    value: Param::Int(1),
                },
                4,
            ),
            (
                Op::DropIdentity {
                    table: t.clone(),
                    column: c.clone(),
                },
                5,
            ),
            (
                Op::DropPrimaryKey {
                    table: t.clone(),
                    name: c.clone(),
                },
                6,
            ),
            (
                Op::AlterColumnType {
                    table: t.clone(),
                    column: col,
                },
                7,
            ),
            (
                Op::SetNotNull {
                    table: t.clone(),
                    column: c.clone(),
                },
                7,
            ),
            (
                Op::DropNotNull {
                    table: t.clone(),
                    column: c.clone(),
                },
                7,
            ),
            (
                Op::AddPrimaryKey {
                    table: t.clone(),
                    key: pk,
                },
                8,
            ),
            (
                Op::AddIdentity {
                    table: t.clone(),
                    column: c.clone(),
                },
                9,
            ),
            (
                Op::RebuildTable {
                    from: table.clone(),
                    to: table,
                    column_map: Vec::new(),
                },
                9,
            ),
            (
                Op::DropColumn {
                    table: t.clone(),
                    column: c.clone(),
                },
                10,
            ),
            (Op::DropTable(t.clone()), 10),
            (Op::AddForeignKey { table: t, key: fk }, 11),
        ];
        for (op, want) in cases {
            assert_eq!(phase(&op), want, "{}", op.describe());
        }
    }

    #[test]
    fn same_phase_goes_by_table_then_diff_order() {
        let actual = models(vec![
            plain("b", &[("x", Int, true)]),
            plain("a", &[("x", Int, true)]),
        ]);
        let desired = models(vec![
            plain("b", &[("x", Int, true), ("z", Int, true), ("y", Int, true)]),
            plain("a", &[("x", Int, true), ("w", Int, true)]),
        ]);
        let plan = ok::<PgDialect>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "4 add column a.w (integer)",
                "4 add column b.z (integer)",
                "4 add column b.y (integer)",
            ]
        );
    }

    #[test]
    fn chained_table_renames_keep_the_diff_order() {
        let actual = models(vec![
            plain("a", &[("from_a", Int, true)]),
            plain("b", &[("from_b", Int, true)]),
        ]);
        let desired = models(vec![
            plain("b", &[("from_a", Int, true)]),
            plain("c", &[("from_b", Int, true)]),
        ]);
        let renames = Renames {
            tables: vec![(q("a"), q("b")), (q("b"), q("c"))],
            ..Renames::default()
        };
        let out = diff_of(&desired, &actual, &renames);
        let plan = finalize::<PgDialect>(
            out,
            &PrecheckResults::default(),
            &Decisions::default(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            lines(&plan),
            ["2 rename table b to c", "2 rename table a to b"]
        );
    }

    #[test]
    fn moves_then_table_renames_then_column_renames() {
        let nick = |mut t: TableDef| {
            t.columns[1].name = id("nick");
            t
        };
        let previous = models(vec![league(), nick(player())]);
        let actual = models(vec![introspected(league()), introspected(nick(player()))]);
        let mut league_s = league();
        league_s.name = q("s.league");
        let mut gamer = player_with("gamer", Int);
        gamer.name = q("s.gamer");
        gamer.foreign_keys[0].ref_table = q("s.league");
        let desired = models(vec![league_s, gamer]);
        let decisions = Decisions::default();
        let prepared = crate::planning::prepare::<PgDialect>(
            &desired,
            &actual,
            &Previous::Snapshot(previous),
            &decisions,
            "public",
            &[],
        );
        assert!(prepared.conflicts.is_empty(), "{:?}", prepared.conflicts);
        let plan = finalize::<PgDialect>(
            prepared.diff,
            &PrecheckResults::default(),
            &prepared.decisions,
            prepared.conflicts,
        )
        .unwrap_or_else(|c| panic!("{c:?}"));
        assert_eq!(
            lines(&plan),
            [
                "0 create schema s",
                "2 move table league to schema s",
                "2 move table player to schema s",
                "2 rename table s.player to gamer",
                "2 rename column s.gamer.nick to name",
            ]
        );
    }

    fn planned(previous: SchemaModel, actual: SchemaModel, desired: SchemaModel) -> Vec<String> {
        let decisions = Decisions::default();
        let prepared = crate::planning::prepare::<PgDialect>(
            &desired,
            &actual,
            &Previous::Snapshot(previous),
            &decisions,
            "public",
            &[id("s")],
        );
        assert!(prepared.conflicts.is_empty(), "{:?}", prepared.conflicts);
        let plan = finalize::<PgDialect>(
            prepared.diff,
            &PrecheckResults::default(),
            &prepared.decisions,
            prepared.conflicts,
        )
        .unwrap_or_else(|c| panic!("{c:?}"));
        lines(&plan)
    }

    #[test]
    fn a_moved_table_keeps_its_self_reference() {
        let node = |schema: Option<&str>| {
            let mut b = TableDef::builder("Node", "node");
            if let Some(s) = schema {
                b = b.schema(s);
            }
            let target = if schema.is_some() { "s.node" } else { "node" };
            b.column("id", Int, false)
                .primary_key(None)
                .column("parent", Int, true)
                .foreign_key(target, "id")
                .build()
                .unwrap()
        };
        assert_eq!(
            planned(
                models(vec![node(None)]),
                models(vec![introspected(node(None))]),
                models(vec![node(Some("s"))]),
            ),
            ["2 move table node to schema s"]
        );
    }

    #[test]
    fn a_table_moves_back_to_the_default_schema() {
        let mut league_s = league();
        league_s.name = q("s.league");
        let mut player_s = player();
        player_s.foreign_keys[0].ref_table = q("s.league");
        assert_eq!(
            planned(
                models(vec![league_s.clone(), player_s.clone()]),
                models(vec![introspected(league_s), introspected(player_s)]),
                models(vec![league(), player()]),
            ),
            ["2 move table s.league to the default schema"]
        );
    }

    // --- risks, prechecks and decisions ---

    fn with_region(nullable: bool) -> SchemaModel {
        let mut l = league();
        l.columns.push(
            plain("x", &[("region", Text { max_len: Some(20) }, nullable)])
                .columns
                .remove(0),
        );
        models(vec![l])
    }

    #[test]
    fn not_null_column_on_a_table_with_rows_needs_a_value() {
        let actual = models(vec![introspected(league())]);
        let rows = results(&[(Precheck::TableHasRows(q("league")), 1)]);
        let err = run::<PgDialect>(&with_region(false), &actual, &rows, &Decisions::default())
            .unwrap_err();
        assert_eq!(
            err,
            [Conflict::NotNullOnTableWithRows {
                table: q("league"),
                column: id("region")
            }]
        );

        let d = with_fill("league", "region", Param::Text("eu".into()));
        let plan = run::<PgDialect>(&with_region(false), &actual, &rows, &d).unwrap();
        assert_eq!(
            lines(&plan),
            [
                "4 add column league.region (text(20))",
                "4 fill empty values of league.region",
                "7 make league.region NOT NULL",
            ]
        );
        assert!(matches!(&plan.ops[0].op, Op::AddColumn { column, .. } if column.nullable));

        // No rows: the column is added NOT NULL as it is
        let plan = ok::<PgDialect>(&with_region(false), &actual);
        assert!(
            matches!(&plan.ops[..], [p] if matches!(&p.op, Op::AddColumn { column, .. } if !column.nullable))
        );
    }

    #[test]
    fn nulls_block_not_null_unless_filled() {
        let actual = models(vec![introspected(
            with_region(true).tables.into_values().next().unwrap(),
        )]);
        let nulls = results(&[(
            Precheck::HasNulls {
                table: q("league"),
                column: id("region"),
            },
            1,
        )]);
        let err = run::<PgDialect>(&with_region(false), &actual, &nulls, &Decisions::default())
            .unwrap_err();
        assert_eq!(
            err,
            [Conflict::NullsBlockNotNull {
                table: q("league"),
                column: id("region")
            }]
        );
        let d = with_fill("league", "region", Param::Text("eu".into()));
        let plan = run::<PgDialect>(&with_region(false), &actual, &nulls, &d).unwrap();
        assert_eq!(
            lines(&plan),
            [
                "4 fill empty values of league.region",
                "7 make league.region NOT NULL"
            ]
        );
    }

    #[test]
    fn destructive_ops_need_a_decision() {
        let mut have = introspected(league());
        have.columns
            .push(plain("x", &[("old", Int, true)]).columns.remove(0));
        let actual = models(vec![have]);
        let desired = models(vec![league()]);
        let drop = Op::DropColumn {
            table: q("league"),
            column: id("old"),
        };
        let err = run::<PgDialect>(
            &desired,
            &actual,
            &PrecheckResults::default(),
            &Decisions::default(),
        )
        .unwrap_err();
        assert_eq!(err, [Conflict::Destructive { op: drop }]);
        let plan = run::<PgDialect>(
            &desired,
            &actual,
            &PrecheckResults::default(),
            &Decisions::from_config(true, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS),
        )
        .unwrap();
        assert_eq!(lines(&plan), ["10 drop column league.old"]);
    }

    #[test]
    fn failed_prechecks_name_the_columns_after_the_renames() {
        // Narrowing, text shortening and a new key on a renamed table
        let mut have = introspected(plain(
            "league",
            &[
                ("pts", Int, false),
                ("slug", Text { max_len: Some(100) }, false),
            ],
        ));
        have.primary_key = None;
        let actual = models(vec![have]);
        let desired = models(vec![
            TableDef::builder("L", "ligue")
                .column("points", SmallInt, false)
                .column("slug", Text { max_len: Some(10) }, false)
                .primary_key(None)
                .build()
                .unwrap(),
        ]);
        let renames = Renames {
            tables: vec![(q("league"), q("ligue"))],
            columns: vec![(q("ligue"), id("pts"), id("points"))],
            ..Renames::default()
        };
        let d = Decisions::from_config(true, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS);
        // A real snapshot: the shorter text is the code's intent
        let out = diff(
            &desired,
            &actual,
            &Previous::Snapshot(actual.clone()),
            &renames,
            &d,
        );
        let found = results(&[
            (
                Precheck::OutOfRange {
                    table: q("league"),
                    column: id("pts"),
                    min: i16::MIN.into(),
                    max: i16::MAX.into(),
                },
                1,
            ),
            (
                Precheck::LongerThan {
                    table: q("league"),
                    column: id("slug"),
                    max_len: 10,
                },
                1,
            ),
            (
                Precheck::Duplicates {
                    table: q("league"),
                    columns: vec![id("slug")],
                },
                1,
            ),
        ]);
        let err = finalize::<PgDialect>(out, &found, &d, Vec::new()).unwrap_err();
        assert_eq!(
            err,
            [
                Conflict::ValuesOutOfRange {
                    table: q("ligue"),
                    column: id("points"),
                    to: SmallInt
                },
                Conflict::ValuesTooLong {
                    table: q("ligue"),
                    column: id("slug"),
                    max_len: 10
                },
                Conflict::DuplicateKeyValues {
                    table: q("ligue"),
                    columns: vec![id("slug")]
                },
            ]
        );
    }

    #[test]
    fn fill_value_key_comes_from_the_op_after_the_rename() {
        let actual = models(vec![introspected(league())]);
        let mut ligue = with_region(false).tables.into_values().next().unwrap();
        ligue.name = q("ligue");
        let desired = models(vec![ligue]);
        let renames = Renames {
            tables: vec![(q("league"), q("ligue"))],
            ..Renames::default()
        };
        let rows = results(&[(Precheck::TableHasRows(q("league")), 1)]);

        let err = finalize::<PgDialect>(
            diff_of(&desired, &actual, &renames),
            &rows,
            &Decisions::default(),
            Vec::new(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            [Conflict::NotNullOnTableWithRows {
                table: q("ligue"),
                column: id("region")
            }]
        );

        let d = with_fill("ligue", "region", Param::Text("eu".into()));
        let plan =
            finalize::<PgDialect>(diff_of(&desired, &actual, &renames), &rows, &d, Vec::new())
                .unwrap();
        assert_eq!(
            lines(&plan),
            [
                "2 rename table league to ligue",
                "4 add column ligue.region (text(20))",
                "4 fill empty values of ligue.region",
                "7 make ligue.region NOT NULL",
            ]
        );
    }

    fn team() -> TableDef {
        TableDef::builder("Team", "team")
            .column("id", Int, false)
            .primary_key(None)
            .build()
            .unwrap()
    }

    #[test]
    fn orphans_found_on_an_existing_column_block_the_foreign_key() {
        let with_team = |t: TableDef| TableDef {
            columns: [t.columns.clone(), vec![col_team()]].concat(),
            ..t
        };
        fn col_team() -> ColumnDef {
            plain("x", &[("team", Int, true)]).columns.remove(0)
        }
        let have = models(vec![
            introspected(with_team(league())),
            introspected(team()),
        ]);
        let mut want = with_team(league());
        want.foreign_keys = vec![ForeignKey {
            name: None,
            columns: vec![id("team")],
            ref_table: q("team"),
            ref_columns: vec![id("id")],
        }];
        let desired = models(vec![want, team()]);
        let out = diff_of(&desired, &have, &Renames::default());
        let asked: Vec<Precheck> = (out.changes.iter())
            .flat_map(|c| c.prechecks.iter().cloned())
            .filter(|p| matches!(p, Precheck::Orphans { .. }))
            .collect();
        assert_eq!(asked.len(), 1, "{asked:?}");
        let found = results(&[(asked[0].clone(), 3)]);
        let err =
            finalize::<PgDialect>(out, &found, &Decisions::default(), Vec::new()).unwrap_err();
        assert_eq!(
            err,
            [Conflict::OrphanRows {
                table: q("league"),
                columns: vec![id("team")],
                target: q("team"),
            }]
        );
    }

    #[test]
    fn a_fill_value_for_a_new_foreign_key_must_exist_in_the_target() {
        // league has rows; the new NOT NULL `team` gets 7 on every row, and `team` exists
        let mut want = league();
        want.columns
            .push(plain("x", &[("team", Int, false)]).columns.remove(0));
        want.foreign_keys = vec![ForeignKey {
            name: None,
            columns: vec![id("team")],
            ref_table: q("team"),
            ref_columns: vec![id("id")],
        }];
        let desired = models(vec![want, team()]);
        let actual = models(vec![introspected(league()), introspected(team())]);
        let mut decisions = Decisions::default();
        decisions
            .fill_values
            .insert((q("league"), id("team")), Param::Int(7));
        let out = diff(
            &desired,
            &actual,
            &Previous::Synthesized(actual.clone()),
            &Renames::default(),
            &decisions,
        );
        let missing = Precheck::FilledOrphans {
            table: q("league"),
            target: q("team"),
            columns: vec![(KeyValue::Fill(Param::Int(7)), id("id"))],
        };
        let add = (out.changes.iter())
            .find(|c| matches!(c.op, Op::AddForeignKey { .. }))
            .unwrap();
        assert_eq!(add.prechecks, std::slice::from_ref(&missing));

        let rows = (Precheck::TableHasRows(q("league")), 1);
        let err = run::<PgDialect>(
            &desired,
            &actual,
            &results(&[rows.clone(), (missing.clone(), 1)]),
            &decisions,
        )
        .unwrap_err();
        assert_eq!(
            err,
            [Conflict::OrphanRows {
                table: q("league"),
                columns: vec![id("team")],
                target: q("team"),
            }]
        );

        // The target has 7: added nullable, filled, made NOT NULL, then the key
        let plan = run::<PgDialect>(&desired, &actual, &results(&[rows]), &decisions).unwrap();
        let kinds: Vec<&str> = plan.ops.iter().map(|p| p.op.kind()).collect();
        assert_eq!(
            kinds,
            ["AddColumn", "FillColumn", "SetNotNull", "AddForeignKey"]
        );

        // No value yet, or one of another type: nothing to ask, the plan stops anyway
        for decisions in [Decisions::default(), {
            let mut d = Decisions::default();
            d.fill_values
                .insert((q("league"), id("team")), Param::Text("7".into()));
            d
        }] {
            let out = diff(
                &desired,
                &actual,
                &Previous::Synthesized(actual.clone()),
                &Renames::default(),
                &decisions,
            );
            assert!((out.changes.iter()).all(|c| {
                !c.prechecks
                    .iter()
                    .any(|p| matches!(p, Precheck::FilledOrphans { .. }))
            }));
        }
    }

    #[test]
    fn nulls_filled_under_a_new_foreign_key_must_find_the_value() {
        // league.team exists, nullable, with NULLs; it becomes NOT NULL with 7 over the
        // NULLs and gets a key to the existing `team`
        let team_col = |nullable| plain("x", &[("team", Int, nullable)]).columns.remove(0);
        let mut have = introspected(league());
        have.columns.push(team_col(true));
        let mut want = league();
        want.columns.push(team_col(false));
        want.foreign_keys = vec![ForeignKey {
            name: None,
            columns: vec![id("team")],
            ref_table: q("team"),
            ref_columns: vec![id("id")],
        }];
        let desired = models(vec![want, team()]);
        let actual = models(vec![have, introspected(team())]);
        let mut decisions = Decisions::default();
        decisions
            .fill_values
            .insert((q("league"), id("team")), Param::Int(7));
        let out = diff(
            &desired,
            &actual,
            &Previous::Synthesized(actual.clone()),
            &Renames::default(),
            &decisions,
        );
        let missing = Precheck::FilledOrphans {
            table: q("league"),
            target: q("team"),
            columns: vec![(KeyValue::ColumnOrFill(id("team"), Param::Int(7)), id("id"))],
        };
        let add = (out.changes.iter())
            .find(|c| matches!(c.op, Op::AddForeignKey { .. }))
            .unwrap();
        assert_eq!(add.prechecks, std::slice::from_ref(&missing));

        let nulls = (
            Precheck::HasNulls {
                table: q("league"),
                column: id("team"),
            },
            1,
        );
        let err = run::<PgDialect>(
            &desired,
            &actual,
            &results(&[nulls.clone(), (missing, 1)]),
            &decisions,
        )
        .unwrap_err();
        assert_eq!(
            err,
            [Conflict::OrphanRows {
                table: q("league"),
                columns: vec![id("team")],
                target: q("team"),
            }]
        );
        let plan = run::<PgDialect>(&desired, &actual, &results(&[nulls]), &decisions).unwrap();
        let kinds: Vec<&str> = plan.ops.iter().map(|p| p.op.kind()).collect();
        assert_eq!(kinds, ["FillColumn", "SetNotNull", "AddForeignKey"]);

        // Without a value the NULL rows aren't asked about (the plan stops on them)
        let out = diff(
            &desired,
            &actual,
            &Previous::Synthesized(actual.clone()),
            &Renames::default(),
            &Decisions::default(),
        );
        let add = (out.changes.iter())
            .find(|c| matches!(c.op, Op::AddForeignKey { .. }))
            .unwrap();
        assert!(matches!(add.prechecks[..], [Precheck::Orphans { .. }]));
    }

    #[test]
    fn rows_found_for_a_foreign_key_are_orphans() {
        // Existing column, new target table: `NonNull`
        let mut have = introspected(league());
        have.columns
            .push(plain("x", &[("team", Int, true)]).columns.remove(0));
        let mut want = league();
        want.columns
            .push(plain("x", &[("team", Int, true)]).columns.remove(0));
        want = TableDef {
            foreign_keys: vec![ForeignKey {
                name: None,
                columns: vec![id("team")],
                ref_table: q("team"),
                ref_columns: vec![id("id")],
            }],
            ..want
        };
        let desired = models(vec![want.clone(), team()]);
        let found = results(&[(
            Precheck::NonNull {
                table: q("league"),
                columns: vec![id("team")],
            },
            1,
        )]);
        let err = run::<PgDialect>(&desired, &models(vec![have]), &found, &Decisions::default())
            .unwrap_err();
        let orphans = Conflict::OrphanRows {
            table: q("league"),
            columns: vec![id("team")],
            target: q("team"),
        };
        assert_eq!(err, std::slice::from_ref(&orphans));

        // New NOT NULL column, new target table: `TableHasRows` is both a NOT NULL and an orphan question
        want.columns.last_mut().unwrap().nullable = false;
        let desired = models(vec![want, team()]);
        let found = results(&[(Precheck::TableHasRows(q("league")), 1)]);
        let err = run::<PgDialect>(
            &desired,
            &models(vec![introspected(league())]),
            &found,
            &Decisions::default(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            [
                Conflict::NotNullOnTableWithRows {
                    table: q("league"),
                    column: id("team")
                },
                orphans
            ]
        );
    }

    #[test]
    fn checked_change_without_prechecks_has_nothing_to_ask() {
        let actual = models(vec![plain("t", &[("a", Int, true)])]);
        let out = DiffOutput {
            changes: vec![Change {
                op: Op::DropNotNull {
                    table: q("t"),
                    column: id("a"),
                },
                risk: Risk::Checked,
                prechecks: Vec::new(),
            }],
            after_renames: actual.clone(),
            actual,
            ..DiffOutput::default()
        };
        let plan = finalize::<PgDialect>(
            out,
            &PrecheckResults::default(),
            &Decisions::default(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(lines(&plan), ["7 make t.a nullable"]);
    }

    #[test]
    fn a_constant_fill_in_a_new_primary_key_repeats_from_two_rows() {
        let actual = models(vec![plain("t", &[("b", Int, false)])]);
        let desired = models(vec![
            TableDef::builder("T", "t")
                .column("b", Int, false)
                .column("id", Int, false)
                .primary_key(None)
                .build()
                .unwrap(),
        ]);
        let fill = with_fill("t", "id", Param::Int(1));
        let rows = |n| {
            results(&[
                (Precheck::TableHasRows(q("t")), 1),
                (Precheck::RowCount(q("t")), n),
            ])
        };
        let err = run::<PgDialect>(&desired, &actual, &rows(2), &fill).unwrap_err();
        assert_eq!(
            err,
            [Conflict::DuplicateKeyValues {
                table: q("t"),
                columns: vec![id("id")]
            }]
        );
        // One row can't repeat
        let plan = run::<PgDialect>(&desired, &actual, &rows(1), &fill).unwrap();
        assert_eq!(
            lines(&plan),
            [
                "4 add column t.id (integer)",
                "4 fill empty values of t.id",
                "7 make t.id NOT NULL",
                "8 add primary key to t (id)"
            ]
        );
        // An empty table needs no value at all
        let plan = ok::<PgDialect>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            ["4 add column t.id (integer)", "8 add primary key to t (id)"]
        );
    }

    #[test]
    fn fill_value_must_match_the_column_type() {
        let actual = models(vec![introspected(league())]);
        let rows = results(&[(Precheck::TableHasRows(q("league")), 1)]);
        let wrong = with_fill("league", "region", Param::BigInt(1));
        let err = run::<PgDialect>(&with_region(false), &actual, &rows, &wrong).unwrap_err();
        let expected = Logical::Known(Text { max_len: Some(20) });
        assert_eq!(
            err,
            [Conflict::FillValueType {
                table: q("league"),
                column: id("region"),
                expected: expected.clone()
            }]
        );
        let long = with_fill("league", "region", Param::Text("x".repeat(21)));
        let err = run::<PgDialect>(&with_region(false), &actual, &rows, &long).unwrap_err();
        assert_eq!(
            err,
            [Conflict::FillValueType {
                table: q("league"),
                column: id("region"),
                expected
            }]
        );

        // Integers by width, on SetNotNull too
        let mut have = introspected(league());
        have.columns
            .push(plain("x", &[("rank", SmallInt, true)]).columns.remove(0));
        let mut want = league();
        want.columns
            .push(plain("x", &[("rank", SmallInt, false)]).columns.remove(0));
        let nulls = results(&[(
            Precheck::HasNulls {
                table: q("league"),
                column: id("rank"),
            },
            1,
        )]);
        let (desired, actual) = (models(vec![want]), models(vec![have]));
        let err = run::<PgDialect>(
            &desired,
            &actual,
            &nulls,
            &with_fill("league", "rank", Param::Int(0)),
        )
        .unwrap_err();
        assert_eq!(
            err,
            [Conflict::FillValueType {
                table: q("league"),
                column: id("rank"),
                expected: Logical::Known(SmallInt)
            }]
        );
        assert!(
            run::<PgDialect>(
                &desired,
                &actual,
                &nulls,
                &with_fill("league", "rank", Param::SmallInt(0))
            )
            .is_ok()
        );
    }

    /// `league` with `region` as the database has it (nullable, `native`) and as the
    /// model wants it (NOT NULL text).
    fn other_region(native: NativeType) -> (SchemaModel, SchemaModel) {
        let mut have = introspected(league());
        let mut region = plain("x", &[("region", Int, true)]).columns.remove(0);
        region.ty = Logical::Other(native);
        have.columns.push(region);
        let desired = with_region(false);
        (desired, models(vec![have]))
    }
    fn region_nulls() -> PrecheckResults {
        results(&[(
            Precheck::HasNulls {
                table: q("league"),
                column: id("region"),
            },
            1,
        )])
    }
    fn fill_and_allow(value: Param) -> Decisions {
        let mut d = with_fill("league", "region", value);
        d.allow_destructive = true;
        d
    }

    #[test]
    fn a_character_column_canyon_doesnt_map_takes_a_text_fill() {
        let varchar = NativeType::new("varchar", Some(Length::Chars(20)), None, "varchar(20)");
        let (desired, actual) = other_region(varchar.clone());
        let nulls = region_nulls();
        let plan = run::<MsSql>(
            &desired,
            &actual,
            &nulls,
            &fill_and_allow(Param::Text("eu".into())),
        )
        .unwrap();
        assert_eq!(
            lines(&plan),
            [
                "4 fill empty values of league.region",
                "7 change type of league.region to text(20)",
                "7 make league.region NOT NULL",
            ]
        );
        let expected = Logical::Other(varchar);
        for wrong in [Param::Int(1), Param::Text("x".repeat(21))] {
            let err = run::<MsSql>(&desired, &actual, &nulls, &fill_and_allow(wrong)).unwrap_err();
            assert_eq!(
                err,
                [Conflict::FillValueType {
                    table: q("league"),
                    column: id("region"),
                    expected: expected.clone(),
                }]
            );
        }
        // MySQL `text` has no length
        let (desired, actual) = other_region(NativeType::named("text", "text"));
        let d = fill_and_allow(Param::Text("x".repeat(500)));
        assert!(run::<MySql>(&desired, &actual, &nulls, &d).is_ok());
    }

    #[test]
    fn a_column_no_value_binds_to_is_filled_by_hand() {
        let datetime = NativeType::named("datetime", "datetime");
        let (desired, actual) = other_region(datetime.clone());
        let by_hand = [Conflict::FillByHand {
            table: q("league"),
            column: id("region"),
            column_type: Logical::Other(datetime),
        }];
        let allowed = Decisions::from_config(true, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS);
        for d in [allowed, fill_and_allow(Param::Text("eu".into()))] {
            let err = run::<MsSql>(&desired, &actual, &region_nulls(), &d).unwrap_err();
            assert_eq!(err, by_hand);
        }
    }

    /// `t (a int, region text(20) nullable)` to `t` keyed on `columns`.
    fn keyed_on(columns: &[&str]) -> (SchemaModel, SchemaModel) {
        let cols = [
            ("a", Int, false),
            ("region", Text { max_len: Some(20) }, true),
        ];
        let actual = models(vec![plain("t", &cols)]);
        let mut want = plain("t", &cols);
        want.entity = Some("T".into());
        want.columns[1].nullable = false;
        want.primary_key = Some(PrimaryKey {
            name: None,
            columns: columns.iter().map(|c| id(c)).collect(),
            generated: None,
        });
        (models(vec![want]), actual)
    }

    #[test]
    fn a_fill_over_the_nulls_of_a_new_key_column_repeats_from_two_rows() {
        let nulls = |n| {
            results(&[
                (
                    Precheck::HasNulls {
                        table: q("t"),
                        column: id("region"),
                    },
                    1,
                ),
                (Precheck::RowCount(q("t")), n),
            ])
        };
        let fill = with_fill("t", "region", Param::Text("eu".into()));
        // Alone and next to a column that was there already
        for columns in [&["region"][..], &["a", "region"]] {
            let (desired, actual) = keyed_on(columns);
            let err = run::<PgDialect>(&desired, &actual, &nulls(2), &fill).unwrap_err();
            assert_eq!(
                err,
                [Conflict::DuplicateKeyValues {
                    table: q("t"),
                    columns: columns.iter().map(|c| id(c)).collect(),
                }]
            );
            assert!(run::<PgDialect>(&desired, &actual, &nulls(1), &fill).is_ok());
        }
        // No NULLs: nothing is filled and the precheck decides
        let (desired, actual) = keyed_on(&["a", "region"]);
        assert!(run::<PgDialect>(&desired, &actual, &PrecheckResults::default(), &fill).is_ok());
    }

    #[test]
    fn a_new_filled_column_next_to_an_existing_one_in_a_new_key_repeats_from_two_rows() {
        let actual = models(vec![plain("t", &[("a", Int, false)])]);
        let desired = models(vec![
            TableDef::builder("T", "t")
                .column("a", Int, false)
                .column("b", Int, false)
                .build()
                .map(|mut t| {
                    t.primary_key = Some(PrimaryKey {
                        name: None,
                        columns: vec![id("a"), id("b")],
                        generated: None,
                    });
                    t
                })
                .unwrap(),
        ]);
        let fill = with_fill("t", "b", Param::Int(1));
        let rows = |n| {
            results(&[
                (Precheck::TableHasRows(q("t")), 1),
                (Precheck::RowCount(q("t")), n),
            ])
        };
        let err = run::<PgDialect>(&desired, &actual, &rows(2), &fill).unwrap_err();
        assert_eq!(
            err,
            [Conflict::DuplicateKeyValues {
                table: q("t"),
                columns: vec![id("a"), id("b")],
            }]
        );
        let plan = run::<PgDialect>(&desired, &actual, &rows(1), &fill).unwrap();
        assert_eq!(
            lines(&plan),
            [
                "4 add column t.b (integer)",
                "4 fill empty values of t.b",
                "7 make t.b NOT NULL",
                "8 add primary key to t (a, b)"
            ]
        );
        // The existing column still has to be unique
        let mut dup = rows(1);
        dup.0.insert(
            Precheck::Duplicates {
                table: q("t"),
                columns: vec![id("a")],
            },
            1,
        );
        assert!(run::<PgDialect>(&desired, &actual, &dup, &fill).is_err());
    }

    #[test]
    fn a_new_key_over_filled_or_nullable_columns_counts_the_rows() {
        let actual = models(vec![plain("t", &[("a", Int, false)])]);
        let keyed = |columns: &[&str], extra: Option<&str>| {
            let mut b = TableDef::builder("T", "t").column("a", Int, false);
            if let Some(c) = extra {
                b = b.column(c, Int, false);
            }
            let mut t = b.build().unwrap();
            t.primary_key = Some(PrimaryKey {
                name: None,
                columns: columns.iter().map(|c| id(c)).collect(),
                generated: None,
            });
            models(vec![t])
        };
        let key_prechecks = |desired: &SchemaModel| {
            diff_of(desired, &actual, &Renames::default())
                .changes
                .into_iter()
                .find(|c| matches!(c.op, Op::AddPrimaryKey { .. }))
                .unwrap()
                .prechecks
        };
        assert_eq!(
            key_prechecks(&keyed(&["a", "b"], Some("b"))),
            [
                Precheck::Duplicates {
                    table: q("t"),
                    columns: vec![id("a")],
                },
                Precheck::RowCount(q("t")),
            ]
        );
        // Nothing can be filled: no count
        assert_eq!(
            key_prechecks(&keyed(&["a"], None)),
            [Precheck::Duplicates {
                table: q("t"),
                columns: vec![id("a")],
            }]
        );
    }

    #[test]
    fn a_rebuild_after_a_type_change_drops_the_text_collation() {
        let mut have = introspected(league_with(Int, None));
        let mut code = plain("x", &[("code", Text { max_len: Some(10) }, true)])
            .columns
            .remove(0);
        code.extra.collation = Some(CatalogSql::new("Latin1_General_CI_AS"));
        have.columns.push(code);
        let mut want = league();
        want.columns
            .push(plain("x", &[("code", Int, true)]).columns.remove(0));
        let d = Decisions::from_config(true, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS);
        let plan = run::<MsSql>(
            &models(vec![want]),
            &models(vec![have]),
            &PrecheckResults::default(),
            &d,
        )
        .unwrap();
        let [(_, to)] = rebuilds(&plan)[..] else {
            panic!("{:?}", lines(&plan))
        };
        let code = to.column("code").unwrap();
        assert_eq!(code.ty, Logical::Known(Int));
        assert_eq!(code.extra.collation, None);
    }

    #[test]
    fn conflicts_passed_in_come_back_with_the_planner_ones() {
        let mut have = introspected(league());
        have.columns
            .push(plain("x", &[("old", Int, true)]).columns.remove(0));
        let mut out = diff_of(
            &models(vec![league()]),
            &models(vec![have]),
            &Renames::default(),
        );
        let cycle = Conflict::RenameCycle {
            tables: vec![q("a"), q("b")],
        };
        out.conflicts.push(cycle.clone());
        let passed = Conflict::Internal("from validate".into());
        let err = finalize::<PgDialect>(
            out,
            &PrecheckResults::default(),
            &Decisions::default(),
            vec![passed.clone(), cycle.clone()],
        )
        .unwrap_err();
        assert_eq!(
            err,
            [
                passed,
                cycle,
                Conflict::Destructive {
                    op: Op::DropColumn {
                        table: q("league"),
                        column: id("old")
                    }
                },
            ]
        );
    }

    #[test]
    fn an_op_the_model_cant_apply_is_reported_not_planned() {
        let actual = models(vec![plain("t", &[("a", Int, true)])]);
        let out = DiffOutput {
            changes: vec![Change {
                op: Op::DropNotNull {
                    table: q("missing"),
                    column: id("a"),
                },
                risk: Risk::Safe,
                prechecks: Vec::new(),
            }],
            after_renames: actual.clone(),
            actual,
            ..DiffOutput::default()
        };
        let err = finalize::<PgDialect>(
            out,
            &PrecheckResults::default(),
            &Decisions::default(),
            Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(&err[..], [Conflict::Internal(_)]), "{err:?}");
    }

    // --- keys around type changes ---

    fn widened() -> (SchemaModel, SchemaModel) {
        (
            models(vec![
                league_with(BigInt, Some(Generated::Identity)),
                player_with("player", BigInt),
            ]),
            models(vec![introspected(league()), introspected(player())]),
        )
    }

    #[test]
    fn key_column_type_change_drops_and_adds_the_keys_around_it() {
        let (desired, actual) = widened();
        let plan = ok::<MsSql>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_league_fkey of player",
                "6 drop primary key of league",
                "7 change type of league.id to bigint",
                "7 change type of player.league to bigint",
                "8 add primary key to league (id)",
                "11 add foreign key player (league) to league",
            ]
        );
        let add_pk = plan.ops.iter().find_map(|p| match &p.op {
            Op::AddPrimaryKey { key, .. } => Some(key),
            _ => None,
        });
        assert_eq!(add_pk.unwrap().generated, Some(Generated::Identity));
        assert!(matches!(&plan.ops[5].op, Op::AddForeignKey { key, .. } if key.name.is_none()));

        let plan = ok::<PgDialect>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_league_fkey of player",
                "7 change type of league.id to bigint",
                "7 change type of player.league to bigint",
                "11 add foreign key player (league) to league",
            ]
        );
    }

    #[test]
    fn readded_foreign_key_keeps_a_hand_given_name() {
        let (desired, mut actual) = widened();
        let p = actual.tables.get_mut(&q("player")).unwrap();
        p.foreign_keys[0].name = Some(id("fk_by_hand"));
        let plan = ok::<PgDialect>(&desired, &actual);
        assert_eq!(
            plan.ops[0].op,
            Op::DropForeignKey {
                table: q("player"),
                name: id("fk_by_hand")
            }
        );
        assert!(
            matches!(&plan.ops.last().unwrap().op, Op::AddForeignKey { key, .. } if key.name == Some(id("fk_by_hand")))
        );
    }

    #[test]
    fn foreign_keys_to_a_dropped_primary_key_are_dropped_and_added_back() {
        let key = |a: &str, b: &str| PrimaryKey {
            name: None,
            columns: vec![id(a), id(b)],
            generated: None,
        };
        let mut want_l = plain("league", &[("a", Int, false), ("b", Int, false)]);
        want_l.primary_key = Some(key("b", "a"));
        let mut have_l = want_l.clone();
        have_l.primary_key = Some(key("a", "b"));
        let mut p = plain("player", &[("la", Int, true), ("lb", Int, true)]);
        p.foreign_keys.push(ForeignKey {
            name: None,
            columns: vec![id("la"), id("lb")],
            ref_table: q("league"),
            ref_columns: vec![id("a"), id("b")],
        });
        let desired = models(vec![want_l, p.clone()]);
        let actual = models(vec![introspected(have_l), introspected(p)]);
        let plan = ok::<PgDialect>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_la_lb_fkey of player",
                "6 drop primary key of league",
                "8 add primary key to league (b, a)",
                "11 add foreign key player (la, lb) to league",
            ]
        );
    }

    // --- SQL Server rebuilds ---

    fn identity_added() -> (SchemaModel, SchemaModel) {
        (
            models(vec![league(), player()]),
            models(vec![
                introspected(league_with(Int, None)),
                introspected(player()),
            ]),
        )
    }

    #[test]
    fn sql_server_rebuilds_a_table_to_add_an_identity() {
        let (desired, actual) = identity_added();
        let plan = ok::<MsSql>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_league_fkey of player",
                "9 rebuild table league",
                "11 add foreign key player (league) to league",
            ]
        );
        let [(from, to)] = rebuilds(&plan)[..] else {
            panic!()
        };
        assert_eq!(from.primary_key.as_ref().unwrap().generated, None);
        assert_eq!(
            to.primary_key.as_ref().unwrap().generated,
            Some(Generated::Identity)
        );

        let big = results(&[(Precheck::RowEstimate(q("league")), 2_500_000)]);
        let err = run::<MsSql>(&desired, &actual, &big, &Decisions::default()).unwrap_err();
        assert_eq!(
            err,
            [Conflict::LargeRebuild {
                table: q("league"),
                estimated_rows: 2_500_000
            }]
        );
        let mut d = Decisions::default();
        d.confirmed.insert(ConfirmKey::rebuild(&q("league")));
        assert!(run::<MsSql>(&desired, &actual, &big, &d).is_ok());
    }

    #[test]
    fn postgres_adds_the_identity_in_place() {
        let (desired, actual) = identity_added();
        let big = results(&[(Precheck::RowEstimate(q("league")), 2_500_000)]);
        let plan = run::<PgDialect>(&desired, &actual, &big, &Decisions::default()).unwrap();
        assert_eq!(lines(&plan), ["9 make league.id generated by the database"]);
    }

    #[test]
    fn rebuild_drops_and_adds_back_the_tables_own_foreign_keys() {
        let desired = models(vec![league(), player()]);
        let mut have = introspected(player());
        have.primary_key.as_mut().unwrap().generated = None;
        let actual = models(vec![introspected(league()), have]);
        let plan = ok::<MsSql>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_league_fkey of player",
                "9 rebuild table player",
                "11 add foreign key player (league) to league",
            ]
        );
        let [(from, to)] = rebuilds(&plan)[..] else {
            panic!()
        };
        assert!(from.foreign_keys.is_empty() && to.foreign_keys.is_empty());
    }

    #[test]
    fn moving_the_identity_to_another_column_is_one_rebuild() {
        let desired = models(vec![
            TableDef::builder("T", "t")
                .column("id", Int, false)
                .column("b", Int, false)
                .primary_key(Some(Generated::Identity))
                .build()
                .unwrap(),
        ]);
        let actual = models(vec![introspected(
            TableDef::builder("T", "t")
                .column("id", Int, false)
                .primary_key(Some(Generated::Identity))
                .column("b", Int, false)
                .build()
                .unwrap(),
        )]);
        let plan = ok::<MsSql>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "6 drop primary key of t",
                "8 add primary key to t (b)",
                "9 rebuild table t"
            ]
        );
        let [(_, to)] = rebuilds(&plan)[..] else {
            panic!()
        };
        let pk = to.primary_key.as_ref().unwrap();
        assert_eq!(
            (pk.columns.clone(), pk.generated),
            (vec![id("b")], Some(Generated::Identity))
        );

        let big = results(&[(Precheck::RowEstimate(q("t")), 2_500_000)]);
        let err = run::<MsSql>(&desired, &actual, &big, &Decisions::default()).unwrap_err();
        assert_eq!(
            err,
            [Conflict::LargeRebuild {
                table: q("t"),
                estimated_rows: 2_500_000
            }]
        );
    }

    #[test]
    fn dropping_the_identity_is_a_rebuild_too() {
        let desired = models(vec![league_with(Int, None)]);
        let actual = models(vec![introspected(league())]);
        let plan = ok::<MsSql>(&desired, &actual);
        let [(from, to)] = rebuilds(&plan)[..] else {
            panic!("{:?}", lines(&plan))
        };
        assert_eq!(
            from.primary_key.as_ref().unwrap().generated,
            Some(Generated::Identity)
        );
        assert_eq!(to.primary_key.as_ref().unwrap().generated, None);
    }

    #[test]
    fn rename_and_rebuild_in_one_plan() {
        // `player` becomes `gamer` and `league` gets an identity: the FK is dropped by
        // the names the database has before the rename
        let desired = models(vec![league(), player_with("gamer", Int)]);
        let actual = models(vec![
            introspected(league_with(Int, None)),
            introspected(player()),
        ]);
        let renames = Renames {
            tables: vec![(q("player"), q("gamer"))],
            ..Renames::default()
        };
        let plan = finalize::<MsSql>(
            diff_of(&desired, &actual, &renames),
            &PrecheckResults::default(),
            &Decisions::default(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_league_fkey of player",
                "2 rename table player to gamer",
                "9 rebuild table league",
                "11 add foreign key gamer (league) to league",
            ]
        );
        assert!(matches!(&plan.ops[3].op, Op::AddForeignKey { key, .. } if key.name.is_none()));
    }

    #[test]
    fn key_type_change_on_a_rebuilt_table_drops_each_key_once() {
        let desired = models(vec![
            league_with(BigInt, Some(Generated::Identity)),
            player_with("player", BigInt),
        ]);
        let actual = models(vec![
            introspected(league_with(Int, None)),
            introspected(player()),
        ]);
        let plan = ok::<MsSql>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_league_fkey of player",
                "6 drop primary key of league",
                "7 change type of league.id to bigint",
                "7 change type of player.league to bigint",
                "8 add primary key to league (id)",
                "9 rebuild table league",
                "11 add foreign key player (league) to league",
            ]
        );
    }

    // --- MySQL identity under a foreign key ---

    #[test]
    fn mysql_drops_the_foreign_keys_to_a_column_getting_an_identity() {
        let (desired, actual) = identity_added();
        let plan = ok::<MySql>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_league_fkey of player",
                "9 make league.id generated by the database",
                "11 add foreign key player (league) to league",
            ]
        );
        assert!(matches!(&plan.ops[2].op, Op::AddForeignKey { key, .. } if key.name.is_none()));
    }

    #[test]
    fn mysql_drops_the_foreign_keys_to_a_column_losing_its_identity() {
        let desired = models(vec![league_with(Int, None), player()]);
        let actual = models(vec![introspected(league()), introspected(player())]);
        let plan = ok::<MySql>(&desired, &actual);
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key player_league_fkey of player",
                "5 stop generating league.id",
                "11 add foreign key player (league) to league",
            ]
        );
        let plan = ok::<PgDialect>(&desired, &actual);
        assert_eq!(lines(&plan), ["5 stop generating league.id"]);
    }

    #[test]
    fn mysql_drops_the_own_foreign_key_of_a_column_getting_an_identity() {
        let profile = |generated| {
            TableDef::builder("Profile", "profile")
                .column("id", Int, false)
                .primary_key(generated)
                .foreign_key("league", "id")
                .build()
                .unwrap()
        };
        let desired = models(vec![league(), profile(Some(Generated::Identity))]);
        let actual = models(vec![introspected(league()), introspected(profile(None))]);
        assert_eq!(
            lines(&ok::<MySql>(&desired, &actual)),
            [
                "1 drop foreign key profile_id_fkey of profile",
                "9 make profile.id generated by the database",
                "11 add foreign key profile (id) to league",
            ]
        );
        assert_eq!(
            lines(&ok::<PgDialect>(&desired, &actual)),
            ["9 make profile.id generated by the database"]
        );
    }

    #[test]
    fn mysql_identity_change_keeps_a_hand_given_name_and_drops_by_the_old_names() {
        let desired = models(vec![league(), player_with("gamer", Int)]);
        let mut have = introspected(player());
        have.foreign_keys[0].name = Some(id("fk_by_hand"));
        let actual = models(vec![introspected(league_with(Int, None)), have]);
        let renames = Renames {
            tables: vec![(q("player"), q("gamer"))],
            ..Renames::default()
        };
        let plan = finalize::<MySql>(
            diff_of(&desired, &actual, &renames),
            &PrecheckResults::default(),
            &Decisions::default(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            lines(&plan),
            [
                "1 drop foreign key fk_by_hand of player",
                "2 rename table player to gamer",
                "9 make league.id generated by the database",
                "11 add foreign key gamer (league) to league",
            ]
        );
        assert!(
            matches!(&plan.ops[3].op, Op::AddForeignKey { key, .. } if key.name == Some(id("fk_by_hand")))
        );
    }

    #[test]
    fn mysql_identity_and_type_change_drop_the_key_once() {
        let desired = models(vec![
            league_with(BigInt, Some(Generated::Identity)),
            player_with("player", BigInt),
        ]);
        let actual = models(vec![
            introspected(league_with(Int, None)),
            introspected(player()),
        ]);
        assert_eq!(
            lines(&ok::<MySql>(&desired, &actual)),
            [
                "1 drop foreign key player_league_fkey of player",
                "7 change type of league.id to bigint",
                "7 change type of player.league to bigint",
                "9 make league.id generated by the database",
                "11 add foreign key player (league) to league",
            ]
        );
    }

    #[test]
    fn mysql_keeps_the_foreign_key_through_a_nullability_change() {
        let desired = models(vec![league(), player_with("player", Int)]);
        let mut have = player();
        have.columns[2].nullable = false;
        let actual = models(vec![introspected(league()), introspected(have)]);
        assert_eq!(
            lines(&ok::<MySql>(&desired, &actual)),
            ["7 make player.league nullable"]
        );
    }

    // --- generated columns ---

    fn scored(total_nullable: bool) -> TableDef {
        plain(
            "t",
            &[("points", Int, false), ("total", Int, total_nullable)],
        )
    }

    #[test]
    fn generated_column_is_not_rewritten_where_the_dialect_restates_it() {
        let desired = models(vec![scored(false)]);
        let actual = models(vec![generated(scored(true), "total")]);
        let err = run::<MySql>(
            &desired,
            &actual,
            &PrecheckResults::default(),
            &Decisions::default(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            [Conflict::GeneratedColumn {
                table: q("t"),
                column: id("total"),
                op: "make t.total NOT NULL".into()
            }]
        );
        assert!(
            run::<PgDialect>(
                &desired,
                &actual,
                &PrecheckResults::default(),
                &Decisions::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn generated_column_is_never_filled() {
        let desired = models(vec![scored(false)]);
        let actual = models(vec![generated(scored(true), "total")]);
        let nulls = results(&[(
            Precheck::HasNulls {
                table: q("t"),
                column: id("total"),
            },
            1,
        )]);
        let err = run::<PgDialect>(
            &desired,
            &actual,
            &nulls,
            &with_fill("t", "total", Param::Int(0)),
        )
        .unwrap_err();
        assert_eq!(
            err,
            [Conflict::GeneratedColumn {
                table: q("t"),
                column: id("total"),
                op: "fill empty values of t.total".into()
            }]
        );
    }

    #[test]
    fn sql_server_wont_rebuild_a_table_with_a_computed_column() {
        let mut want = scored(true);
        want.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec![id("points")],
            generated: Some(Generated::Identity),
        });
        let mut have = generated(scored(true), "total");
        have.primary_key = Some(PrimaryKey {
            name: Some(id("t_pkey")),
            columns: vec![id("points")],
            generated: None,
        });
        let err = run::<MsSql>(
            &models(vec![want]),
            &models(vec![have]),
            &PrecheckResults::default(),
            &Decisions::default(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            [Conflict::GeneratedColumn {
                table: q("t"),
                column: id("total"),
                op: "rebuild table t".into()
            }]
        );
    }

    #[test]
    fn type_change_on_a_table_with_generated_columns() {
        let desired = models(vec![plain(
            "t",
            &[("points", BigInt, false), ("total", Int, true)],
        )]);
        let actual = models(vec![generated(scored(true), "total")]);
        let conflict = Conflict::GeneratedColumnDependency {
            table: q("t"),
            column: id("points"),
            generated: vec![id("total")],
        };
        for err in [
            run::<PgDialect>(
                &desired,
                &actual,
                &PrecheckResults::default(),
                &Decisions::default(),
            ),
            run::<MsSql>(
                &desired,
                &actual,
                &PrecheckResults::default(),
                &Decisions::default(),
            ),
        ] {
            assert_eq!(err.unwrap_err(), std::slice::from_ref(&conflict));
        }
        assert!(
            run::<MySql>(
                &desired,
                &actual,
                &PrecheckResults::default(),
                &Decisions::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn blocked_drop_still_reports_the_generated_column() {
        let desired = models(vec![plain("t", &[("total", Int, true)])]);
        let actual = models(vec![generated(scored(true), "total")]);
        let err = run::<PgDialect>(
            &desired,
            &actual,
            &PrecheckResults::default(),
            &Decisions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err[..], [Conflict::Destructive { .. }, _]),
            "{err:?}"
        );
        assert!(err.contains(&Conflict::DependedOnByGenerated {
            table: q("t"),
            column: id("points"),
            generated: vec![id("total")],
        }));
    }

    #[test]
    fn dropping_a_column_of_a_table_with_generated_columns() {
        let desired = models(vec![plain("t", &[("total", Int, true)])]);
        let actual = models(vec![generated(scored(true), "total")]);
        let d = Decisions::from_config(true, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS);
        let conflict = Conflict::DependedOnByGenerated {
            table: q("t"),
            column: id("points"),
            generated: vec![id("total")],
        };
        for err in [
            run::<PgDialect>(&desired, &actual, &PrecheckResults::default(), &d),
            run::<MsSql>(&desired, &actual, &PrecheckResults::default(), &d),
            run::<MySql>(&desired, &actual, &PrecheckResults::default(), &d),
        ] {
            assert_eq!(err.unwrap_err(), std::slice::from_ref(&conflict));
        }
    }
}
