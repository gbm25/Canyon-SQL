//! The plan to SQL, one op at a time on the database as the ops before it leave it.

use super::Staged;
use crate::dialect::{DdlDialect, LowerError};
use crate::error::MigrationError;
use crate::introspect::Catalog;
use crate::op::Ctx;
use crate::planning::{PlannedOp, apply_in_memory};
use crate::report::RenderedOp;

impl Staged {
    /// The statements of the plan, in order.
    ///
    /// # Errors
    /// The plan has conflicts, or an op can't be written for this backend.
    pub fn render<D: DdlDialect>(&self) -> Result<Vec<RenderedOp>, MigrationError> {
        match &self.outcome {
            Ok(ops) => render_ops::<D>(ops, &self.catalog, &self.datasource),
            Err(conflicts) => Err(MigrationError::Conflicts(vec![(
                self.datasource.clone(),
                conflicts.clone(),
            )])),
        }
    }
}

/// Each op is lowered on the database as it is when the op runs: the read one with the
/// ops before it applied. A rename only renames the constraints still there, and a
/// backend that restates a column (SQL Server, MySQL) restates the type it has then.
pub(super) fn render_ops<D: DdlDialect>(
    ops: &[PlannedOp],
    catalog: &Catalog,
    datasource: &str,
) -> Result<Vec<RenderedOp>, MigrationError> {
    let lower_error = |source| MigrationError::Lower {
        datasource: datasource.to_owned(),
        source,
    };
    let mut state = catalog.model.clone();
    let mut out = Vec::with_capacity(ops.len());
    for planned in ops {
        let ctx = Ctx {
            server: catalog.server,
            default_schema: &catalog.default_schema,
            no_backslash_escapes: catalog.no_backslash_escapes,
            actual: &state,
        };
        let statements = D::lower(&planned.op, &ctx).map_err(lower_error)?;
        out.push(RenderedOp {
            phase: planned.phase,
            description: planned.op.describe(),
            statements,
        });
        state = apply_in_memory(&state, std::slice::from_ref(&planned.op))
            .map_err(|detail| lower_error(LowerError::MissingActual(detail)))?;
    }
    Ok(out)
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use super::*;
    use crate::dialect::fixtures::{id, q};
    use crate::dialect::fk_name;
    use crate::model::{SchemaModel, TableDef};
    use crate::op::{Op, ServerVersion};
    use crate::planning::{Risk, phase};
    use canyon_core::query::querybuilder::syntax::dialect::PgDialect;
    use canyon_core::types::SqlType;

    fn planned(op: Op) -> PlannedOp {
        PlannedOp {
            phase: phase(&op),
            op,
            risk: Risk::Safe,
        }
    }

    fn catalog(model: SchemaModel) -> Catalog {
        Catalog {
            server: ServerVersion {
                major: 14,
                minor: 0,
                patch: 0,
            },
            default_schema: "public".into(),
            existing_schemas: Vec::new(),
            no_backslash_escapes: false,
            canyon_tables: Vec::new(),
            model,
        }
    }

    // The key is dropped in phase 1: the rename in phase 2 must not rename it again
    #[test]
    fn a_dropped_key_is_not_renamed_with_its_table() {
        let mut player = TableDef::builder("Player", "player")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .column("league", SqlType::Int, false)
            .foreign_key("league", "id")
            .build()
            .unwrap();
        let fk = fk_name(&q("player"), &[id("league")]);
        player.foreign_keys[0].name = Some(fk.clone());
        let league = TableDef::builder("League", "league")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .build()
            .unwrap();
        let model = SchemaModel::from_tables(vec![player, league]).unwrap();
        let ops = [
            planned(Op::DropForeignKey {
                table: q("player"),
                name: fk.clone(),
            }),
            planned(Op::RenameTable {
                from: q("player"),
                to: id("gamer"),
            }),
        ];
        let rendered = render_ops::<PgDialect>(&ops, &catalog(model.clone()), "ds").unwrap();
        assert_eq!(rendered.len(), 2);
        let rename: Vec<&str> = rendered[1]
            .statements
            .iter()
            .map(|s| s.sql.as_str())
            .collect();
        assert!(
            rename.iter().all(|sql| !sql.contains(fk.as_str())),
            "{rename:?}"
        );
        // Lowered on the database as read, the rename would rename the dropped key too
        let alone = render_ops::<PgDialect>(&ops[1..], &catalog(model), "ds").unwrap();
        assert!(
            (alone[0].statements.iter()).any(|s| s.sql.contains(fk.as_str())),
            "the check above would prove nothing"
        );
    }

    #[test]
    fn an_op_that_does_not_fit_the_database_is_an_error() {
        let ops = [planned(Op::DropTable(q("nowhere")))];
        let err =
            render_ops::<PgDialect>(&ops, &catalog(SchemaModel::default()), "ds").unwrap_err();
        assert!(matches!(err, MigrationError::Lower { .. }), "{err:?}");
    }
}
