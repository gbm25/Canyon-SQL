//! Which tables and columns were renamed since the last run.
//!
//! Tables, first match wins: same name; same entity with another name; an old table
//! sharing enough columns with exactly one new one (both ways), else
//! `AmbiguousTableRename`. An old table with no match is left alone, not dropped.
//! Columns of a matched pair: same name; else one compatible candidate each way, else
//! `AmbiguousColumnRename`.

use super::conflict::Conflict;
use super::decisions::Decisions;
use crate::model::{
    ColumnDef, Compat, Ident, Logical, QualifiedName, SchemaModel, TableDef, compat,
};
use std::collections::BTreeSet;

/// An old table is a candidate for a new one when at least this share of its columns
/// (numerator, denominator; rounded up) is in the new one, same name and a compatible
/// type.
pub const RENAME_TABLE_MIN_SHARED: (usize, usize) = (1, 2);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Renames {
    /// Name to new name, same schema. For a moved table the name is the one after the move.
    pub tables: Vec<(QualifiedName, QualifiedName)>,
    /// Old name to the same name in another schema. They run before `tables`.
    pub moves: Vec<(QualifiedName, QualifiedName)>,
    /// (new table name, old column, new column).
    pub columns: Vec<(QualifiedName, Ident, Ident)>,
    /// Old tables with no match: no longer managed, not dropped.
    pub unmanaged: Vec<QualifiedName>,
}

#[derive(Debug, Clone)]
pub enum Previous {
    Snapshot(SchemaModel),
    Synthesized(SchemaModel),
}

impl Previous {
    pub fn model(&self) -> &SchemaModel {
        match self {
            Self::Snapshot(m) | Self::Synthesized(m) => m,
        }
    }
    pub fn is_snapshot(&self) -> bool {
        matches!(self, Self::Snapshot(_))
    }
}

#[derive(Debug, Clone)]
pub struct LegacyRow {
    pub struct_name: String,
    pub table: String,
}

/// First run: the database is the previous model; old `canyon_memory` rows give the struct names.
pub fn previous_from_database(actual: &SchemaModel, legacy: &[LegacyRow]) -> Previous {
    let mut model = actual.clone();
    for table in model.tables.values_mut() {
        table.entity = legacy
            .iter()
            .find(|row| table.name.schema.is_none() && row.table == table.name.name.as_str())
            .map(|row| row.struct_name.clone());
    }
    Previous::Synthesized(model)
}

fn compatible(old: &Logical, new: &Logical) -> bool {
    match new {
        Logical::Known(ty) => compat(old, *ty) != Compat::Incompatible,
        Logical::Other(_) => old == new,
    }
}

fn shared(old: &TableDef, new: &TableDef) -> usize {
    old.columns
        .iter()
        .filter(|o| {
            new.columns
                .iter()
                .any(|n| n.name == o.name && compatible(&o.ty, &n.ty))
        })
        .count()
}

fn threshold(old: &TableDef) -> usize {
    let (num, den) = RENAME_TABLE_MIN_SHARED;
    (old.columns.len() * num).div_ceil(den).max(1)
}

