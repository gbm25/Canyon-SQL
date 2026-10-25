//! Desired vs actual, after renames, to changes with risk and prechecks.

use super::conflict::{Conflict, Info, Warning};
use super::decisions::Decisions;
use super::rename::{Previous, Renames};
use crate::dialect::{fk_name, pk_name};
use crate::model::{
    ColumnDef, Compat, ForeignKey, Ident, Logical, PrimaryKey, QualifiedName, SchemaModel,
    TableDef, compat,
};
use crate::op::Op;
use canyon_core::types::SqlType;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Safe,
    Checked,
    Destructive,
}

/// A read-only question for the database, asked before any SQL runs. Its names are
/// the ones the database has before the plan (before any rename).
///
/// On an `AddForeignKey` whose target table or columns the plan creates (empty when
/// the key is added), `TableHasRows` and `NonNull` finding rows mean orphan rows.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precheck {
    TableHasRows(QualifiedName),
    /// Rows where every one of `columns` has a value.
    NonNull {
        table: QualifiedName,
        columns: Vec<Ident>,
    },
    HasNulls {
        table: QualifiedName,
        column: Ident,
    },
    OutOfRange {
        table: QualifiedName,
        column: Ident,
        min: i64,
        max: i64,
    },
    LongerThan {
        table: QualifiedName,
        column: Ident,
        max_len: u32,
    },
    Duplicates {
        table: QualifiedName,
        columns: Vec<Ident>,
    },
    Orphans {
        table: QualifiedName,
        columns: Vec<Ident>,
        target: QualifiedName,
        target_columns: Vec<Ident>,
    },
    RowEstimate(QualifiedName),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub op: Op,
    pub risk: Risk,
    pub prechecks: Vec<Precheck>,
}

impl Change {
    pub(crate) fn safe(op: Op) -> Self {
        Self {
            op,
            risk: Risk::Safe,
            prechecks: Vec::new(),
        }
    }

    pub(crate) fn checked(op: Op, prechecks: Vec<Precheck>) -> Self {
        Self {
            op,
            risk: Risk::Checked,
            prechecks,
        }
    }

