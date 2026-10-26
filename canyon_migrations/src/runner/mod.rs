//! One datasource's run: take the session and the lock, read the database, plan, and
//! later render and apply the plan (`apply.rs`).
//!
//! Everything up to the plan is read-only, so a run with conflicts on any datasource
//! changes nothing anywhere. The lock is taken before reading and given back on every
//! path: by [`Staged::apply`] or [`Staged::release`], or by the server when the session's
//! connection goes away.

mod apply;
mod render;

pub use apply::ApplyOptions;
pub use canyon_core::connection::datasources::MigrationsConfig;

use crate::dialect::{DdlDialect, SNAPSHOT_TABLE};
use crate::error::MigrationError;
use crate::introspect::{Catalog, introspect};
use crate::memory::{
    Declined, LEGACY_TABLE, SnapshotDoc, canyon_tables, read_legacy, read_snapshot,
};
use crate::model::{Ident, Logical, SchemaModel, TableDef};
use crate::op::{Op, Statement};
use crate::planning::{
    Conflict, Decisions, Info, ModelIssue, PlannedOp, Precheck, PrecheckResults, Previous, Risk,
    Warning, finalize, phase, prepare, previous_from_database,
};
use canyon_core::connection::contracts::DbConnection;
use canyon_core::connection::database_type::DatabaseType;
use canyon_core::connection::db_connector::DatabaseConnector;
use canyon_core::connection::session::Session;
use canyon_core::error::CanyonError;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// How long to wait between two tries for the migration lock.
const LOCK_RETRY: Duration = Duration::from_millis(500);

/// One datasource, read and planned, still holding its session (and the migration lock,
/// if it was asked for). Consume it with [`Staged::apply`] or [`Staged::release`].
pub struct Staged {
    pub(crate) session: Session,
    /// What gives the lock back; `None` when no lock was taken.
    pub(crate) unlock: Option<Statement>,
    pub(crate) datasource: String,
    /// The database as it was read: rendering starts from it.
    pub(crate) catalog: Catalog,
    /// The model to store in the snapshot (default schema as `None`).
    pub(crate) desired: SchemaModel,
    /// The newest snapshot, `None` on a first run.
    pub(crate) previous: Option<SnapshotDoc>,
    /// Tighten offers the user said no to, written to the next snapshot.
    pub(crate) declined: Vec<Declined>,
    pub(crate) outcome: Result<Vec<PlannedOp>, Vec<Conflict>>,
    pub(crate) infos: Vec<Info>,
    pub(crate) warnings: Vec<Warning>,
}

/// Opens a session on `conn`, takes the migration lock when `lock` is set (waiting up to
/// `settings.lock_timeout_secs`), reads the database and plans `desired` against it.
/// Nothing is written. Conflicts don't make it fail: they are in [`Staged::conflicts`].
///
/// # Errors
/// The lock isn't free in time, or reading the database or the snapshot fails. The lock
/// and the session are given back first.
pub async fn prepare_one<D: DdlDialect>(
    conn: &DatabaseConnector,
    datasource: &str,
    desired: &[TableDef],
    settings: &MigrationsConfig,
    decisions: &Decisions,
    lock: bool,
) -> Result<Staged, MigrationError> {
    let session = conn
        .acquire()
        .await
        .map_err(|source| database(datasource, "connecting", source))?;
    let unlock = if lock {
        if let Err(error) = take_lock::<D>(&session, datasource, settings.lock_timeout_secs).await {
            quiet_release(session, None, datasource).await;
            return Err(error);
        }
        Some(D::unlock())
    } else {
        None
    };
    match plan::<D>(&session, datasource, desired, decisions).await {
        Ok(planned) => Ok(Staged {
            session,
            unlock,
            datasource: datasource.to_owned(),
            catalog: planned.catalog,
            desired: planned.desired,
            previous: planned.previous,
            declined: planned.declined,
            outcome: planned.outcome,
            infos: planned.infos,
            warnings: planned.warnings,
        }),
        Err(error) => {
            quiet_release(session, unlock, datasource).await;
            Err(error)
        }
    }
}

