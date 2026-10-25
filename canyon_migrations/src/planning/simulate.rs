//! Ops applied to a model in memory: the property tests' oracle, and the SQL Server rebuild's source.

use super::diff::{rename_column_in_model, rename_table_in, retarget_references};
use crate::dialect::{fk_name, pk_name, restated};
use crate::model::{Compat, Generated, Logical, QualifiedName, SchemaModel, TableDef, compat};
use crate::op::Op;

/// Applies `ops` in order, the way the databases would. `Err` names the op that failed.
pub fn apply_in_memory(model: &SchemaModel, ops: &[Op]) -> Result<SchemaModel, String> {
    let mut m = model.clone();
    for op in ops {
        apply_one(&mut m, op).map_err(|e| format!("{}: {e}", op.describe()))?;
    }
    Ok(m)
}

/// The constraints named after `from` get the names the dialect gives them for `to`.
fn rename_constraints_like_the_dialect(t: &mut TableDef, from: &QualifiedName, to: &QualifiedName) {
    if let Some(pk) = t.primary_key.as_mut()
        && pk.name.as_ref() == Some(&pk_name(from))
    {
        pk.name = Some(pk_name(to));
    }
    for fk in &mut t.foreign_keys {
        if fk.name.as_ref() == Some(&fk_name(from, &fk.columns)) {
            fk.name = Some(fk_name(to, &fk.columns));
        }
    }
}

fn apply_one(m: &mut SchemaModel, op: &Op) -> Result<(), String> {
    match op {
        Op::CreateSchema { .. } | Op::FillColumn { .. } => {}
        Op::CreateTable(t) => {
            if m.tables.contains_key(&t.name) {
                return Err("table exists".into());
            }
            m.tables.insert(t.name.clone(), t.clone());
        }
        Op::RenameTable { from, to } => {
            let new = QualifiedName {
                schema: from.schema.clone(),
                name: to.clone(),
            };
            if m.tables.contains_key(&new) {
                return Err("table exists".into());
            }
            if !rename_table_in(m, from, &new) {
                return Err("no such table".into());
            }
            if let Some(t) = m.tables.get_mut(&new) {
                rename_constraints_like_the_dialect(t, from, &new);
            }
        }
        Op::MoveTable { from, to_schema } => {
            let new = QualifiedName {
                schema: to_schema.clone(),
                name: from.name.clone(),
            };
            if new != *from && m.tables.contains_key(&new) {
                return Err("table exists".into());
            }
            let mut t = m.tables.remove(from).ok_or("no such table")?;
            t.name = new.clone();
            m.tables.insert(new.clone(), t);
            retarget_references(m, from, &new);
        }
        Op::DropTable(name) => {
            m.tables.remove(name).ok_or("no such table")?;
        }
        Op::RebuildTable { from, to, .. } => {
            if m.tables.remove(&from.name).is_none() {
                return Err("no such table".into());
            }
            m.tables.insert(to.name.clone(), to.clone());
        }
        Op::RenameColumn { table, from, to } => {
            if !rename_column_in_model(m, table, from, to) {
                return Err("no such column, or the new name is taken".into());
            }
        }
        _ => {
            let name = op.table().ok_or("op without a table")?;
            let t = m.tables.get_mut(name).ok_or("no such table")?;
            apply_to_table(t, op)?;
        }
    }
    Ok(())
}