    pub(crate) fn destructive(op: Op) -> Self {
        Self {
            op,
            risk: Risk::Destructive,
            prechecks: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct DiffOutput {
    /// `MoveTable` and `RenameTable` changes come in the order they must run (one that
    /// frees a name goes before the one that takes it): the planner keeps that order.
    pub changes: Vec<Change>,
    pub infos: Vec<Info>,
    pub warnings: Vec<Warning>,
    /// `actual` with the moves and renames applied.
    pub after_renames: SchemaModel,
    /// The database as it was introspected.
    pub actual: SchemaModel,
    /// Name after the moves and renames to name in the database, for every table
    /// moved or renamed.
    pub old_names: BTreeMap<QualifiedName, QualifiedName>,
    /// Renames the diff can't apply (`RenameCycle`, `RenameTargetTaken`,
    /// `ColumnRenameTargetTaken`). Like any conflict, they stop the plan.
    pub conflicts: Vec<Conflict>,
}

/// The changes that bring `actual` to `desired`, once the moves and renames are applied.
///
/// Ops after phase 2 (moves and renames, see `phase`) use the names after them. `DropForeignKey`
/// (phase 1) and every precheck run on the database as it is, so they use the names it
/// has now.
pub fn diff(
    desired: &SchemaModel,
    actual: &SchemaModel,
    previous: &Previous,
    renames: &Renames,
    decisions: &Decisions,
) -> DiffOutput {
    let mut out = DiffOutput {
        actual: actual.clone(),
        ..DiffOutput::default()
    };
    let mut current = actual.clone();
    let mut old_columns = BTreeMap::new();
    // Sources of column renames onto a taken name: not dropped while the conflict stands
    let mut kept_columns = BTreeSet::new();
    let not_renamed = rename_tables(&mut current, renames, &mut out);
    for (table, old, new) in &renames.columns {
        // The pairs describe the old table: they don't apply to another one
        if not_renamed.contains(table) {
            continue;
        }
        let has = |c: &Ident| {
            (current.table(table)).is_some_and(|t| t.columns.iter().any(|x| &x.name == c))
        };
        if has(old) && has(new) {
            out.conflicts.push(Conflict::ColumnRenameTargetTaken {
                table: table.clone(),
                old: old.clone(),
                new: new.clone(),
            });
            kept_columns.insert((table.clone(), old.clone()));
            continue;
        }
        if rename_column_in_model(&mut current, table, old, new) {
            old_columns.insert((table.clone(), new.clone()), old.clone());
            out.changes.push(Change::safe(Op::RenameColumn {
                table: table.clone(),
                from: old.clone(),
                to: new.clone(),
            }));
        }
    }

    let ctx = Ctx {
        actual,
        old_tables: &out.old_names,
        old_columns: &old_columns,
        kept_columns: &kept_columns,
        previous,
        decisions,
    };
    let mut found = Found::default();
    for wanted in desired.tables.values() {
        match current.table(&wanted.name) {
            None => {
                let mut create = wanted.clone();
                create.foreign_keys.clear();
                found.changes.push(Change::safe(Op::CreateTable(create)));
                for fk in &wanted.foreign_keys {
                    found.changes.push(Change::safe(Op::AddForeignKey {
                        table: wanted.name.clone(),
                        key: fk.clone(),
                    }));
                }
            }
            Some(have) => ctx.table(wanted, have, &mut found),
        }
    }
    out.changes.append(&mut found.changes);
    out.infos = found.infos;
    out.warnings = found.warnings;
    for name in &renames.unmanaged {
        out.infos.push(Info::Unmanaged {
            table: name.clone(),
        });
    }
    out.after_renames = current;
    out
}

/// Renames a table the way the database does: foreign keys pointing at it follow.
/// Constraint names stay as they are. `false` (and no change) when `from` is missing
/// or `to` is taken.
pub(crate) fn rename_table_in(
    model: &mut SchemaModel,
    from: &QualifiedName,
    to: &QualifiedName,
) -> bool {
    if model.tables.contains_key(to) {
        return false;
    }
    let Some(mut t) = model.tables.remove(from) else {
        return false;
    };
    t.name = to.clone();
    model.tables.insert(to.clone(), t);
    retarget_references(model, from, to);
    true
}

/// Points every foreign key that references `from` at `to`.
pub(crate) fn retarget_references(
    model: &mut SchemaModel,
    from: &QualifiedName,
    to: &QualifiedName,
) {
    for fk in model
        .tables
        .values_mut()
        .flat_map(|t| t.foreign_keys.iter_mut())
    {
        if &fk.ref_table == from {
            fk.ref_table = to.clone();
        }
    }
}

/// Renames a column of `t`, in its columns and its own keys.
pub(crate) fn rename_column_in(t: &mut TableDef, old: &Ident, new: &Ident) {
    let keys = t
        .primary_key
        .iter_mut()
        .flat_map(|pk| pk.columns.iter_mut());
    let fks = t
        .foreign_keys
        .iter_mut()
        .flat_map(|fk| fk.columns.iter_mut());
    let columns = t.columns.iter_mut().map(|c| &mut c.name);
    for name in columns.chain(keys).chain(fks) {
        if name == old {
            *name = new.clone();
        }
    }
}

/// Points every foreign key that references `table.old` at `table.new`.
pub(crate) fn retarget_column_references(
    model: &mut SchemaModel,
    table: &QualifiedName,
    old: &Ident,
    new: &Ident,
) {
    for fk in model
        .tables
        .values_mut()
        .flat_map(|t| t.foreign_keys.iter_mut())
        .filter(|fk| &fk.ref_table == table)
    {
        for c in &mut fk.ref_columns {
            if c == old {
                *c = new.clone();
            }
        }
    }
}

/// Renames a column the way the database does. `false` (and no change) when the table
/// or the column is missing, or the new name is taken.
pub(crate) fn rename_column_in_model(
    model: &mut SchemaModel,
    table: &QualifiedName,
    old: &Ident,
    new: &Ident,
) -> bool {
    let Some(t) = model.tables.get_mut(table) else {
        return false;
    };
    if !t.columns.iter().any(|c| &c.name == old) || t.columns.iter().any(|c| &c.name == new) {
        return false;
    }
    rename_column_in(t, old, new);
    retarget_column_references(model, table, old, new);
    true
}

/// `t`'s name in the database before the plan: `old_names` maps the moved and renamed
/// tables back (`DiffOutput::old_names`).
pub(crate) fn name_in_db(
    old_names: &BTreeMap<QualifiedName, QualifiedName>,
    t: &QualifiedName,
) -> QualifiedName {
    old_names.get(t).unwrap_or(t).clone()
}

#[derive(Default)]
struct Found {
    changes: Vec<Change>,
    infos: Vec<Info>,
    warnings: Vec<Warning>,
}

struct Ctx<'a> {
    actual: &'a SchemaModel,
    /// After the moves and renames to before, for the ones that were applied.
    old_tables: &'a BTreeMap<QualifiedName, QualifiedName>,
    /// (table after, column after) to the column before.
    old_columns: &'a BTreeMap<(QualifiedName, Ident), Ident>,
    /// (table after, column) kept as it is: the source of a rename onto a taken name.
    kept_columns: &'a BTreeSet<(QualifiedName, Ident)>,
    previous: &'a Previous,
    decisions: &'a Decisions,
}

impl Ctx<'_> {
    /// The table's name in the database now.
    fn old_table(&self, t: &QualifiedName) -> QualifiedName {
        name_in_db(self.old_tables, t)
    }

    /// The column's name in the database now.
    fn old_column(&self, t: &QualifiedName, c: &Ident) -> Ident {
        self.old_columns
            .get(&(t.clone(), c.clone()))
            .unwrap_or(c)
            .clone()
    }

    /// The names the database has now, if every column is there already.
    fn existing_columns(
        &self,
        t: &QualifiedName,
        columns: &[Ident],
    ) -> Option<(QualifiedName, Vec<Ident>)> {
        let old = self.old_table(t);
        let table = self.actual.table(&old)?;
        let names = columns
            .iter()
            .map(|c| {
                let c = self.old_column(t, c);
                table.columns.iter().any(|have| have.name == c).then_some(c)
            })
            .collect::<Option<Vec<_>>>()?;
        Some((old, names))
    }

    /// The column as the previous model had it (before the applied renames).
    fn previous_column(&self, t: &QualifiedName, c: &Ident) -> Option<&ColumnDef> {
        let old_c = self.old_column(t, c);
        self.previous
            .model()
            .table(&self.old_table(t))?
            .columns
            .iter()
            .find(|col| col.name == old_c)
    }

    fn table(&self, wanted: &TableDef, have: &TableDef, out: &mut Found) {
        let t = &wanted.name;
        let old_t = self.old_table(t);

        for column in &wanted.columns {
            let Logical::Known(field) = column.ty else {
                continue;
            };
            let Some(existing) = have.columns.iter().find(|c| c.name == column.name) else {
                let add = Op::AddColumn {
                    table: t.clone(),
                    column: column.clone(),
                };
                out.changes.push(if column.nullable {
                    Change::safe(add)
                } else {
                    Change::checked(add, vec![Precheck::TableHasRows(old_t.clone())])
                });
                continue;
            };
            let old_c = self.old_column(t, &column.name);
            // Intent beats tolerance: a type the code changed since the last snapshot is
            // applied even where the column would be compatible. Without a real snapshot
            // there is no intent to read
            let explicit = self.previous.is_snapshot()
                && self
                    .previous_column(t, &column.name)
                    .is_some_and(|before| before.ty != column.ty);
            let tighten = self
                .decisions
                .tighten
                .contains(&(t.clone(), column.name.clone()));
            match compat(&existing.ty, field) {
                Compat::Exact => {}
                Compat::Wider if !explicit && !tighten => out.infos.push(Info::Tightenable {
                    table: t.clone(),
                    column: column.name.clone(),
                    column_type: existing.ty.clone(),
                    canonical: field,
                }),
                Compat::Narrower if !explicit => out.warnings.push(Warning::Narrower {
                    table: t.clone(),
                    column: column.name.clone(),
                    column_type: existing.ty.clone(),
                    field,
                }),
                _ => {
                    // The type only: nullability changes are their own ops
                    let mut to = column.clone();
                    to.nullable = existing.nullable;
                    out.changes
                        .push(type_change(t, &existing.ty, to, field, (&old_t, &old_c)));
                }
            }
            match (existing.nullable, column.nullable) {
                (true, false) => out.changes.push(Change::checked(
                    Op::SetNotNull {
                        table: t.clone(),
                        column: column.name.clone(),
                    },
                    vec![Precheck::HasNulls {
                        table: old_t.clone(),
                        column: old_c,
                    }],
                )),
                (false, true) => out.changes.push(Change::safe(Op::DropNotNull {
                    table: t.clone(),
                    column: column.name.clone(),
                })),
                _ => {}
            }
        }
        for existing in &have.columns {
            if wanted.columns.iter().all(|c| c.name != existing.name)
                && !(self.kept_columns).contains(&(t.clone(), existing.name.clone()))
            {
                out.changes.push(Change::destructive(Op::DropColumn {
                    table: t.clone(),
                    column: existing.name.clone(),
                }));
            }
        }

        self.primary_key(
            t,
            have.primary_key.as_ref(),
            wanted.primary_key.as_ref(),
            out,
        );
        self.foreign_keys(wanted, have, out);
    }

    fn primary_key(
        &self,
        t: &QualifiedName,
        have: Option<&PrimaryKey>,
        want: Option<&PrimaryKey>,
        out: &mut Found,
    ) {
        match (have, want) {
            (Some(hp), Some(wp)) if hp.columns == wp.columns => {
                if hp.generated.is_none() && wp.generated.is_some() {
                    out.changes.extend(self.identity(t, &wp.columns, true));
                } else if hp.generated.is_some() && wp.generated.is_none() {
                    out.changes.extend(self.identity(t, &hp.columns, false));
                }
            }
            (Some(hp), Some(wp)) => {
                if hp.generated.is_some() {
                    out.changes.extend(self.identity(t, &hp.columns, false));
                }
                out.changes.push(Change::safe(self.drop_primary_key(t, hp)));
                out.changes.push(self.add_primary_key(t, wp));
                if wp.generated.is_some() {
                    out.changes.extend(self.identity(t, &wp.columns, true));
                }
            }
            (Some(hp), None) => {
                if hp.generated.is_some() {
                    out.changes.extend(self.identity(t, &hp.columns, false));
                }
                out.changes
                    .push(Change::destructive(self.drop_primary_key(t, hp)));
            }
            (None, Some(wp)) => {
                out.changes.push(self.add_primary_key(t, wp));
                if wp.generated.is_some() {
                    out.changes.extend(self.identity(t, &wp.columns, true));
                }
            }
            (None, None) => {}
        }
    }

    /// Phase 6, after the renames: a name Canyon gave follows the table (PostgreSQL and
    /// SQL Server rename it with the table, MySQL ignores it).
    fn drop_primary_key(&self, t: &QualifiedName, hp: &PrimaryKey) -> Op {
        let old_t = self.old_table(t);
        let name = match &hp.name {
            Some(name) if old_t != *t && *name == pk_name(&old_t) => pk_name(t),
            Some(name) => name.clone(),
            None => pk_name(t),
        };
        Op::DropPrimaryKey {
            table: t.clone(),
            name,
        }
    }

    fn add_primary_key(&self, t: &QualifiedName, wp: &PrimaryKey) -> Change {
        // Identity travels only in AddIdentity / DropIdentity, never in the key
        let mut key = wp.clone();
        key.generated = None;
        // New key columns hold one value (NULL or the fill value) on every row, so the
        // existing ones decide whether there are duplicates
        let (old_t, columns) = self.existing_part(t, &wp.columns);
        let prechecks = if columns.is_empty() {
            Vec::new()
        } else {
            vec![Precheck::Duplicates {
                table: old_t,
                columns,
            }]
        };
        Change::checked(
            Op::AddPrimaryKey {
                table: t.clone(),
                key,
            },
            prechecks,
        )
    }

    /// The old names of the columns the database already has.
    fn existing_part(&self, t: &QualifiedName, columns: &[Ident]) -> (QualifiedName, Vec<Ident>) {
        let old_t = self.old_table(t);
        let have = self.actual.table(&old_t);
        let columns = columns
            .iter()
            .map(|c| self.old_column(t, c))
            .filter(|c| have.is_some_and(|h| h.columns.iter().any(|col| &col.name == c)))
            .collect();
        (old_t, columns)
    }

    /// `RowEstimate` is only used where the change rebuilds the table (SQL Server).
    /// Only a single-column key can be an identity.
    fn identity(&self, t: &QualifiedName, key: &[Ident], add: bool) -> Option<Change> {
        let [column] = key else {
            return None;
        };
        let column = column.clone();
        let op = if add {
            Op::AddIdentity {
                table: t.clone(),
                column,
            }
        } else {
            Op::DropIdentity {
                table: t.clone(),
                column,
            }
        };
        Some(Change::checked(
            op,
            vec![Precheck::RowEstimate(self.old_table(t))],
        ))
    }

    /// Compared by structure (columns and target), never by name: the desired model has
    /// no names, Canyon picks them when it creates the key.
    fn foreign_keys(&self, wanted: &TableDef, have: &TableDef, out: &mut Found) {
        let t = &wanted.name;
        let same = |a: &ForeignKey, b: &ForeignKey| {
            a.columns == b.columns && a.ref_table == b.ref_table && a.ref_columns == b.ref_columns
        };
        for fk in &have.foreign_keys {
            if wanted.foreign_keys.iter().any(|w| same(w, fk)) {
                continue;
            }
            // Phase 1, before the renames: the table's name and the key's name now
            let old_t = self.old_table(t);
            let name = fk.name.clone().unwrap_or_else(|| {
                let columns: Vec<Ident> =
                    fk.columns.iter().map(|c| self.old_column(t, c)).collect();
                fk_name(&old_t, &columns)
            });
            out.changes
                .push(Change::safe(Op::DropForeignKey { table: old_t, name }));
        }
        for fk in &wanted.foreign_keys {
            if have.foreign_keys.iter().any(|h| same(h, fk)) {
                continue;
            }
            let prechecks = self.orphan_prechecks(wanted, fk);
            out.changes.push(Change::checked(
                Op::AddForeignKey {
                    table: t.clone(),
                    key: fk.clone(),
                },
                prechecks,
            ));
        }
    }
}

impl Ctx<'_> {
    /// What finds rows the new key would leave pointing nowhere. A precheck can only
    /// ask about tables and columns the database has now.
    fn orphan_prechecks(&self, wanted: &TableDef, fk: &ForeignKey) -> Vec<Precheck> {
        let t = &wanted.name;
        let old_t = self.old_table(t);
        let have = self.actual.table(&old_t);
        let mut existing = Vec::new();
        for c in &fk.columns {
            let old_c = self.old_column(t, c);
            if have.is_some_and(|h| h.columns.iter().any(|col| col.name == old_c)) {
                existing.push(old_c);
            } else if wanted
                .columns
                .iter()
                .find(|col| &col.name == c)
                .is_none_or(|col| col.nullable)
            {
                // A new nullable column is NULL on every row: no row can point nowhere
                return Vec::new();
            }
        }
        match self.existing_columns(&fk.ref_table, &fk.ref_columns) {
            Some((target, target_columns)) if existing.len() == fk.columns.len() => {
                vec![Precheck::Orphans {
                    table: old_t,
                    columns: existing,
                    target,
                    target_columns,
                }]
            }
            // A new NOT NULL column gets its fill value on every row; whether the target
            // has it can't be asked before the plan
            Some(_) => Vec::new(),
            // The plan creates the target, empty when the key is added: any row with a
            // value points nowhere
            None if existing.is_empty() => vec![Precheck::TableHasRows(old_t)],
            None => vec![Precheck::NonNull {
                table: old_t,
                columns: existing,
            }],
        }
    }
}

