//! What a run or a plan tells the user.

use crate::op::Statement;
use crate::planning::{Conflict, ConflictReport, Info, Warning};
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RenderedOp {
    pub phase: u8,
    pub description: String,
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone)]
pub struct DatasourcePlan {
    pub datasource: String,
    pub outcome: Result<Vec<RenderedOp>, Vec<Conflict>>,
    pub infos: Vec<Info>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Default)]
pub struct PlanReport {
    pub datasources: Vec<DatasourcePlan>,
}

impl fmt::Display for PlanReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, ds) in self.datasources.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            match &ds.outcome {
                Err(conflicts) => write!(
                    f,
                    "{}",
                    ConflictReport {
                        datasource: &ds.datasource,
                        conflicts
                    }
                )?,
                Ok(ops) if ops.is_empty() => writeln!(
                    f,
                    "Plan for datasource `{}`: nothing to change.",
                    ds.datasource
                )?,
                Ok(ops) => {
                    writeln!(
                        f,
                        "Plan for datasource `{}` (nothing is applied):\n",
                        ds.datasource
                    )?;
                    for op in ops {
                        writeln!(f, "  phase {:<2} {}", op.phase, op.description)?;
                        for st in &op.statements {
                            writeln!(f, "           {}", st.sql)?;
                        }
                    }
                }
            }
            write_notes(f, &ds.warnings, &ds.infos)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct DatasourceReport {
    pub datasource: String,
    pub run_id: String,
    pub statements: usize,
    pub duration: Duration,
    pub infos: Vec<Info>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub datasources: Vec<DatasourceReport>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for ds in &self.datasources {
            if ds.statements == 0 {
                writeln!(
                    f,
                    "Migrations for datasource `{}`: nothing to change.",
                    ds.datasource
                )?;
            } else {
                writeln!(
                    f,
                    "Migrations for datasource `{}`: {} statement{} applied in {} ms (run {}).",
                    ds.datasource,
                    ds.statements,
                    if ds.statements == 1 { "" } else { "s" },
                    ds.duration.as_millis(),
                    ds.run_id
                )?;
            }
            write_notes(f, &ds.warnings, &ds.infos)?;
        }
        Ok(())
    }
}

fn write_notes(f: &mut fmt::Formatter<'_>, warnings: &[Warning], infos: &[Info]) -> fmt::Result {
    let warnings = warnings.iter().map(ToString::to_string);
    for line in warnings.chain(infos.iter().map(ToString::to_string)) {
        writeln!(f, "  {line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Logical;
    use crate::planning::test_support::{id, q};
    use canyon_core::types::SqlType;

    fn tighten() -> Info {
        Info::Tightenable {
            table: q("league"),
            column: id("region"),
            column_type: Logical::Known(SqlType::Text { max_len: None }),
            canonical: SqlType::Text { max_len: Some(50) },
        }
    }

    #[test]
    fn run_report() {
        let report = Report {
            datasources: vec![
                DatasourceReport {
                    datasource: "postgres_docker".into(),
                    run_id: "18f3a2c1-9b2e4d10".into(),
                    statements: 3,
                    duration: Duration::from_millis(42),
                    infos: vec![tighten()],
                    warnings: vec![],
                },
                DatasourceReport {
                    datasource: "sqlserver_docker".into(),
                    run_id: "18f3a2c1-9b2e4d11".into(),
                    statements: 0,
                    duration: Duration::from_millis(3),
                    infos: vec![],
                    warnings: vec![],
                },
            ],
        };
        insta::assert_snapshot!(report.to_string());
    }

    #[test]
    fn plan_report() {
        let st = |sql: &str| Statement {
            sql: sql.into(),
            params: vec![],
            transactional: true,
        };
        let report = PlanReport {
            datasources: vec![
                DatasourcePlan {
                    datasource: "postgres_docker".into(),
                    outcome: Ok(vec![
                        RenderedOp {
                            phase: 3,
                            description: "create table player".into(),
                            statements: vec![st(
                                "CREATE TABLE \"player\" (\"id\" integer GENERATED ALWAYS AS IDENTITY NOT NULL)",
                            )],
                        },
                        RenderedOp {
                            phase: 11,
                            description: "add foreign key player (league) to league".into(),
                            statements: vec![st(
                                "ALTER TABLE \"player\" ADD CONSTRAINT \"fk\" FOREIGN KEY (\"league\") REFERENCES \"league\" (\"id\")",
                            )],
                        },
                    ]),
                    infos: vec![tighten()],
                    warnings: vec![],
                },
                DatasourcePlan {
                    datasource: "mysql_docker".into(),
                    outcome: Ok(vec![]),
                    infos: vec![],
                    warnings: vec![],
                },
                DatasourcePlan {
                    datasource: "sqlserver_docker".into(),
                    outcome: Err(vec![Conflict::RenameCycle {
                        tables: vec![q("gamer"), q("player")],
                    }]),
                    infos: vec![],
                    warnings: vec![],
                },
            ],
        };
        insta::assert_snapshot!(report.to_string());
    }
}