impl Staged {
    pub fn datasource(&self) -> &str {
        &self.datasource
    }

    pub fn db_type(&self) -> DatabaseType {
        self.session.db_type()
    }

    /// Everything that stops this datasource's plan; empty when it can be applied.
    pub fn conflicts(&self) -> &[Conflict] {
        match &self.outcome {
            Ok(_) => &[],
            Err(conflicts) => conflicts,
        }
    }

    pub fn infos(&self) -> &[Info] {
        &self.infos
    }

    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Gives the lock back and closes the session, without applying anything.
    ///
    /// # Errors
    /// Unlocking or closing fails. The server frees the lock when the connection
    /// closes anyway.
    pub async fn release(self) -> Result<(), MigrationError> {
        release(self.session, self.unlock, &self.datasource).await
    }
}

struct Planned {
    catalog: Catalog,
    desired: SchemaModel,
    previous: Option<SnapshotDoc>,
    declined: Vec<Declined>,
    outcome: Result<Vec<PlannedOp>, Vec<Conflict>>,
    infos: Vec<Info>,
    warnings: Vec<Warning>,
}

/// Read the database and the snapshot, run the prechecks, plan.
async fn plan<D: DdlDialect>(
    session: &Session,
    datasource: &str,
    desired: &[TableDef],
    decisions: &Decisions,
) -> Result<Planned, MigrationError> {
    let desired = SchemaModel::from_tables(desired.to_vec());
    let mut schemas = match &desired {
        Ok(model) => schemas_of(model),
        Err(_) => Vec::new(),
    };
    let mut catalog = read(session, datasource, &schemas).await?;
    let previous = if catalog.canyon_tables.iter().any(|t| t == SNAPSHOT_TABLE) {
        let json = read_snapshot::<D>(session)
            .await
            .map_err(|source| database(datasource, "reading the schema snapshot", source))?;
        json.map(|json| SnapshotDoc::from_json(&json))
            .transpose()
            .map_err(|source| MigrationError::Snapshot {
                datasource: datasource.to_owned(),
                source,
            })?
    } else {
        None
    };
    // A table may have left a schema the entities no longer use: read those too
    if let Some(doc) = &previous {
        let more: Vec<Ident> = (schemas_of(&doc.tables).into_iter())
            .filter(|s| !schemas.contains(s))
            .collect();
        if !more.is_empty() {
            schemas.extend(more);
            schemas.sort();
            catalog = read(session, datasource, &schemas).await?;
        }
    }
    let previous_model = match &previous {
        Some(doc) => Previous::Snapshot(doc.tables.clone()),
        None => {
            let legacy = if catalog.canyon_tables.iter().any(|t| t == LEGACY_TABLE) {
                read_legacy(session)
                    .await
                    .map_err(|source| database(datasource, "reading `canyon_memory`", source))?
            } else {
                Vec::new()
            };
            previous_from_database(&catalog.model, &legacy)
        }
    };

    let desired = match desired {
        Ok(model) => model,
        Err(e) => {
            return Ok(Planned {
                catalog,
                desired: SchemaModel::default(),
                previous,
                declined: Vec::new(),
                outcome: Err(vec![Conflict::InvalidModel(ModelIssue::Model(e))]),
                infos: Vec::new(),
                warnings: Vec::new(),
            });
        }
    };
    let prepared = prepare::<D>(
        &desired,
        &catalog.model,
        &previous_model,
        decisions,
        &catalog.default_schema,
        &catalog.existing_schemas,
    );
    let results = run_prechecks::<D>(
        session,
        datasource,
        &prepared.prechecks,
        &catalog.default_schema,
    )
    .await?;
    // `finalize` only gives them back with a plan
    let (diff_infos, diff_warnings) = (prepared.diff.infos.clone(), prepared.diff.warnings.clone());
    let declined = carried_declines(previous.as_ref(), &prepared.desired);
    let (outcome, infos, warnings) = match finalize::<D>(
        prepared.diff,
        &results,
        &prepared.decisions,
        prepared.conflicts,
    ) {
        Ok(plan) => {
            let mut ops = plan.ops;
            let outcome = add_canyon_tables(&mut ops, &catalog.canyon_tables).map(|()| ops);
            (outcome, plan.infos, plan.warnings)
        }
        Err(conflicts) => (Err(conflicts), diff_infos, diff_warnings),
    };
    Ok(Planned {
        catalog,
        desired: prepared.desired,
        previous,
        declined,
        outcome,
        infos,
        warnings,
    })
}