/// Applies the moves, then the table renames (phase 2). Returns the new names
/// of the pairs not applied: their column pairs describe a table that isn't there.
///
/// - old table missing and new one there: applied by an earlier run, skipped, and the
///   pairs after it still run;
/// - both missing: skipped, the desired table is created;
/// - a rename after a move that didn't run: skipped too;
/// - a rename to another schema: a planner bug, `Internal`;
/// - new name kept by a table that stays, or names in a cycle: a conflict.
fn rename_tables<'a>(
    current: &mut SchemaModel,
    renames: &'a Renames,
    out: &mut DiffOutput,
) -> BTreeSet<&'a QualifiedName> {
    let moves = renames.moves.iter().map(|(old, new)| (old, new)).collect();
    let mut skipped = in_order(current, moves, out, |from, to| Op::MoveTable {
        from: from.clone(),
        to_schema: to.schema.clone(),
    });
    let mut pairs = Vec::new();
    for (old, new) in &renames.tables {
        if old.schema != new.schema {
            out.conflicts.push(Conflict::Internal(format!(
                "the rename of `{old}` to `{new}` changes the schema: it should come as a move"
            )));
            skipped.insert(new);
        } else if skipped.contains(old) {
            skipped.insert(new);
        } else {
            pairs.push((old, new));
        }
    }
    let not_renamed = in_order(current, pairs, out, |from, to| Op::RenameTable {
        from: from.clone(),
        to: to.name.clone(),
    });
    skipped.extend(not_renamed);
    skipped
}

/// Applies `pairs` in the order they can run: one whose new name another pair frees
/// goes after it. Returns the new names of the pairs not applied.
fn in_order<'a>(
    current: &mut SchemaModel,
    pairs: Vec<(&'a QualifiedName, &'a QualifiedName)>,
    out: &mut DiffOutput,
    op: impl Fn(&QualifiedName, &QualifiedName) -> Op,
) -> BTreeSet<&'a QualifiedName> {
    let mut not_applied = BTreeSet::new();
    let mut pending = Vec::new();
    for (old, new) in pairs {
        if current.tables.contains_key(old) {
            pending.push((old, new));
        } else if !current.tables.contains_key(new) {
            // Neither name exists: the desired table is created
            not_applied.insert(new);
        }
        // Old name gone and new one there: an earlier run applied it, the pairs
        // after it still run
    }
    loop {
        let before = pending.len();
        pending.retain(|&(old, new)| {
            if !rename_table_in(current, old, new) {
                return true;
            }
            // A table moved and then renamed maps back to its name before both
            let in_db = out.old_names.remove(old).unwrap_or_else(|| old.clone());
            out.old_names.insert(new.clone(), in_db);
            out.changes.push(Change::safe(op(old, new)));
            false
        });
        if pending.is_empty() || pending.len() == before {
            break;
        }
    }

    // Every pair left wants a name that is still taken
    let next: BTreeMap<&QualifiedName, &QualifiedName> = pending.iter().copied().collect();
    let mut cycles = BTreeSet::new();
    for &(old, new) in &pending {
        not_applied.insert(new);
        if !next.contains_key(new) {
            out.conflicts.push(Conflict::RenameTargetTaken {
                old: old.clone(),
                new: new.clone(),
            });
            continue;
        }
        let mut path = vec![old.clone()];
        let mut at = new;
        while let Some(&to) = next.get(at) {
            if at == old {
                path.sort();
                cycles.insert(path);
                break;
            }
            if path.len() > next.len() {
                break;
            }
            path.push(at.clone());
            at = to;
        }
    }
    // Pairs waiting on a reported pair (a chain into a cycle or a taken name) need
    // no conflict of their own
    out.conflicts.extend(
        cycles
            .into_iter()
            .map(|tables| Conflict::RenameCycle { tables }),
    );
    not_applied
}

/// Bytes, min and max of an integer type.
fn int_range(ty: SqlType) -> Option<(u8, i64, i64)> {
    match ty {
        SqlType::SmallInt => Some((2, i16::MIN.into(), i16::MAX.into())),
        SqlType::Int => Some((4, i32::MIN.into(), i32::MAX.into())),
        SqlType::BigInt => Some((8, i64::MIN, i64::MAX)),
        _ => None,
    }
}