/// Renames since the previous model. Only old tables with an `entity` can be renamed or
/// end up in `unmanaged`: a snapshot has one for every table, and a model built from the
/// database has one only for tables an old `canyon_memory` row named. Tables someone made
/// by hand are never diffed or dropped, apart from a same-name match.
pub fn resolve(
    previous: &SchemaModel,
    desired: &SchemaModel,
    decisions: &Decisions,
) -> (Renames, Vec<Conflict>) {
    let mut renames = Renames::default();
    let mut conflicts = Vec::new();
    let mut old_left: BTreeSet<&QualifiedName> = previous.tables.keys().collect();
    let mut new_left: BTreeSet<&QualifiedName> = desired.tables.keys().collect();
    let mut pairs: Vec<(&TableDef, &TableDef)> = Vec::new();

    // 1. same name
    for name in new_left.clone() {
        if old_left.remove(name) {
            new_left.remove(name);
            pairs.push((&previous.tables[name], &desired.tables[name]));
        }
    }

    // answers, for old tables rule 1 didn't match. A target that is gone or taken is
    // ignored and the heuristics decide.
    for (old, answer) in &decisions.table_renames {
        if !old_left.contains(old) {
            continue;
        }
        match answer {
            Some(new) => {
                let Some(new) = new_left.get(new).copied() else {
                    continue;
                };
                pairs.push((&previous.tables[old], &desired.tables[new]));
                pair(old, new, &mut renames);
                new_left.remove(new);
            }
            None => renames.unmanaged.push(old.clone()),
        }
        old_left.remove(old);
    }

    // 2. same struct
    for old in old_left.clone() {
        let Some(entity) = &previous.tables[old].entity else {
            continue;
        };
        if let Some(new) = new_left
            .iter()
            .copied()
            .find(|n| desired.tables[*n].entity.as_ref() == Some(entity))
        {
            old_left.remove(old);
            new_left.remove(new);
            pairs.push((&previous.tables[old], &desired.tables[new]));
            pair(old, new, &mut renames);
        }
    }

    // 3. shared columns, unique both ways. `back` counts over every leftover old table,
    // not just the ones not visited yet.
    let rule3: BTreeSet<&QualifiedName> = old_left
        .iter()
        .copied()
        .filter(|o| previous.tables[*o].entity.is_some())
        .collect();
    let candidates = |old: &TableDef, pool: &BTreeSet<&QualifiedName>| -> Vec<QualifiedName> {
        pool.iter()
            .filter(|n| shared(old, &desired.tables[**n]) >= threshold(old))
            .map(|n| (*n).clone())
            .collect()
    };
    for old in old_left.clone() {
        let old_table = &previous.tables[old];
        if old_table.entity.is_none() {
            old_left.remove(old);
            continue;
        }
        let cands = candidates(old_table, &new_left);
        match cands.as_slice() {
            [] => renames.unmanaged.push(old.clone()),
            [new] => {
                let new_table = &desired.tables[new];
                let back: Vec<_> = rule3
                    .iter()
                    .filter(|o| {
                        shared(&previous.tables[**o], new_table) >= threshold(&previous.tables[**o])
                    })
                    .collect();
                if back.len() == 1 {
                    pairs.push((old_table, new_table));
                    pair(old, new, &mut renames);
                    new_left.remove(new);
                } else {
                    conflicts.push(Conflict::AmbiguousTableRename {
                        old: old.clone(),
                        candidates: cands.clone(),
                    });
                }
            }
            _ => conflicts.push(Conflict::AmbiguousTableRename {
                old: old.clone(),
                candidates: cands,
            }),
        }
        old_left.remove(old);
    }

    for (old, new) in pairs {
        columns(old, new, decisions, &mut renames, &mut conflicts);
    }
    (renames, conflicts)
}

/// Splits a matched pair in another schema into a move and, if the name changed too, a
/// rename in the new schema.
fn pair(old: &QualifiedName, new: &QualifiedName, renames: &mut Renames) {
    let mut from = old.clone();
    if old.schema != new.schema {
        from = QualifiedName {
            schema: new.schema.clone(),
            name: old.name.clone(),
        };
        renames.moves.push((old.clone(), from.clone()));
    }
    if from != *new {
        renames.tables.push((from, new.clone()));
    }
}