async fn read(
    session: &Session,
    datasource: &str,
    schemas: &[Ident],
) -> Result<Catalog, MigrationError> {
    introspect(session, session.db_type(), schemas)
        .await
        .map_err(|source| MigrationError::Introspection {
            datasource: datasource.to_owned(),
            source: Box::new(source),
        })
}

/// The schemas a model's tables and foreign keys name, sorted.
fn schemas_of(model: &SchemaModel) -> Vec<Ident> {
    let mut found: Vec<Ident> = (model.tables.values())
        .flat_map(|t| {
            let targets = t.foreign_keys.iter().map(|fk| &fk.ref_table);
            std::iter::once(&t.name).chain(targets)
        })
        .filter_map(|n| n.schema.clone())
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The previous snapshot's declines whose column is still in the model, with the type
/// that was declined: a field changed since gets asked again.
fn carried_declines(previous: Option<&SnapshotDoc>, desired: &SchemaModel) -> Vec<Declined> {
    let Some(doc) = previous else {
        return Vec::new();
    };
    (doc.declined_tighten.iter())
        .filter(|d| {
            desired.table(&d.table).is_some_and(|t| {
                (t.columns.iter())
                    .any(|c| c.name == d.column && c.ty == Logical::Known(d.column_type))
            })
        })
        .cloned()
        .collect()
}

/// Canyon's tables the database lacks are created with the plan (phase 3, the phase
/// of `CreateTable`), but before every other op: no statement runs before the history
/// table that records it exists. They are never in the desired model, or every run
/// would see them as new.
fn add_canyon_tables(ops: &mut Vec<PlannedOp>, existing: &[String]) -> Result<(), Vec<Conflict>> {
    let tables = canyon_tables()
        .map_err(|e| vec![Conflict::Internal(format!("Canyon's own tables: {e}"))])?;
    let missing = (tables.into_iter())
        .filter(|t| !existing.iter().any(|e| e == t.name.name.as_str()))
        .map(|t| {
            let op = Op::CreateTable(t);
            PlannedOp {
                phase: phase(&op),
                op,
                risk: Risk::Safe,
            }
        });
    ops.splice(0..0, missing);
    Ok(())
}

/// Every precheck, by the exact value the plan looks it up with.
async fn run_prechecks<D: DdlDialect>(
    session: &Session,
    datasource: &str,
    prechecks: &[Precheck],
    default_schema: &str,
) -> Result<PrecheckResults, MigrationError> {
    let mut found = BTreeMap::new();
    for p in prechecks {
        let st = D::precheck(p, default_schema);
        let n: i64 = session
            .query_one_for(&st.sql, &st.query_params())
            .await
            .map_err(|source| database(datasource, "checking the data", source))?;
        found.insert(p.clone(), u64::try_from(n).unwrap_or(0));
    }
    Ok(PrecheckResults(found))
}

/// Tries every [`LOCK_RETRY`] until `timeout_secs` have passed. Never takes the lock
/// twice: a try that answered 0 took nothing.
async fn take_lock<D: DdlDialect>(
    session: &Session,
    datasource: &str,
    timeout_secs: u64,
) -> Result<(), MigrationError> {
    let st = D::try_lock();
    let started = Instant::now();
    // `None`: too far away to count, so no limit
    let deadline = started.checked_add(Duration::from_secs(timeout_secs));
    loop {
        let taken: i64 = session
            .query_one_for(&st.sql, &st.query_params())
            .await
            .map_err(|source| database(datasource, "taking the migration lock", source))?;
        if taken == 1 {
            return Ok(());
        }
        let wait = match deadline {
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => left.min(LOCK_RETRY),
                _ => {
                    return Err(MigrationError::LockTimeout {
                        datasource: datasource.to_owned(),
                        waited_secs: started.elapsed().as_secs(),
                    });
                }
            },
            None => LOCK_RETRY,
        };
        tokio::time::sleep(wait).await;
    }
}