fn apply_to_table(t: &mut TableDef, op: &Op) -> Result<(), String> {
    match op {
        Op::AddColumn { column, .. } => {
            if t.columns.iter().any(|c| c.name == column.name) {
                return Err("column exists".into());
            }
            t.columns.push(column.clone());
        }
        Op::DropColumn { column, .. } => {
            let before = t.columns.len();
            t.columns.retain(|c| &c.name != column);
            if t.columns.len() == before {
                return Err("no such column".into());
            }
        }
        // The type only, never the nullability. Restated like the dialects do: no
        // collation on a non-text type, `ON UPDATE` only on a timestamp
        Op::AlterColumnType { column, .. } => {
            let c = t
                .columns
                .iter_mut()
                .find(|c| c.name == column.name)
                .ok_or("no such column")?;
            match &column.ty {
                Logical::Known(ty) => *c = restated(c, *ty),
                Logical::Other(_) => c.ty = column.ty.clone(),
            }
        }
        Op::SetNotNull { column, .. } | Op::DropNotNull { column, .. } => {
            let c = t
                .columns
                .iter_mut()
                .find(|c| &c.name == column)
                .ok_or("no such column")?;
            c.nullable = matches!(op, Op::DropNotNull { .. });
        }
        Op::AddPrimaryKey { key, .. } => t.primary_key = Some(key.clone()),
        Op::DropPrimaryKey { .. } => t.primary_key = None,
        Op::AddIdentity { .. } | Op::DropIdentity { .. } => {
            let pk = t.primary_key.as_mut().ok_or("no primary key")?;
            pk.generated = matches!(op, Op::AddIdentity { .. }).then_some(Generated::Identity);
        }
        Op::AddForeignKey { key, .. } => t.foreign_keys.push(key.clone()),
        Op::DropForeignKey { name, .. } => {
            let before = t.foreign_keys.len();
            let table_name = t.name.clone();
            t.foreign_keys.retain(|fk| {
                fk.name.as_ref() != Some(name) && fk_name(&table_name, &fk.columns) != *name
            });
            if t.foreign_keys.len() == before {
                return Err("no such foreign key".into());
            }
        }
        Op::CreateSchema { .. }
        | Op::FillColumn { .. }
        | Op::CreateTable(_)
        | Op::RenameTable { .. }
        | Op::MoveTable { .. }
        | Op::DropTable(_)
        | Op::RebuildTable { .. }
        | Op::RenameColumn { .. } => return Err(format!("{} isn't a column op", op.kind())),
    }
    Ok(())
}