/// `old` = the table and column names the database has now, for the prechecks.
fn type_change(
    t: &QualifiedName,
    from: &Logical,
    column: ColumnDef,
    to: SqlType,
    (old_t, old_c): (&QualifiedName, &Ident),
) -> Change {
    let op = Op::AlterColumnType {
        table: t.clone(),
        column,
    };
    let Logical::Known(from) = *from else {
        return Change::destructive(op);
    };
    if let (Some((from_bytes, ..)), Some((to_bytes, min, max))) = (int_range(from), int_range(to)) {
        return if to_bytes > from_bytes {
            Change::safe(op)
        } else {
            Change::checked(
                op,
                vec![Precheck::OutOfRange {
                    table: old_t.clone(),
                    column: old_c.clone(),
                    min,
                    max,
                }],
            )
        };
    }
    match (from, to) {
        (SqlType::Text { .. }, SqlType::Text { max_len: None }) => Change::safe(op),
        (SqlType::Text { max_len: old }, SqlType::Text { max_len: Some(n) }) => {
            if old.is_some_and(|old| old <= n) {
                Change::safe(op)
            } else {
                Change::checked(
                    op,
                    vec![Precheck::LongerThan {
                        table: old_t.clone(),
                        column: old_c.clone(),
                        max_len: n,
                    }],
                )
            }
        }
        (SqlType::Time { precision: a }, SqlType::Time { precision: b })
        | (SqlType::Timestamp { precision: a }, SqlType::Timestamp { precision: b })
            if b >= a =>
        {
            Change::safe(op)
        }
        _ => Change::destructive(op),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::planning::rename::{Previous, Renames};
    use crate::planning::test_support::{id, introspected, q};
    use canyon_core::types::SqlType::{self, *};

    fn table(cols: &[(&str, SqlType, bool)]) -> TableDef {
        let mut b = TableDef::builder("T", "t");
        for (c, ty, nullable) in cols {
            b = b.column(c, *ty, *nullable);
        }
        b.build().unwrap()
    }
    fn model(t: TableDef) -> SchemaModel {
        SchemaModel::from_tables(vec![t]).unwrap()
    }
    fn models(tables: Vec<TableDef>) -> SchemaModel {
        SchemaModel::from_tables(tables).unwrap()
    }
    fn run(desired: &SchemaModel, actual: &SchemaModel) -> DiffOutput {
        diff(
            desired,
            actual,
            &Previous::Synthesized(actual.clone()),
            &Renames::default(),
            &Decisions::default(),
        )
    }
    fn renamed(desired: &SchemaModel, actual: &SchemaModel, renames: &Renames) -> DiffOutput {
        diff(
            desired,
            actual,
            &Previous::Synthesized(actual.clone()),
            renames,
            &Decisions::default(),
        )
    }
    fn only(out: &DiffOutput) -> &Change {
        assert_eq!(out.changes.len(), 1, "{:?}", ops(out));
        &out.changes[0]
    }
    fn ops(out: &DiffOutput) -> Vec<String> {
        out.changes.iter().map(|c| c.op.describe()).collect()
    }
    fn text(n: Option<u32>) -> SqlType {
        Text { max_len: n }
    }
    fn league() -> TableDef {
        TableDef::builder("League", "league")
            .column("id", Int, false)
            .primary_key(Some(Generated::Identity))
            .column("name", text(Some(100)), false)
            .build()
            .unwrap()
    }
    fn player(table: &str) -> TableDef {
        TableDef::builder("Player", table)
            .column("id", Int, false)
            .primary_key(Some(Generated::Identity))
            .column("name", text(None), false)
            .column("league", Int, true)
            .foreign_key("league", "id")
            .build()
            .unwrap()
    }
    fn change(op: Op, risk: Risk, prechecks: Vec<Precheck>) -> Change {
        Change {
            op,
            risk,
            prechecks,
        }
    }

    // --- one test per kind of change ---

    #[test]
    fn new_table_is_created_without_foreign_keys_then_they_are_added() {
        let desired = models(vec![league(), player("player")]);
        let actual = models(vec![introspected(league())]);
        let out = run(&desired, &actual);
        let mut bare = player("player");
        let fk = bare.foreign_keys.remove(0);
        assert_eq!(
            out.changes,
            [
                change(Op::CreateTable(bare), Risk::Safe, vec![]),
                change(
                    Op::AddForeignKey {
                        table: q("player"),
                        key: fk
                    },
                    Risk::Safe,
                    vec![]
                ),
            ]
        );
    }

    #[test]
    fn nullable_column_is_a_safe_add() {
        let out = run(
            &model(table(&[("a", Int, true), ("b", Int, true)])),
            &model(table(&[("a", Int, true)])),
        );
        let c = only(&out);
        assert!(matches!(&c.op, Op::AddColumn { column, .. } if column.name == id("b")));
        assert_eq!((c.risk, c.prechecks.clone()), (Risk::Safe, vec![]));
    }

    #[test]
    fn not_null_column_on_existing_table_is_checked_for_rows() {
        let out = run(
            &model(table(&[("a", Int, true), ("b", Int, false)])),
            &model(table(&[("a", Int, true)])),
        );
        let c = only(&out);
        assert!(matches!(c.op, Op::AddColumn { .. }));
        assert_eq!(
            (c.risk, c.prechecks.clone()),
            (Risk::Checked, vec![Precheck::TableHasRows(q("t"))])
        );
    }

    #[test]
    fn column_only_in_the_database_is_dropped() {
        let out = run(
            &model(table(&[("a", Int, true)])),
            &model(table(&[("a", Int, true), ("b", Int, true)])),
        );
        assert_eq!(
            *only(&out),
            change(
                Op::DropColumn {
                    table: q("t"),
                    column: id("b")
                },
                Risk::Destructive,
                vec![]
            )
        );
    }

    #[test]
    fn exact_type_is_left_alone() {
        let out = run(
            &model(table(&[("a", text(Some(100)), false)])),
            &model(table(&[("a", text(Some(100)), false)])),
        );
        assert!(out.changes.is_empty() && out.infos.is_empty() && out.warnings.is_empty());
    }

    #[test]
    fn wider_column_is_an_info_unless_tightened() {
        let desired = model(table(&[("a", text(Some(100)), true)]));
        let actual = model(table(&[("a", text(Some(200)), true)]));
        let out = run(&desired, &actual);
        assert!(out.changes.is_empty());
        assert_eq!(
            out.infos,
            [Info::Tightenable {
                table: q("t"),
                column: id("a"),
                column_type: Logical::Known(text(Some(200))),
                canonical: text(Some(100)),
            }]
        );

        let mut decisions = Decisions::default();
        decisions.tighten.insert((q("t"), id("a")));
        let out = diff(
            &desired,
            &actual,
            &Previous::Synthesized(actual.clone()),
            &Renames::default(),
            &decisions,
        );
        assert!(out.infos.is_empty());
        let c = only(&out);
        assert!(matches!(c.op, Op::AlterColumnType { .. }));
        assert_eq!(
            (c.risk, c.prechecks.clone()),
            (
                Risk::Checked,
                vec![Precheck::LongerThan {
                    table: q("t"),
                    column: id("a"),
                    max_len: 100
                }]
            )
        );
    }

    #[test]
    fn narrower_column_is_a_warning() {
        let out = run(
            &model(table(&[("a", text(None), true)])),
            &model(table(&[("a", text(Some(100)), true)])),
        );
        assert!(out.changes.is_empty());
        assert_eq!(
            out.warnings,
            [Warning::Narrower {
                table: q("t"),
                column: id("a"),
                column_type: Logical::Known(text(Some(100))),
                field: text(None),
            }]
        );
    }

    #[test]
    fn incompatible_kind_change_is_destructive() {
        for (from, to) in [
            (text(None), Int),
            (Date, Timestamp { precision: 6 }),
            (Bool, SmallInt),
            (Time { precision: 6 }, Timestamp { precision: 6 }),
        ] {
            let out = run(
                &model(table(&[("a", to, true)])),
                &model(table(&[("a", from, true)])),
            );
            let c = only(&out);
            assert!(matches!(c.op, Op::AlterColumnType { .. }), "{from:?}");
            assert_eq!((c.risk, c.prechecks.clone()), (Risk::Destructive, vec![]));
        }
    }

    #[test]
    fn unknown_database_type_is_destructive() {
        let mut actual = table(&[("a", text(None), true)]);
        actual.columns[0].ty = Logical::Other(NativeType::named("xml", "xml"));
        let out = run(&model(table(&[("a", text(None), true)])), &model(actual));
        assert_eq!(only(&out).risk, Risk::Destructive);
    }

    #[test]
    fn set_not_null_checks_for_nulls() {
        let out = run(
            &model(table(&[("a", Int, false)])),
            &model(table(&[("a", Int, true)])),
        );
        assert_eq!(
            *only(&out),
            change(
                Op::SetNotNull {
                    table: q("t"),
                    column: id("a")
                },
                Risk::Checked,
                vec![Precheck::HasNulls {
                    table: q("t"),
                    column: id("a")
                }]
            )
        );
    }

    #[test]
    fn drop_not_null_is_safe() {
        let out = run(
            &model(table(&[("a", Int, true)])),
            &model(table(&[("a", Int, false)])),
        );
        assert_eq!(
            *only(&out),
            change(
                Op::DropNotNull {
                    table: q("t"),
                    column: id("a")
                },
                Risk::Safe,
                vec![]
            )
        );
    }

    #[test]
    fn type_change_keeps_nullability_out_of_the_op() {
        let out = run(
            &model(table(&[("a", BigInt, false)])),
            &model(table(&[("a", Int, true)])),
        );
        assert_eq!(out.changes.len(), 2, "{:?}", ops(&out));
        let Op::AlterColumnType { column, .. } = &out.changes[0].op else {
            panic!("{:?}", ops(&out));
        };
        assert!(column.nullable, "the type change keeps the column nullable");
        assert_eq!(column.ty, Logical::Known(BigInt));
        assert!(matches!(out.changes[1].op, Op::SetNotNull { .. }));
    }

    fn keyed(generated: Option<Generated>) -> TableDef {
        TableDef::builder("T", "t")
            .column("id", Int, false)
            .primary_key(generated)
            .column("b", Int, false)
            .build()
            .unwrap()
    }

    #[test]
    fn primary_key_added_checks_duplicates_and_adds_identity() {
        let out = run(
            &model(keyed(Some(Generated::Identity))),
            &model(table(&[("id", Int, false), ("b", Int, false)])),
        );
        assert_eq!(
            out.changes,
            [
                change(
                    Op::AddPrimaryKey {
                        table: q("t"),
                        key: keyed(None).primary_key.unwrap()
                    },
                    Risk::Checked,
                    vec![Precheck::Duplicates {
                        table: q("t"),
                        columns: vec![id("id")]
                    }]
                ),
                change(
                    Op::AddIdentity {
                        table: q("t"),
                        column: id("id")
                    },
                    Risk::Checked,
                    vec![Precheck::RowEstimate(q("t"))]
                ),
            ]
        );
    }

    #[test]
    fn primary_key_removed_is_destructive() {
        let out = run(
            &model(table(&[("id", Int, false), ("b", Int, false)])),
            &model(introspected(keyed(Some(Generated::Identity)))),
        );
        assert_eq!(
            out.changes,
            [
                change(
                    Op::DropIdentity {
                        table: q("t"),
                        column: id("id")
                    },
                    Risk::Checked,
                    vec![Precheck::RowEstimate(q("t"))]
                ),
                change(
                    Op::DropPrimaryKey {
                        table: q("t"),
                        name: id("t_pkey")
                    },
                    Risk::Destructive,
                    vec![]
                ),
            ]
        );
    }

    #[test]
    fn primary_key_columns_changed_is_drop_then_checked_add() {
        let desired = TableDef::builder("T", "t")
            .column("id", Int, false)
            .column("b", Int, false)
            .primary_key(None)
            .build()
            .unwrap();
        let mut actual = introspected(keyed(None));
        actual.primary_key.as_mut().unwrap().name = Some(id("PK__t__3213E83F"));
        let out = run(&model(desired.clone()), &model(actual));
        assert_eq!(
            out.changes,
            [
                change(
                    Op::DropPrimaryKey {
                        table: q("t"),
                        name: id("PK__t__3213E83F")
                    },
                    Risk::Safe,
                    vec![]
                ),
                change(
                    Op::AddPrimaryKey {
                        table: q("t"),
                        key: desired.primary_key.unwrap()
                    },
                    Risk::Checked,
                    vec![Precheck::Duplicates {
                        table: q("t"),
                        columns: vec![id("b")]
                    }]
                ),
            ]
        );
    }

    #[test]
    fn identity_on_and_off_on_the_same_key() {
        let plain = introspected(keyed(None));
        let generated = introspected(keyed(Some(Generated::Identity)));
        let on = run(&model(generated.clone()), &model(plain.clone()));
        assert_eq!(
            *only(&on),
            change(
                Op::AddIdentity {
                    table: q("t"),
                    column: id("id")
                },
                Risk::Checked,
                vec![Precheck::RowEstimate(q("t"))]
            )
        );
        let off = run(&model(plain), &model(generated));
        assert_eq!(
            *only(&off),
            change(
                Op::DropIdentity {
                    table: q("t"),
                    column: id("id")
                },
                Risk::Checked,
                vec![Precheck::RowEstimate(q("t"))]
            )
        );
    }

    #[test]
    fn postgres_serial_key_becomes_an_identity() {
        // A `serial` reads as a plain key with a `nextval` default: the dialect drops the
        // default and resyncs the identity, the diff has nothing special to do
        let mut actual = introspected(keyed(None));
        actual.columns[0].default = Some(ColumnDefault::Expr {
            constraint: None,
            sql: CatalogSql::new("nextval('t_id_seq'::regclass)"),
        });
        let out = run(&model(keyed(Some(Generated::Identity))), &model(actual));
        assert_eq!(
            *only(&out),
            change(
                Op::AddIdentity {
                    table: q("t"),
                    column: id("id")
                },
                Risk::Checked,
                vec![Precheck::RowEstimate(q("t"))]
            )
        );
    }

    #[test]
    fn foreign_key_only_in_the_database_is_dropped_by_its_name() {
        let mut desired = player("player");
        desired.foreign_keys.clear();
        let mut actual = introspected(player("player"));
        actual.foreign_keys[0].name = Some(id("FK__player__league__1234"));
        let out = run(
            &models(vec![league(), desired]),
            &models(vec![introspected(league()), actual]),
        );
        assert_eq!(
            *only(&out),
            change(
                Op::DropForeignKey {
                    table: q("player"),
                    name: id("FK__player__league__1234")
                },
                Risk::Safe,
                vec![]
            )
        );
    }

    #[test]
    fn foreign_key_added_to_existing_table_checks_orphans() {
        let mut actual = introspected(player("player"));
        actual.foreign_keys.clear();
        let out = run(
            &models(vec![league(), player("player")]),
            &models(vec![introspected(league()), actual]),
        );
        let c = only(&out);
        assert!(
            matches!(&c.op, Op::AddForeignKey { table, key } if *table == q("player") && key.name.is_none())
        );
        assert_eq!(
            (c.risk, c.prechecks.clone()),
            (
                Risk::Checked,
                vec![Precheck::Orphans {
                    table: q("player"),
                    columns: vec![id("league")],
                    target: q("league"),
                    target_columns: vec![id("id")],
                }]
            )
        );
    }

    #[test]
    fn foreign_key_keeps_a_hand_given_name() {
        let mut desired = player("player");
        desired.foreign_keys[0].name = Some(id("by_hand"));
        let mut actual = introspected(player("player"));
        actual.foreign_keys.clear();
        let out = run(
            &models(vec![league(), desired]),
            &models(vec![introspected(league()), actual]),
        );
        assert!(
            matches!(&only(&out).op, Op::AddForeignKey { key, .. } if key.name == Some(id("by_hand")))
        );
    }

    #[test]
    fn foreign_key_on_a_new_column_has_no_orphan_precheck() {
        // The new column is all NULL when the key is added; the precheck would ask
        // about a column the database doesn't have yet
        let mut actual = introspected(player("player"));
        actual.foreign_keys.clear();
        actual.columns.pop();
        let out = run(
            &models(vec![league(), player("player")]),
            &models(vec![introspected(league()), actual]),
        );
        assert_eq!(out.changes.len(), 2, "{:?}", ops(&out));
        assert!(matches!(out.changes[0].op, Op::AddColumn { .. }));
        assert!(matches!(out.changes[1].op, Op::AddForeignKey { .. }));
        assert_eq!(out.changes[1].prechecks, []);
    }

    #[test]
    fn foreign_key_to_a_new_table_checks_for_values() {
        // `player.league` has values, `league` is created by the plan: empty when the
        // key is added
        let mut actual = introspected(player("player"));
        actual.foreign_keys.clear();
        let out = run(&models(vec![league(), player("player")]), &model(actual));
        let add = out
            .changes
            .iter()
            .find(|c| matches!(c.op, Op::AddForeignKey { .. }))
            .unwrap();
        assert_eq!(add.risk, Risk::Checked);
        assert_eq!(
            add.prechecks,
            [Precheck::NonNull {
                table: q("player"),
                columns: vec![id("league")]
            }]
        );
    }

    #[test]
    fn new_not_null_foreign_key_column_to_a_new_table_checks_for_rows() {
        // Every row would get the fill value, and the new target is empty
        let desired_player = TableDef::builder("Player", "player")
            .column("id", Int, false)
            .column("league", Int, false)
            .foreign_key("league", "id")
            .build()
            .unwrap();
        let actual = table_named("player", &[("id", Int, false)]);
        let out = run(&models(vec![league(), desired_player]), &model(actual));
        let add = out
            .changes
            .iter()
            .find(|c| matches!(c.op, Op::AddForeignKey { .. }))
            .unwrap();
        assert_eq!(add.prechecks, [Precheck::TableHasRows(q("player"))]);
    }

    fn table_named(name: &str, cols: &[(&str, SqlType, bool)]) -> TableDef {
        let mut b = TableDef::builder("", name);
        for (c, ty, nullable) in cols {
            b = b.column(c, *ty, *nullable);
        }
        let mut t = b.build().unwrap();
        t.entity = None;
        t
    }

    #[test]
    fn primary_key_moving_to_another_identity_column() {
        let desired = TableDef::builder("T", "t")
            .column("id", Int, false)
            .column("b", Int, false)
            .primary_key(Some(Generated::Identity))
            .build()
            .unwrap();
        let out = run(
            &model(desired),
            &model(introspected(keyed(Some(Generated::Identity)))),
        );
        assert_eq!(
            ops(&out),
            [
                "stop generating t.id",
                "drop primary key of t",
                "add primary key to t (b)",
                "make t.b generated by the database",
            ]
        );
        assert_eq!(
            out.changes.iter().map(|c| c.risk).collect::<Vec<_>>(),
            [Risk::Checked, Risk::Safe, Risk::Checked, Risk::Checked]
        );
    }

    // --- type change risk ---

    #[test]
    fn integer_widening_is_safe() {
        for (from, to) in [(SmallInt, Int), (Int, BigInt), (SmallInt, BigInt)] {
            let out = run(
                &model(table(&[("a", to, true)])),
                &model(table(&[("a", from, true)])),
            );
            let c = only(&out);
            assert!(matches!(c.op, Op::AlterColumnType { .. }));
            assert_eq!(
                (c.risk, c.prechecks.clone()),
                (Risk::Safe, vec![]),
                "{from:?}"
            );
        }
    }

    #[test]
    fn integer_narrowing_is_range_checked() {
        let out = run(
            &model(table(&[("a", SmallInt, true)])),
            &model(table(&[("a", Int, true)])),
        );
        let c = only(&out);
        assert_eq!(
            c.prechecks,
            [Precheck::OutOfRange {
                table: q("t"),
                column: id("a"),
                min: i16::MIN.into(),
                max: i16::MAX.into()
            }]
        );
        let out = run(
            &model(table(&[("a", Int, true)])),
            &model(table(&[("a", BigInt, true)])),
        );
        let c = only(&out);
        assert_eq!(c.risk, Risk::Checked);
        assert_eq!(
            c.prechecks,
            [Precheck::OutOfRange {
                table: q("t"),
                column: id("a"),
                min: i32::MIN.into(),
                max: i32::MAX.into()
            }]
        );
    }

    #[test]
    fn longer_text_is_safe() {
        let out = run(
            &model(table(&[("a", text(Some(100)), true)])),
            &model(table(&[("a", text(Some(50)), true)])),
        );
        let c = only(&out);
        assert!(matches!(c.op, Op::AlterColumnType { .. }));
        assert_eq!((c.risk, c.prechecks.clone()), (Risk::Safe, vec![]));
    }

    #[test]
    fn unlimited_text_by_intent_is_safe() {
        let actual = model(table(&[("a", text(Some(50)), true)]));
        let out = diff(
            &model(table(&[("a", text(None), true)])),
            &actual,
            &Previous::Snapshot(actual.clone()),
            &Renames::default(),
            &Decisions::default(),
        );
        let c = only(&out);
        assert!(matches!(c.op, Op::AlterColumnType { .. }));
        assert_eq!((c.risk, c.prechecks.clone()), (Risk::Safe, vec![]));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn shorter_text_by_intent_is_length_checked_only_with_a_snapshot() {
        let desired = model(table(&[("a", text(Some(50)), true)]));
        let before = model(table(&[("a", text(Some(100)), true)]));
        let actual = before.clone();
        let out = diff(
            &desired,
            &actual,
            &Previous::Snapshot(before.clone()),
            &Renames::default(),
            &Decisions::default(),
        );
        let c = only(&out);
        assert!(matches!(c.op, Op::AlterColumnType { .. }));
        assert_eq!(
            (c.risk, c.prechecks.clone()),
            (
                Risk::Checked,
                vec![Precheck::LongerThan {
                    table: q("t"),
                    column: id("a"),
                    max_len: 50
                }]
            )
        );
        assert!(out.infos.is_empty());

        let out = diff(
            &desired,
            &actual,
            &Previous::Synthesized(before),
            &Renames::default(),
            &Decisions::default(),
        );
        assert!(out.changes.is_empty());
        assert!(matches!(out.infos[..], [Info::Tightenable { .. }]));
    }

    #[test]
    fn same_type_in_the_snapshot_keeps_the_tolerance() {
        let desired = model(table(&[("a", text(Some(50)), true)]));
        let actual = model(table(&[("a", text(Some(100)), true)]));
        let out = diff(
            &desired,
            &actual,
            &Previous::Snapshot(desired.clone()),
            &Renames::default(),
            &Decisions::default(),
        );
        assert!(out.changes.is_empty());
        assert!(matches!(out.infos[..], [Info::Tightenable { .. }]));
    }

    #[test]
    fn higher_precision_is_safe() {
        for (from, to) in [
            (Timestamp { precision: 0 }, Timestamp { precision: 3 }),
            (Time { precision: 0 }, Time { precision: 3 }),
        ] {
            let out = run(
                &model(table(&[("a", to, true)])),
                &model(table(&[("a", from, true)])),
            );
            let c = only(&out);
            assert!(matches!(c.op, Op::AlterColumnType { .. }));
            assert_eq!(
                (c.risk, c.prechecks.clone()),
                (Risk::Safe, vec![]),
                "{from:?}"
            );
        }
    }

    #[test]
    fn lower_precision_by_intent_is_destructive() {
        let actual = model(table(&[("a", Timestamp { precision: 6 }, true)]));
        let out = diff(
            &model(table(&[("a", Timestamp { precision: 3 }, true)])),
            &actual,
            &Previous::Snapshot(actual.clone()),
            &Renames::default(),
            &Decisions::default(),
        );
        let c = only(&out);
        assert!(matches!(c.op, Op::AlterColumnType { .. }));
        assert_eq!(c.risk, Risk::Destructive);
    }

    // --- renames ---

    #[test]
    fn renames_move_the_table_and_column_before_comparing() {
        let actual = models(vec![
            TableDef::builder("", "player_old")
                .column("id", Int, false)
                .column("nick", text(None), true)
                .build()
                .unwrap(),
        ]);
        let desired = models(vec![
            TableDef::builder("Player", "player")
                .column("id", Int, false)
                .column("name", text(None), true)
                .build()
                .unwrap(),
        ]);
        let renames = Renames {
            tables: vec![(q("player_old"), q("player"))],
            columns: vec![(q("player"), id("nick"), id("name"))],
            ..Renames::default()
        };
        let out = renamed(&desired, &actual, &renames);
        assert_eq!(
            ops(&out),
            [
                "rename table player_old to player",
                "rename column player.nick to name"
            ]
        );
        assert!(out.changes.iter().all(|c| c.risk == Risk::Safe));
        assert!(
            out.after_renames
                .table(&q("player"))
                .unwrap()
                .column("name")
                .is_some()
        );
        assert_eq!(out.actual, actual);
        assert_eq!(
            out.old_names,
            BTreeMap::from([(q("player"), q("player_old"))])
        );
    }

    #[test]
    fn rename_whose_source_is_missing_is_skipped() {
        let renames = Renames {
            tables: vec![(q("gone"), q("t"))],
            columns: vec![(q("t"), id("x"), id("a"))],
            ..Renames::default()
        };
        let out = renamed(
            &model(table(&[("a", Int, true)])),
            &SchemaModel::default(),
            &renames,
        );
        assert!(matches!(only(&out).op, Op::CreateTable(_)));
        assert!(out.old_names.is_empty());
    }

    #[test]
    fn column_renames_of_a_skipped_table_rename_are_skipped() {
        // `t` already exists, so `old` isn't renamed onto it: its column pairs don't
        // describe `t`'s columns
        let actual = models(vec![
            TableDef::builder("", "old")
                .column("x", Int, true)
                .build()
                .unwrap(),
            table(&[("x", Int, true), ("a", Int, true)]),
        ]);
        let renames = Renames {
            tables: vec![(q("old"), q("t"))],
            columns: vec![(q("t"), id("x"), id("a"))],
            ..Renames::default()
        };
        let out = renamed(
            &model(table(&[("x", Int, true), ("a", Int, true)])),
            &actual,
            &renames,
        );
        assert!(out.changes.is_empty(), "{:?}", ops(&out));
    }

    #[test]
    fn chained_renames_run_in_order_and_keep_the_data() {
        // `b` becomes `c` first, then `a` takes the free name `b`
        let actual = models(vec![
            table_named("a", &[("from_a", Int, true)]),
            table_named("b", &[("from_b", Int, true)]),
        ]);
        let desired = models(vec![
            table_named("b", &[("from_a", Int, true)]),
            table_named("c", &[("from_b", Int, true)]),
        ]);
        let renames = Renames {
            tables: vec![(q("a"), q("b")), (q("b"), q("c"))],
            ..Renames::default()
        };
        let out = renamed(&desired, &actual, &renames);
        assert_eq!(ops(&out), ["rename table b to c", "rename table a to b"]);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.after_renames.tables.len(), 2);
        assert!(
            out.after_renames
                .table(&q("b"))
                .unwrap()
                .column("from_a")
                .is_some()
        );
        assert!(
            out.after_renames
                .table(&q("c"))
                .unwrap()
                .column("from_b")
                .is_some()
        );
        assert_eq!(
            out.old_names,
            BTreeMap::from([(q("b"), q("a")), (q("c"), q("b"))])
        );
    }

    #[test]
    fn swapped_names_are_a_conflict() {
        let actual = models(vec![
            table_named("a", &[("x", Int, true)]),
            table_named("b", &[("y", Int, true)]),
        ]);
        let desired = models(vec![
            table_named("a", &[("y", Int, true)]),
            table_named("b", &[("x", Int, true)]),
        ]);
        let renames = Renames {
            tables: vec![(q("a"), q("b")), (q("b"), q("a"))],
            ..Renames::default()
        };
        let out = renamed(&desired, &actual, &renames);
        assert_eq!(
            out.conflicts,
            [Conflict::RenameCycle {
                tables: vec![q("a"), q("b")]
            }]
        );
        assert!(
            !out.changes
                .iter()
                .any(|c| matches!(c.op, Op::RenameTable { .. })),
            "{:?}",
            ops(&out)
        );
    }

    #[test]
    fn rename_onto_a_table_that_stays_is_a_conflict() {
        // `t` exists and isn't renamed away; `old` can't take its name
        let actual = models(vec![
            table_named("old", &[("x", Int, true)]),
            table(&[("x", Int, true)]),
        ]);
        let renames = Renames {
            tables: vec![(q("old"), q("t"))],
            ..Renames::default()
        };
        let out = renamed(&model(table(&[("x", Int, true)])), &actual, &renames);
        assert_eq!(
            out.conflicts,
            [Conflict::RenameTargetTaken {
                old: q("old"),
                new: q("t")
            }]
        );
    }

    #[test]
    fn rename_to_another_schema_is_a_planner_bug() {
        let renames = Renames {
            tables: vec![(q("t"), q("s.t2"))],
            ..Renames::default()
        };
        let desired = models(vec![
            TableDef::builder("T", "t2")
                .schema("s")
                .column("a", Int, true)
                .build()
                .unwrap(),
        ]);
        let out = renamed(&desired, &model(table(&[("a", Int, true)])), &renames);
        assert!(
            matches!(only(&out).op, Op::CreateTable(_)),
            "{:?}",
            ops(&out)
        );
        assert!(matches!(&out.conflicts[..], [Conflict::Internal(_)]));
    }

    #[test]
    fn unmanaged_tables_are_reported() {
        let renames = Renames {
            unmanaged: vec![q("old")],
            ..Renames::default()
        };
        let m = model(table(&[("a", Int, true)]));
        let out = renamed(&m, &m, &renames);
        assert!(out.changes.is_empty());
        assert_eq!(out.infos, [Info::Unmanaged { table: q("old") }]);
    }

    #[test]
    fn foreign_key_of_a_renamed_table_is_dropped_by_its_old_names() {
        let mut gamer = player("gamer");
        gamer.foreign_keys.clear();
        let renames = Renames {
            tables: vec![(q("player"), q("gamer"))],
            ..Renames::default()
        };
        let out = renamed(
            &models(vec![league(), gamer]),
            &models(vec![introspected(league()), introspected(player("player"))]),
            &renames,
        );
        assert_eq!(
            out.changes,
            [
                change(
                    Op::RenameTable {
                        from: q("player"),
                        to: id("gamer")
                    },
                    Risk::Safe,
                    vec![]
                ),
                change(
                    Op::DropForeignKey {
                        table: q("player"),
                        name: id("player_league_fkey")
                    },
                    Risk::Safe,
                    vec![]
                ),
            ]
        );
    }

    #[test]
    fn prechecks_use_the_names_before_the_renames() {
        let actual = models(vec![
            TableDef::builder("", "player")
                .column("id", Int, false)
                .column("nick", text(None), true)
                .build()
                .unwrap(),
        ]);
        let desired = models(vec![
            TableDef::builder("Player", "gamer")
                .column("id", Int, false)
                .primary_key(None)
                .column("name", text(None), false)
                .column("level", Int, false)
                .build()
                .unwrap(),
        ]);
        let renames = Renames {
            tables: vec![(q("player"), q("gamer"))],
            columns: vec![(q("gamer"), id("nick"), id("name"))],
            ..Renames::default()
        };
        let out = renamed(&desired, &actual, &renames);
        let prechecks: Vec<(String, Vec<Precheck>)> = out
            .changes
            .iter()
            .filter(|c| !c.prechecks.is_empty())
            .map(|c| (c.op.describe(), c.prechecks.clone()))
            .collect();
        assert_eq!(
            prechecks,
            [
                (
                    "make gamer.name NOT NULL".to_owned(),
                    vec![Precheck::HasNulls {
                        table: q("player"),
                        column: id("nick")
                    }]
                ),
                (
                    "add column gamer.level (integer)".to_owned(),
                    vec![Precheck::TableHasRows(q("player"))]
                ),
                (
                    "add primary key to gamer (id)".to_owned(),
                    vec![Precheck::Duplicates {
                        table: q("player"),
                        columns: vec![id("id")]
                    }]
                ),
            ]
        );
    }

    #[test]
    fn primary_key_of_a_renamed_table_is_dropped_by_its_name_after_the_rename() {
        let renames = Renames {
            tables: vec![(q("t"), q("u"))],
            ..Renames::default()
        };
        let desired = models(vec![
            TableDef::builder("T", "u")
                .column("id", Int, false)
                .column("b", Int, false)
                .build()
                .unwrap(),
        ]);
        // Canyon's name follows the table on PostgreSQL and SQL Server
        let out = renamed(&desired, &model(introspected(keyed(None))), &renames);
        assert_eq!(
            out.changes[1].op,
            Op::DropPrimaryKey {
                table: q("u"),
                name: id("u_pkey")
            }
        );
        // A hand-made name stays
        let mut actual = introspected(keyed(None));
        actual.primary_key.as_mut().unwrap().name = Some(id("by_hand"));
        let out = renamed(&desired, &model(actual), &renames);
        assert_eq!(
            out.changes[1].op,
            Op::DropPrimaryKey {
                table: q("u"),
                name: id("by_hand")
            }
        );
    }

    #[test]
    fn renamed_table_keeps_the_foreign_keys_pointing_at_it() {
        let team = TableDef::builder("League", "team")
            .column("id", Int, false)
            .primary_key(Some(Generated::Identity))
            .column("name", text(Some(100)), false)
            .build()
            .unwrap();
        let desired_player = TableDef::builder("Player", "player")
            .column("id", Int, false)
            .primary_key(Some(Generated::Identity))
            .column("name", text(None), false)
            .column("league", Int, true)
            .foreign_key("team", "id")
            .build()
            .unwrap();
        let renames = Renames {
            tables: vec![(q("league"), q("team"))],
            ..Renames::default()
        };
        let out = renamed(
            &models(vec![team, desired_player]),
            &models(vec![introspected(league()), introspected(player("player"))]),
            &renames,
        );
        assert_eq!(ops(&out), ["rename table league to team"]);
        let fk = &out.after_renames.table(&q("player")).unwrap().foreign_keys[0];
        assert_eq!(fk.ref_table, q("team"));
    }

    #[test]
    fn renamed_column_keeps_the_foreign_keys_pointing_at_it() {
        let desired_league = TableDef::builder("League", "league")
            .column("key", Int, false)
            .primary_key(Some(Generated::Identity))
            .column("name", text(Some(100)), false)
            .build()
            .unwrap();
        let desired_player = TableDef::builder("Player", "player")
            .column("id", Int, false)
            .primary_key(Some(Generated::Identity))
            .column("name", text(None), false)
            .column("league", Int, true)
            .foreign_key("league", "key")
            .build()
            .unwrap();
        let renames = Renames {
            columns: vec![(q("league"), id("id"), id("key"))],
            ..Renames::default()
        };
        let out = renamed(
            &models(vec![desired_league, desired_player]),
            &models(vec![introspected(league()), introspected(player("player"))]),
            &renames,
        );
        assert_eq!(ops(&out), ["rename column league.id to key"]);
        let fk = &out.after_renames.table(&q("player")).unwrap().foreign_keys[0];
        assert_eq!(fk.ref_columns, [id("key")]);
    }

    // --- no changes ---

    #[test]
    fn same_model_gives_no_changes() {
        let every_type = TableDef::builder("All", "all_types")
            .column("id", BigInt, false)
            .primary_key(Some(Generated::Identity))
            .column("b", Bool, true)
            .column("s", SmallInt, true)
            .column("i", Int, false)
            .column("t", text(None), true)
            .column("v", text(Some(20)), false)
            .column("d", Date, true)
            .column("tm", Time { precision: 3 }, true)
            .column("ts", Timestamp { precision: 6 }, true)
            .column("league", Int, true)
            .foreign_key("league", "id")
            .build()
            .unwrap();
        let desired = models(vec![league(), player("player"), every_type]);
        for m in [
            desired.clone(),
            models(desired.tables.values().cloned().map(introspected).collect()),
        ] {
            let out = run(&desired, &m);
            assert!(out.changes.is_empty(), "{:?}", ops(&out));
            assert!(out.infos.is_empty() && out.warnings.is_empty());
        }
    }
    // --- schemas ---

    fn in_schema(mut t: TableDef, schema: &str) -> TableDef {
        t.name.schema = Some(id(schema));
        t
    }
    fn moves(pairs: &[(&str, &str)], tables: &[(&str, &str)]) -> Renames {
        let qq = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (q(a), q(b))).collect();
        Renames {
            moves: qq(pairs),
            tables: qq(tables),
            ..Renames::default()
        }
    }

    #[test]
    fn move_runs_before_the_rename_and_maps_back_to_the_name_in_the_database() {
        let out = renamed(
            &models(vec![league(), in_schema(player("gamer"), "s")]),
            &models(vec![introspected(league()), introspected(player("player"))]),
            &moves(&[("player", "s.player")], &[("s.player", "s.gamer")]),
        );
        assert_eq!(
            ops(&out),
            [
                "move table player to schema s",
                "rename table s.player to gamer"
            ]
        );
        assert!(out.conflicts.is_empty());
        assert_eq!(out.old_names, BTreeMap::from([(q("s.gamer"), q("player"))]));
        assert!(out.after_renames.table(&q("s.gamer")).is_some());
    }

    #[test]
    fn foreign_keys_follow_a_moved_table() {
        let mut p = player("player");
        p.foreign_keys[0].ref_table = q("s.league");
        let out = renamed(
            &models(vec![in_schema(league(), "s"), p]),
            &models(vec![introspected(league()), introspected(player("player"))]),
            &moves(&[("league", "s.league")], &[]),
        );
        assert_eq!(
            only(&out).op,
            Op::MoveTable {
                from: q("league"),
                to_schema: Some(id("s"))
            }
        );
        let fk = &out.after_renames.table(&q("player")).unwrap().foreign_keys[0];
        assert_eq!(fk.ref_table, q("s.league"));
    }

    #[test]
    fn foreign_key_of_a_moved_table_is_dropped_by_its_old_name() {
        let mut gamer = in_schema(player("player"), "s");
        gamer.foreign_keys.clear();
        let out = renamed(
            &models(vec![league(), gamer]),
            &models(vec![introspected(league()), introspected(player("player"))]),
            &moves(&[("player", "s.player")], &[]),
        );
        assert_eq!(
            out.changes,
            [
                change(
                    Op::MoveTable {
                        from: q("player"),
                        to_schema: Some(id("s"))
                    },
                    Risk::Safe,
                    vec![]
                ),
                change(
                    Op::DropForeignKey {
                        table: q("player"),
                        name: id("player_league_fkey")
                    },
                    Risk::Safe,
                    vec![]
                ),
            ]
        );
    }

    #[test]
    fn orphan_precheck_finds_a_moved_target_by_its_old_name() {
        let mut p = player("player");
        p.foreign_keys[0].ref_table = q("s.league");
        let mut have = introspected(player("player"));
        have.foreign_keys.clear();
        let out = renamed(
            &models(vec![in_schema(league(), "s"), p]),
            &models(vec![introspected(league()), have]),
            &moves(&[("league", "s.league")], &[]),
        );
        assert_eq!(out.changes.len(), 2, "{:?}", ops(&out));
        assert_eq!(
            out.changes[1].prechecks,
            [Precheck::Orphans {
                table: q("player"),
                columns: vec![id("league")],
                target: q("league"),
                target_columns: vec![id("id")],
            }]
        );
    }

    #[test]
    fn a_rename_takes_the_name_a_move_frees() {
        // `b` goes to `s` as `c`, then `a` takes the free name `b`
        let actual = models(vec![
            table_named("a", &[("from_a", Int, true)]),
            table_named("b", &[("from_b", Int, true)]),
        ]);
        let desired = models(vec![
            table_named("b", &[("from_a", Int, true)]),
            in_schema(table_named("c", &[("from_b", Int, true)]), "s"),
        ]);
        let out = renamed(
            &desired,
            &actual,
            &moves(&[("b", "s.b")], &[("s.b", "s.c"), ("a", "b")]),
        );
        assert_eq!(
            ops(&out),
            [
                "move table b to schema s",
                "rename table s.b to c",
                "rename table a to b"
            ]
        );
        assert!(out.conflicts.is_empty());
        assert_eq!(
            out.old_names,
            BTreeMap::from([(q("b"), q("a")), (q("s.c"), q("b"))])
        );
    }

    #[test]
    fn move_onto_a_taken_name_is_a_conflict_and_its_rename_is_skipped() {
        let actual = models(vec![
            table_named("player", &[("x", Int, true)]),
            in_schema(table_named("player", &[("y", Int, true)]), "s"),
        ]);
        let desired = models(vec![
            in_schema(table_named("gamer", &[("x", Int, true)]), "s"),
            in_schema(table_named("player", &[("y", Int, true)]), "s"),
        ]);
        let mut renames = moves(&[("player", "s.player")], &[("s.player", "s.gamer")]);
        renames.columns = vec![(q("s.gamer"), id("y"), id("x"))];
        let out = renamed(&desired, &actual, &renames);
        assert_eq!(
            out.conflicts,
            [Conflict::RenameTargetTaken {
                old: q("player"),
                new: q("s.player")
            }]
        );
        assert_eq!(ops(&out), ["create table s.gamer"]);
        assert!(out.old_names.is_empty());
    }

    // --- a run cut off after some of the renames (MySQL) ---

    #[test]
    fn a_move_done_by_an_earlier_run_still_lets_its_rename_run() {
        let out = renamed(
            &models(vec![league(), in_schema(player("gamer"), "s")]),
            &models(vec![
                introspected(league()),
                in_schema(introspected(player("player")), "s"),
            ]),
            &moves(&[("player", "s.player")], &[("s.player", "s.gamer")]),
        );
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
        assert_eq!(ops(&out), ["rename table s.player to gamer"]);
        assert_eq!(
            out.old_names,
            BTreeMap::from([(q("s.gamer"), q("s.player"))])
        );
    }

    #[test]
    fn a_table_rename_done_by_an_earlier_run_still_lets_its_column_renames_run() {
        let have = |name: &str, column: &str| {
            TableDef::builder("Player", name)
                .column("id", Int, false)
                .column(column, text(None), true)
                .build()
                .unwrap()
        };
        let mut actual = have("player", "nick");
        actual.entity = None;
        let renames = Renames {
            tables: vec![(q("player_old"), q("player"))],
            columns: vec![(q("player"), id("nick"), id("name"))],
            ..Renames::default()
        };
        let out = renamed(&model(have("player", "name")), &model(actual), &renames);
        assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
        assert_eq!(ops(&out), ["rename column player.nick to name"]);
        assert!(out.changes.iter().all(|c| c.risk == Risk::Safe));
    }

    #[test]
    fn column_rename_onto_a_taken_name_is_a_conflict_and_keeps_the_source() {
        let actual = model(table(&[
            ("id", Int, false),
            ("nick", text(None), true),
            ("name", text(None), true),
        ]));
        let renames = Renames {
            columns: vec![(q("t"), id("nick"), id("name"))],
            ..Renames::default()
        };
        let out = renamed(
            &model(table(&[("id", Int, false), ("name", text(None), true)])),
            &actual,
            &renames,
        );
        assert_eq!(
            out.conflicts,
            [Conflict::ColumnRenameTargetTaken {
                table: q("t"),
                old: id("nick"),
                new: id("name"),
            }]
        );
        assert!(out.changes.is_empty(), "{:?}", ops(&out));
    }
}