/// Unlocks (only a lock this session took) and closes the session.
async fn release(
    session: Session,
    unlock: Option<Statement>,
    datasource: &str,
) -> Result<(), MigrationError> {
    // What `unlock` answers differs per backend: run it, never decode it
    let unlocked = match &unlock {
        Some(st) => session
            .query_rows(&st.sql, &st.query_params())
            .await
            .map(drop),
        None => Ok(()),
    };
    let closed = session.release().await;
    unlocked.map_err(|source| database(datasource, "releasing the migration lock", source))?;
    closed.map_err(|source| database(datasource, "closing the session", source))
}

/// [`release`] on a path that already has an error to return: this one is only logged.
async fn quiet_release(session: Session, unlock: Option<Statement>, datasource: &str) {
    if let Err(error) = release(session, unlock, datasource).await {
        tracing::warn!(datasource, %error, "releasing the migration session failed");
    }
}

fn database(datasource: &str, stage: &'static str, source: CanyonError) -> MigrationError {
    MigrationError::Database {
        datasource: datasource.to_owned(),
        stage,
        source: Box::new(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::fixtures::{id, q};
    use crate::model::Generated;
    use canyon_core::types::SqlType;

    #[test]
    fn canyon_tables_go_first() {
        let league = TableDef::builder("League", "league")
            .column("id", SqlType::Int, false)
            .primary_key(Some(Generated::Identity))
            .build()
            .unwrap();
        let planned = |op: Op| PlannedOp {
            phase: phase(&op),
            op,
            risk: Risk::Safe,
        };
        let mut ops = vec![
            planned(Op::CreateSchema { schema: id("s") }),
            planned(Op::CreateTable(league)),
        ];
        add_canyon_tables(&mut ops, &["canyon_schema_snapshot".to_owned()]).unwrap();
        let names: Vec<String> = (ops.iter())
            .map(|p| format!("{} {}", p.phase, p.op.describe()))
            .collect();
        assert_eq!(
            names,
            [
                "3 create table canyon_migration_history",
                "0 create schema s",
                "3 create table league"
            ]
        );
        let mut none = Vec::new();
        add_canyon_tables(
            &mut none,
            &["canyon_migration_history".into(), SNAPSHOT_TABLE.into()],
        )
        .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn schemas_come_from_tables_and_targets() {
        let target = TableDef::builder("U", "u")
            .schema("a")
            .column("id", SqlType::Int, false)
            .primary_key(None)
            .build()
            .unwrap();
        let t = TableDef::builder("T", "t")
            .schema("b")
            .column("id", SqlType::Int, false)
            .column("u", SqlType::Int, false)
            .foreign_key("a.u", "id")
            .build()
            .unwrap();
        let model = SchemaModel::from_tables(vec![t, target]).unwrap();
        assert_eq!(schemas_of(&model), [id("a"), id("b")]);
    }

    #[test]
    fn only_declines_of_unchanged_columns_carry_over() {
        let table = TableDef::builder("Player", "player")
            .column("id", SqlType::Int, false)
            .column("name", SqlType::Text { max_len: Some(10) }, false)
            .build()
            .unwrap();
        let desired = SchemaModel::from_tables(vec![table]).unwrap();
        let decline = |column: &str| Declined {
            table: q("player"),
            column: id(column),
            column_type: SqlType::Text { max_len: Some(10) },
        };
        let mut retyped = decline("name");
        retyped.column_type = SqlType::Text { max_len: Some(20) };
        let doc = SnapshotDoc::new(
            desired.clone(),
            vec![decline("name"), decline("gone"), retyped],
        );
        assert_eq!(carried_declines(Some(&doc), &desired), [decline("name")]);
        assert!(carried_declines(None, &desired).is_empty());
    }
}