/// `result` meets `desired`: same tables (desired in result), same column names, types
/// compatible (not `Incompatible`), same nullability, same PK columns and identity, same
/// FKs by structure.
pub fn satisfies(result: &SchemaModel, desired: &SchemaModel) -> Result<(), String> {
    for want in desired.tables.values() {
        let t = &want.name;
        let have = result.table(t).ok_or_else(|| format!("{t}: missing"))?;
        for wc in &want.columns {
            let hc = have
                .columns
                .iter()
                .find(|c| c.name == wc.name)
                .ok_or_else(|| format!("{t}.{}: missing", wc.name))?;
            if let Logical::Known(field) = &wc.ty
                && compat(&hc.ty, *field) == Compat::Incompatible
            {
                return Err(format!(
                    "{t}.{}: {:?} doesn't hold {field:?}",
                    wc.name, hc.ty
                ));
            }
            if hc.nullable != wc.nullable {
                return Err(format!(
                    "{t}.{}: nullable {} vs {}",
                    wc.name, hc.nullable, wc.nullable
                ));
            }
        }
        if have.columns.len() != want.columns.len() {
            return Err(format!(
                "{t}: {} columns, {} wanted",
                have.columns.len(),
                want.columns.len()
            ));
        }
        let pk = |x: &TableDef| {
            x.primary_key
                .as_ref()
                .map(|p| (p.columns.clone(), p.generated))
        };
        if pk(have) != pk(want) {
            return Err(format!("{t}: primary key {:?} vs {:?}", pk(have), pk(want)));
        }
        for fk in &want.foreign_keys {
            let found = have.foreign_keys.iter().any(|h| {
                h.columns == fk.columns
                    && h.ref_table == fk.ref_table
                    && h.ref_columns == fk.ref_columns
            });
            if !found {
                return Err(format!("{t}: foreign key {:?} missing", fk.columns));
            }
        }
        if have.foreign_keys.len() != want.foreign_keys.len() {
            return Err(format!(
                "{t}: {} foreign keys, {} wanted",
                have.foreign_keys.len(),
                want.foreign_keys.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, PrimaryKey};
    use crate::planning::test_support::{id, q};
    use canyon_core::types::SqlType;

    fn col(name: &str, ty: SqlType, nullable: bool) -> ColumnDef {
        TableDef::builder("E", "x")
            .column(name, ty, nullable)
            .build()
            .unwrap()
            .columns
            .remove(0)
    }
    fn model(tables: Vec<TableDef>) -> SchemaModel {
        SchemaModel::from_tables(tables).unwrap()
    }
    fn base() -> SchemaModel {
        model(vec![
            TableDef::builder("P", "parent")
                .column("id", SqlType::Int, false)
                .primary_key(None)
                .build()
                .unwrap(),
            TableDef::builder("C", "child")
                .column("id", SqlType::Int, false)
                .primary_key(None)
                .column("pid", SqlType::Int, false)
                .foreign_key("parent", "id")
                .build()
                .unwrap(),
        ])
    }
    fn apply(m: &SchemaModel, op: Op) -> Result<SchemaModel, String> {
        apply_in_memory(m, &[op])
    }
    fn child(m: &SchemaModel) -> &TableDef {
        m.table(&q("child")).unwrap()
    }

    #[test]
    fn create_table_and_duplicate() {
        let t = TableDef::builder("N", "new")
            .column("a", SqlType::Int, true)
            .build()
            .unwrap();
        let m = apply(&base(), Op::CreateTable(t.clone())).unwrap();
        assert!(m.table(&q("new")).is_some());
        assert!(apply(&m, Op::CreateTable(t)).is_err());
    }

    #[test]
    fn schema_and_fill_have_no_effect() {
        let m = base();
        let ops = [
            Op::CreateSchema { schema: id("s") },
            Op::FillColumn {
                table: q("child"),
                column: id("pid"),
                value: crate::op::Param::Int(0),
            },
        ];
        assert_eq!(apply_in_memory(&m, &ops).unwrap(), m);
    }

    #[test]
    fn rename_table_follows_fks_and_names() {
        let mut m = base();
        let t = m.tables.get_mut(&q("child")).unwrap();
        t.primary_key.as_mut().unwrap().name = Some(pk_name(&q("child")));
        t.foreign_keys[0].name = Some(fk_name(&q("child"), &[id("pid")]));
        let r = apply(
            &m,
            Op::RenameTable {
                from: q("child"),
                to: id("kid"),
            },
        )
        .unwrap();
        let kid = r.table(&q("kid")).unwrap();
        assert!(r.table(&q("child")).is_none());
        assert_eq!(
            kid.primary_key.as_ref().unwrap().name,
            Some(pk_name(&q("kid")))
        );
        assert_eq!(
            kid.foreign_keys[0].name,
            Some(fk_name(&q("kid"), &[id("pid")]))
        );
        let r = apply(
            &base(),
            Op::RenameTable {
                from: q("parent"),
                to: id("mother"),
            },
        )
        .unwrap();
        assert_eq!(child(&r).foreign_keys[0].ref_table, q("mother"));
        let clash = Op::RenameTable {
            from: q("child"),
            to: id("parent"),
        };
        assert!(apply(&base(), clash).is_err());
    }

    #[test]
    fn move_table_retargets_references() {
        let r = apply(
            &base(),
            Op::MoveTable {
                from: q("parent"),
                to_schema: Some(id("s")),
            },
        )
        .unwrap();
        assert!(r.table(&q("s.parent")).is_some());
        assert!(r.table(&q("parent")).is_none());
        assert_eq!(child(&r).foreign_keys[0].ref_table, q("s.parent"));
    }

    #[test]
    fn drop_table() {
        let r = apply(&base(), Op::DropTable(q("child"))).unwrap();
        assert!(r.table(&q("child")).is_none());
        assert!(apply(&r, Op::DropTable(q("child"))).is_err());
    }

    #[test]
    fn rebuild_replaces_the_table() {
        let from = child(&base()).clone();
        let mut to = from.clone();
        to.columns.pop();
        to.foreign_keys.clear();
        let r = apply(
            &base(),
            Op::RebuildTable {
                from,
                to: to.clone(),
                column_map: vec![],
            },
        )
        .unwrap();
        assert_eq!(child(&r), &to);
    }

    #[test]
    fn add_rename_drop_column() {
        let m = apply(
            &base(),
            Op::AddColumn {
                table: q("child"),
                column: col("n", SqlType::Int, true),
            },
        )
        .unwrap();
        assert!(child(&m).column("n").is_some());
        let add_again = Op::AddColumn {
            table: q("child"),
            column: col("n", SqlType::Int, true),
        };
        assert!(apply(&m, add_again).is_err());
        let m = apply(
            &m,
            Op::RenameColumn {
                table: q("child"),
                from: id("pid"),
                to: id("parent_id"),
            },
        )
        .unwrap();
        assert_eq!(child(&m).foreign_keys[0].columns, vec![id("parent_id")]);
        let m = apply(
            &m,
            Op::DropColumn {
                table: q("child"),
                column: id("n"),
            },
        )
        .unwrap();
        assert!(child(&m).column("n").is_none());
        let missing = Op::DropColumn {
            table: q("child"),
            column: id("n"),
        };
        assert!(apply(&m, missing).is_err());
    }

    #[test]
    fn rename_column_retargets_fk_refs() {
        let r = apply(
            &base(),
            Op::RenameColumn {
                table: q("parent"),
                from: id("id"),
                to: id("key"),
            },
        )
        .unwrap();
        assert_eq!(child(&r).foreign_keys[0].ref_columns, vec![id("key")]);
    }

    #[test]
    fn alter_type_keeps_nullability() {
        let r = apply(
            &base(),
            Op::AlterColumnType {
                table: q("child"),
                column: col("pid", SqlType::BigInt, true),
            },
        )
        .unwrap();
        let c = child(&r).column("pid").unwrap();
        assert_eq!(c.ty, Logical::Known(SqlType::BigInt));
        assert!(!c.nullable);
    }

    #[test]
    fn not_null_toggles() {
        let m = apply(
            &base(),
            Op::DropNotNull {
                table: q("child"),
                column: id("pid"),
            },
        )
        .unwrap();
        assert!(child(&m).column("pid").unwrap().nullable);
        let m = apply(
            &m,
            Op::SetNotNull {
                table: q("child"),
                column: id("pid"),
            },
        )
        .unwrap();
        assert!(!child(&m).column("pid").unwrap().nullable);
    }

    #[test]
    fn primary_key_and_identity() {
        let m = apply(
            &base(),
            Op::DropPrimaryKey {
                table: q("child"),
                name: id("child_pkey"),
            },
        )
        .unwrap();
        assert!(child(&m).primary_key.is_none());
        let no_pk = Op::AddIdentity {
            table: q("child"),
            column: id("id"),
        };
        assert!(apply(&m, no_pk).is_err());
        let key = PrimaryKey {
            name: None,
            columns: vec![id("id")],
            generated: None,
        };
        let m = apply(
            &m,
            Op::AddPrimaryKey {
                table: q("child"),
                key,
            },
        )
        .unwrap();
        let m = apply(
            &m,
            Op::AddIdentity {
                table: q("child"),
                column: id("id"),
            },
        )
        .unwrap();
        assert_eq!(
            child(&m).primary_key.as_ref().unwrap().generated,
            Some(Generated::Identity)
        );
        let m = apply(
            &m,
            Op::DropIdentity {
                table: q("child"),
                column: id("id"),
            },
        )
        .unwrap();
        assert_eq!(child(&m).primary_key.as_ref().unwrap().generated, None);
    }

    #[test]
    fn foreign_keys_add_and_drop() {
        let fk = child(&base()).foreign_keys[0].clone();
        let drop_by_canyon_name = Op::DropForeignKey {
            table: q("child"),
            name: fk_name(&q("child"), &fk.columns),
        };
        let m = apply(&base(), drop_by_canyon_name.clone()).unwrap();
        assert!(child(&m).foreign_keys.is_empty());
        assert!(apply(&m, drop_by_canyon_name).is_err());
        let m = apply(
            &m,
            Op::AddForeignKey {
                table: q("child"),
                key: fk,
            },
        )
        .unwrap();
        assert_eq!(child(&m).foreign_keys.len(), 1);
    }

    #[test]
    fn unknown_table_names_the_op() {
        let err = apply(&base(), Op::DropTable(q("nope"))).unwrap_err();
        assert!(err.contains("drop table nope"), "{err}");
    }

    #[test]
    fn satisfies_reports_table_and_column() {
        let a = base();
        assert!(satisfies(&a, &a).is_ok());
        let mut want = a.clone();
        want.tables
            .get_mut(&q("child"))
            .unwrap()
            .columns
            .iter_mut()
            .find(|c| c.name.as_str() == "pid")
            .unwrap()
            .nullable = true;
        let err = satisfies(&a, &want).unwrap_err();
        assert!(err.contains("child.pid"), "{err}");
        let err = satisfies(&SchemaModel::default(), &a).unwrap_err();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn a_table_op_given_as_a_column_op_is_an_error() {
        let mut t = base().tables.into_values().next().unwrap();
        let err = apply_to_table(&mut t, &Op::DropTable(q("parent"))).unwrap_err();
        assert_eq!(err, "DropTable isn't a column op");
    }

    #[test]
    fn a_type_change_restates_the_column() {
        let mut t = TableDef::builder("E", "x")
            .column("c", SqlType::Text { max_len: None }, true)
            .build()
            .unwrap();
        t.columns[0].extra.collation = Some(crate::model::CatalogSql::new("latin1_bin"));
        let m = model(vec![t]);
        let out = apply(
            &m,
            Op::AlterColumnType {
                table: q("x"),
                column: col("c", SqlType::Int, true),
            },
        )
        .unwrap();
        let c = &out.table(&q("x")).unwrap().columns[0];
        assert_eq!(c.ty, Logical::Known(SqlType::Int));
        assert_eq!(c.extra.collation, None);
    }
}