fn columns(
    old: &TableDef,
    new: &TableDef,
    decisions: &Decisions,
    renames: &mut Renames,
    conflicts: &mut Vec<Conflict>,
) {
    let mut removed: Vec<&ColumnDef> = old
        .columns
        .iter()
        .filter(|c| new.columns.iter().all(|n| n.name != c.name))
        .collect();
    let mut added: Vec<&ColumnDef> = new
        .columns
        .iter()
        .filter(|c| old.columns.iter().all(|o| o.name != c.name))
        .collect();

    removed.retain(|o| {
        match decisions
            .column_renames
            .get(&(new.name.clone(), o.name.clone()))
        {
            Some(Some(target)) => {
                if let Some(pos) = added.iter().position(|n| &n.name == target) {
                    renames
                        .columns
                        .push((new.name.clone(), o.name.clone(), target.clone()));
                    added.remove(pos);
                    false
                } else {
                    // taken or not there: back to the heuristics
                    true
                }
            }
            Some(None) => false, // drop and add
            None => true,
        }
    });

    let mut i = 0;
    while i < removed.len() {
        let o = removed[i];
        let cands: Vec<usize> = (0..added.len())
            .filter(|&k| compatible(&o.ty, &added[k].ty))
            .collect();
        if let [k] = cands[..] {
            let back = removed
                .iter()
                .filter(|r| compatible(&r.ty, &added[k].ty))
                .count();
            if back == 1 {
                renames
                    .columns
                    .push((new.name.clone(), o.name.clone(), added[k].name.clone()));
                added.remove(k);
                removed.remove(i);
                continue;
            }
        }
        i += 1;
    }

    if !removed.is_empty() && !added.is_empty() {
        conflicts.push(Conflict::AmbiguousColumnRename {
            table: new.name.clone(),
            removed: removed.into_iter().cloned().collect(),
            added: added.into_iter().cloned().collect(),
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TableDef;
    use crate::planning::test_support::q;
    use canyon_core::types::SqlType::*;

    fn t(entity: &str, name: &str, cols: &[(&str, canyon_core::types::SqlType)]) -> TableDef {
        let mut b = TableDef::builder(entity, name);
        for (c, ty) in cols {
            b = b.column(c, *ty, true);
        }
        b.build().unwrap()
    }
    fn m(tables: Vec<TableDef>) -> SchemaModel {
        SchemaModel::from_tables(tables).unwrap()
    }
    fn none() -> Decisions {
        Decisions::default()
    }

    #[test]
    fn same_table_name_is_the_same_table() {
        let (r, c) = resolve(
            &m(vec![t("Old", "player", &[("id", Int)])]),
            &m(vec![t("New", "player", &[("id", Int)])]),
            &none(),
        );
        assert!(r.tables.is_empty() && c.is_empty());
    }

    #[test]
    fn same_struct_new_table_name_is_a_rename() {
        let (r, c) = resolve(
            &m(vec![t("Player", "player", &[("id", Int)])]),
            &m(vec![t("Player", "players", &[("id", Int)])]),
            &none(),
        );
        assert_eq!(r.tables, [(q("player"), q("players"))]);
        assert!(c.is_empty());
    }

    #[test]
    fn struct_renamed_and_a_column_added_is_a_rename() {
        let prev = m(vec![t(
            "Player",
            "player",
            &[("id", Int), ("name", Text { max_len: None })],
        )]);
        let want = m(vec![t(
            "Gamer",
            "gamer",
            &[("id", Int), ("name", Text { max_len: None }), ("age", Int)],
        )]);
        let (r, c) = resolve(&prev, &want, &none());
        assert_eq!(r.tables, [(q("player"), q("gamer"))]);
        assert!(c.is_empty());
    }

    #[test]
    fn exactly_half_the_columns_is_enough() {
        let prev = m(vec![t("A", "a", &[("x", Int), ("y", Int)])]);
        let want = m(vec![t("B", "b", &[("x", Int), ("z", Date)])]);
        let (r, c) = resolve(&prev, &want, &none());
        assert_eq!(r.tables, [(q("a"), q("b"))]);
        assert!(
            matches!(c[..], [Conflict::AmbiguousColumnRename { .. }]),
            "y gone, z added: ask"
        );
    }

    #[test]
    fn less_than_half_is_not_a_rename() {
        let prev = m(vec![t("A", "a", &[("x", Int), ("y", Int), ("w", Int)])]);
        let want = m(vec![t("B", "b", &[("x", Int), ("z", Date)])]);
        let (r, c) = resolve(&prev, &want, &none());
        assert!(r.tables.is_empty() && c.is_empty());
        assert_eq!(r.unmanaged, [q("a")]);
    }

    #[test]
    fn an_earlier_old_table_that_also_matched_makes_it_ambiguous() {
        let prev = m(vec![
            t("A", "a", &[("x", Int), ("y", Int)]),
            t("B", "b", &[("x", Int), ("y", Int)]),
        ]);
        let want = m(vec![t("C", "c", &[("x", Int), ("y", Int)])]);
        let (r, c) = resolve(&prev, &want, &none());
        assert!(r.tables.is_empty(), "a and b both fit c: no rename");
        assert_eq!(c.len(), 2);
        assert!(
            c.iter()
                .all(|c| matches!(c, Conflict::AmbiguousTableRename { .. }))
        );
    }

    #[test]
    fn a_hand_made_table_is_left_alone_on_a_first_run() {
        let hand = TableDef::builder("", "notes");
        let hand = hand
            .column("id", Int, true)
            .column("body", Date, true)
            .build()
            .unwrap();
        let mut hand = hand;
        hand.entity = None;
        let actual = m(vec![hand]);
        let previous = previous_from_database(&actual, &[]);
        let want = m(vec![t(
            "Comment",
            "comment",
            &[("id", Int), ("text", Date)],
        )]);
        let (r, c) = resolve(previous.model(), &want, &none());
        assert!(r.tables.is_empty() && r.unmanaged.is_empty() && c.is_empty());
        // the same table in a snapshot (entity known) is a rename
        let snapshot = m(vec![t("Note", "notes", &[("id", Int), ("body", Date)])]);
        let (r, _) = resolve(&snapshot, &want, &none());
        assert_eq!(r.tables, [(q("notes"), q("comment"))]);
    }

    #[test]
    fn two_columns_answered_to_the_same_target_are_not_dropped() {
        let prev = m(vec![t("P", "p", &[("id", Int), ("a", Int), ("b", Int)])]);
        let want = m(vec![t("P", "p", &[("id", Int), ("c", Int), ("d", Date)])]);
        let mut d = none();
        let c_ = Ident::new("c").unwrap();
        d.column_renames
            .insert((q("p"), Ident::new("a").unwrap()), Some(c_.clone()));
        d.column_renames
            .insert((q("p"), Ident::new("b").unwrap()), Some(c_));
        let (r, c) = resolve(&prev, &want, &d);
        assert_eq!(r.columns.len(), 1);
        assert!(
            matches!(&c[..], [Conflict::AmbiguousColumnRename { removed, .. }] if removed.len() == 1)
        );
    }

    #[test]
    fn table_answers_allow_deny_and_stale() {
        let prev = m(vec![t("A", "a", &[("x", Int)])]);
        let want = m(vec![t("B", "b", &[("x", Int)])]);
        let mut d = none();
        d.table_renames.insert(q("a"), Some(q("b")));
        let (r, c) = resolve(&prev, &want, &d);
        assert_eq!(r.tables, [(q("a"), q("b"))]);
        assert!(c.is_empty());

        let mut d = none();
        d.table_renames.insert(q("a"), None);
        let (r, _) = resolve(&prev, &want, &d);
        assert!(r.tables.is_empty());
        assert_eq!(r.unmanaged, [q("a")]);

        // target not in the model: the heuristics decide (they find b)
        let mut d = none();
        d.table_renames.insert(q("a"), Some(q("gone")));
        let (r, _) = resolve(&prev, &want, &d);
        assert_eq!(r.tables, [(q("a"), q("b"))]);
        assert!(r.unmanaged.is_empty());

        // a deny for a name that still exists in the model is ignored
        let same = m(vec![t("A", "a", &[("x", Int)])]);
        let mut d = none();
        d.table_renames.insert(q("a"), None);
        let (r, _) = resolve(&prev, &same, &d);
        assert!(r.tables.is_empty() && r.unmanaged.is_empty());
    }

    #[test]
    fn two_candidates_is_ambiguous() {
        let prev = m(vec![t("A", "a", &[("x", Int), ("y", Int)])]);
        let want = m(vec![
            t("B", "b", &[("x", Int), ("y", Int)]),
            t("C", "c", &[("x", Int), ("y", Int)]),
        ]);
        let (_, c) = resolve(&prev, &want, &none());
        assert!(
            matches!(&c[..], [Conflict::AmbiguousTableRename { candidates, .. }] if candidates.len() == 2)
        );
    }

    #[test]
    fn one_compatible_column_pair_is_a_column_rename() {
        let prev = m(vec![t(
            "P",
            "p",
            &[("id", Int), ("name", Text { max_len: None })],
        )]);
        let want = m(vec![t(
            "P",
            "p",
            &[("id", Int), ("full_name", Text { max_len: None })],
        )]);
        let (r, c) = resolve(&prev, &want, &none());
        assert_eq!(
            r.columns,
            [(
                q("p"),
                Ident::new("name").unwrap(),
                Ident::new("full_name").unwrap()
            )]
        );
        assert!(c.is_empty());
    }

    #[test]
    fn two_strings_renamed_at_once_is_ambiguous() {
        let s = Text { max_len: None };
        let prev = m(vec![t("P", "p", &[("first_name", s), ("last_name", s)])]);
        let want = m(vec![t("P", "p", &[("given_name", s), ("family_name", s)])]);
        let (r, c) = resolve(&prev, &want, &none());
        assert!(r.columns.is_empty());
        assert!(matches!(c[..], [Conflict::AmbiguousColumnRename { .. }]));
    }

    #[test]
    fn removed_and_added_with_other_types_is_ambiguous() {
        let prev = m(vec![t("P", "p", &[("id", Int), ("age", Int)])]);
        let want = m(vec![t("P", "p", &[("id", Int), ("birth_date", Date)])]);
        let (_, c) = resolve(&prev, &want, &none());
        assert!(matches!(c[..], [Conflict::AmbiguousColumnRename { .. }]));
    }

    #[test]
    fn decisions_answer_the_ambiguity() {
        let prev = m(vec![t("P", "p", &[("id", Int), ("age", Int)])]);
        let want = m(vec![t("P", "p", &[("id", Int), ("birth_date", Date)])]);
        let mut d = none();
        d.column_renames.insert(
            (q("p"), Ident::new("age").unwrap()),
            Some(Ident::new("birth_date").unwrap()),
        );
        let (r, c) = resolve(&prev, &want, &d);
        assert!(c.is_empty());
        assert_eq!(r.columns.len(), 1);
        let mut d = none();
        d.column_renames
            .insert((q("p"), Ident::new("age").unwrap()), None);
        let (r, c) = resolve(&prev, &want, &d);
        assert!(c.is_empty() && r.columns.is_empty(), "None = drop and add");
    }

    #[test]
    fn only_removed_columns_are_not_renames() {
        let prev = m(vec![t("P", "p", &[("id", Int), ("nick", Int)])]);
        let want = m(vec![t("P", "p", &[("id", Int)])]);
        let (r, c) = resolve(&prev, &want, &none());
        assert!(
            r.columns.is_empty() && c.is_empty(),
            "the diff turns it into a destructive DropColumn"
        );
    }

    #[test]
    fn first_run_uses_legacy_memory_for_struct_names() {
        let actual = m(vec![t("ignored", "league", &[("id", Int)])]);
        let previous = previous_from_database(
            &actual,
            &[LegacyRow {
                struct_name: "League".into(),
                table: "league".into(),
            }],
        );
        assert!(!previous.is_snapshot());
        let want = m(vec![t("League", "leagues", &[("id", Int)])]);
        let (r, _) = resolve(previous.model(), &want, &none());
        assert_eq!(r.tables, [(q("league"), q("leagues"))]);
    }
    fn in_schema(entity: &str, schema: &str, name: &str) -> TableDef {
        TableDef::builder(entity, name)
            .schema(schema)
            .column("id", Int, true)
            .column("born", Date, true)
            .build()
            .unwrap()
    }

    #[test]
    fn same_struct_in_another_schema_is_a_move() {
        let prev = m(vec![t("Player", "player", &[("id", Int)])]);
        let moved = TableDef::builder("Player", "player")
            .schema("s")
            .column("id", Int, true)
            .build()
            .unwrap();
        let (r, c) = resolve(&prev, &m(vec![moved]), &none());
        assert_eq!(r.moves, [(q("player"), q("s.player"))]);
        assert!(r.tables.is_empty() && c.is_empty());
    }

    #[test]
    fn move_and_rename_together() {
        let prev = m(vec![t("Player", "player", &[("id", Int)])]);
        let moved = TableDef::builder("Player", "gamer")
            .schema("s")
            .column("id", Int, true)
            .build()
            .unwrap();
        let (r, _) = resolve(&prev, &m(vec![moved]), &none());
        assert_eq!(r.moves, [(q("player"), q("s.player"))]);
        assert_eq!(r.tables, [(q("s.player"), q("s.gamer"))]);
    }

    #[test]
    fn back_to_the_default_schema_is_a_move() {
        let prev = m(vec![in_schema("Player", "s", "player")]);
        let want = m(vec![t("Player", "player", &[("id", Int), ("born", Date)])]);
        let (r, c) = resolve(&prev, &want, &none());
        assert_eq!(r.moves, [(q("s.player"), q("player"))]);
        assert!(r.tables.is_empty() && c.is_empty());
    }

    #[test]
    fn shared_columns_work_across_schemas() {
        let prev = m(vec![t("A", "a", &[("id", Int), ("born", Date)])]);
        let want = m(vec![in_schema("B", "s", "b")]);
        let (r, c) = resolve(&prev, &want, &none());
        assert_eq!(r.moves, [(q("a"), q("s.a"))]);
        assert_eq!(r.tables, [(q("s.a"), q("s.b"))]);
        assert!(c.is_empty());
    }

    #[test]
    fn an_answer_in_another_schema_is_a_move_and_a_rename() {
        let prev = m(vec![t("A", "a", &[("x", Int)])]);
        let want = m(vec![in_schema("B", "s", "b"), t("C", "c", &[("x", Int)])]);
        let mut d = none();
        d.table_renames.insert(q("a"), Some(q("s.b")));
        let (r, c) = resolve(&prev, &want, &d);
        assert_eq!(r.moves, [(q("a"), q("s.a"))]);
        assert_eq!(r.tables, [(q("s.a"), q("s.b"))]);
        assert!(c.is_empty());
    }
}
