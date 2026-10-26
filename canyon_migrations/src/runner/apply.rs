//! Running a plan: one transaction where the backend's DDL allows it (PostgreSQL, SQL
//! Server), statement by statement elsewhere (MySQL), a history row for each statement
//! and the snapshot last.

use super::render::render_ops;
use super::{Staged, quiet_release};
use crate::dialect::DdlDialect;
use crate::error::{MigrationError, chain, history_error};
use crate::memory::{HISTORY_TABLE, HistoryRow, SnapshotDoc, insert_history, insert_snapshot};
use crate::op::{Op, Statement};
use crate::report::DatasourceReport;
use canyon_core::connection::contracts::DbConnection;
use canyon_core::error::CanyonError;
use chrono::NaiveDateTime;
use std::time::Instant;

/// How [`Staged::apply_with`] runs. Only for tests.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Fail on purpose once this many statements ran.
    pub fail_after: Option<usize>,
}

/// A statement every backend refuses (the function doesn't exist).
const FORCED_FAILURE: &str = "SELECT canyon_forced_failure()";

impl Staged {
    /// Applies the plan and stores the new snapshot, then gives back the lock and the
    /// session, whatever happened.
    ///
    /// An empty plan writes nothing, unless the model or the declined offers differ from
    /// the newest snapshot: then only a new snapshot.
    ///
    /// # Errors
    /// The plan has conflicts, can't be written, or a statement fails. On PostgreSQL and
    /// SQL Server everything is rolled back; on MySQL the statements before the failing
    /// one stay, with their history rows, and the next run continues from there.
    pub async fn apply<D: DdlDialect>(
        self,
        run_id: &str,
    ) -> Result<DatasourceReport, MigrationError> {
        self.apply_with::<D>(run_id, &ApplyOptions::default()).await
    }

    #[doc(hidden)]
    pub async fn apply_with<D: DdlDialect>(
        self,
        run_id: &str,
        options: &ApplyOptions,
    ) -> Result<DatasourceReport, MigrationError> {
        let applied = self.run::<D>(run_id, options).await;
        // The work is done or reported: a failed unlock only gets logged, the server
        // frees the lock with the connection anyway
        quiet_release(self.session, self.unlock, &self.datasource).await;
        applied
    }

    async fn run<D: DdlDialect>(
        &self,
        run_id: &str,
        options: &ApplyOptions,
    ) -> Result<DatasourceReport, MigrationError> {
        let ops = match &self.outcome {
            Ok(ops) => ops,
            Err(conflicts) => {
                return Err(MigrationError::Conflicts(vec![(
                    self.datasource.clone(),
                    conflicts.clone(),
                )]));
            }
        };
        let started = Instant::now();
        let doc = SnapshotDoc::new(self.desired.clone(), self.declined.clone());
        if ops.is_empty() && self.previous.as_ref() == Some(&doc) {
            return Ok(self.report(run_id, 0, started));
        }
        // Built before anything runs: a snapshot that can't be stored stops the run here
        let snapshot =
            insert_snapshot::<D>(run_id, &doc).map_err(|source| MigrationError::Snapshot {
                datasource: self.datasource.clone(),
                source,
            })?;
        let rendered = render_ops::<D>(ops, &self.catalog, &self.datasource)?;

        let mut run = Run {
            staged: self,
            run_id,
            transactional: self.session.transactional_ddl(),
            history_ready: self.has_history_table(),
            pending: Vec::new(),
        };
        if run.transactional {
            self.session
                .begin()
                .await
                .map_err(|source| self.database("starting the transaction", source))?;
        }
        let mut count = 0;
        for (op, rendered) in ops.iter().zip(&rendered) {
            for st in &rendered.statements {
                if options.fail_after == Some(count) {
                    run.forced_failure::<D>().await?;
                }
                debug_assert!(
                    st.transactional || !run.transactional,
                    "a statement that can't run in the plan's transaction"
                );
                run.execute::<D>(&rendered.description, st).await?;
                count += 1;
                // A history row right after its statement, once the table exists
                run.flush().await?;
            }
            if matches!(&op.op, Op::CreateTable(t) if t.name.schema.is_none() && t.name.name.as_str() == HISTORY_TABLE)
            {
                run.history_ready = true;
            }
            run.flush().await?;
        }
        if options.fail_after == Some(count) {
            run.forced_failure::<D>().await?;
        }
        run.write(
            "save the schema snapshot",
            &snapshot,
            "saving the schema snapshot",
        )
        .await?;
        if run.transactional {
            self.session
                .commit()
                .await
                .map_err(|source| MigrationError::Commit {
                    datasource: self.datasource.clone(),
                    source: Box::new(source),
                })?;
        }
        tracing::info!(
            datasource = %self.datasource,
            run_id,
            statements = count,
            duration_ms = started.elapsed().as_millis() as u64,
            "migrations applied"
        );
        Ok(self.report(run_id, count, started))
    }

    fn has_history_table(&self) -> bool {
        self.catalog
            .canyon_tables
            .iter()
            .any(|t| t == HISTORY_TABLE)
    }

    fn report(&self, run_id: &str, statements: usize, started: Instant) -> DatasourceReport {
        DatasourceReport {
            datasource: self.datasource.clone(),
            run_id: run_id.to_owned(),
            statements,
            duration: started.elapsed(),
            infos: self.infos.clone(),
            warnings: self.warnings.clone(),
        }
    }

    fn database(&self, stage: &'static str, source: CanyonError) -> MigrationError {
        super::database(&self.datasource, stage, source)
    }
}

/// One plan being applied.
struct Run<'a> {
    staged: &'a Staged,
    run_id: &'a str,
    transactional: bool,
    /// The history table exists (before the run, or created by it so far).
    history_ready: bool,
    /// History rows of statements that ran before their table existed.
    pending: Vec<Statement>,
}

impl Run<'_> {
    fn history<D: DdlDialect>(
        &self,
        op: &str,
        st: &Statement,
        started_at: NaiveDateTime,
        clock: Instant,
        error: Option<&str>,
    ) -> Statement {
        insert_history::<D>(&HistoryRow {
            run_id: self.run_id,
            datasource: &self.staged.datasource,
            started_at,
            duration_ms: i64::try_from(clock.elapsed().as_millis()).unwrap_or(i64::MAX),
            op,
            statement: &st.sql,
            status: if error.is_some() { "failed" } else { "ok" },
            error,
        })
    }

    /// Runs one statement of the plan and queues its history row.
    async fn execute<D: DdlDialect>(
        &mut self,
        op: &str,
        st: &Statement,
    ) -> Result<(), MigrationError> {
        let started_at = chrono::Utc::now().naive_utc();
        let clock = Instant::now();
        let session = &self.staged.session;
        if let Err(source) = session.execute(&st.sql, &st.query_params()).await {
            return Err(self.fail::<D>(op, st, started_at, clock, source).await);
        }
        tracing::debug!(
            datasource = %self.staged.datasource,
            run_id = self.run_id,
            op,
            duration_ms = clock.elapsed().as_millis() as u64,
            "statement applied"
        );
        let row = self.history::<D>(op, st, started_at, clock, None);
        self.pending.push(row);
        Ok(())
    }

    /// Writes the queued history rows, once their table exists.
    async fn flush(&mut self) -> Result<(), MigrationError> {
        if !self.history_ready {
            return Ok(());
        }
        for row in std::mem::take(&mut self.pending) {
            self.write(
                "record the migration history",
                &row,
                "writing the migration history",
            )
            .await?;
        }
        Ok(())
    }

    /// Runs a statement of Canyon's own (history, snapshot). A failure is reported like
    /// a failing statement of the plan: on MySQL what ran before it stays.
    async fn write(
        &self,
        op: &str,
        st: &Statement,
        stage: &'static str,
    ) -> Result<(), MigrationError> {
        let session = &self.staged.session;
        let Err(source) = session.execute(&st.sql, &st.query_params()).await else {
            return Ok(());
        };
        let rolled_back = self.transactional && session.rollback().await.is_ok();
        tracing::error!(datasource = %self.staged.datasource, run_id = self.run_id, op, stage, "migration write failed");
        tracing::debug!(datasource = %self.staged.datasource, run_id = self.run_id, op, error = %source, "migration write failed with");
        Err(MigrationError::Apply {
            datasource: self.staged.datasource.clone(),
            op: op.to_owned(),
            statement: st.sql.clone(),
            source: Box::new(source),
            transactional: self.transactional,
            rolled_back,
        })
    }

    async fn forced_failure<D: DdlDialect>(&mut self) -> Result<(), MigrationError> {
        let st = Statement {
            sql: FORCED_FAILURE.to_owned(),
            params: Vec::new(),
            transactional: true,
        };
        self.execute::<D>("forced failure", &st).await?;
        // It ran after all: leave it out of the history
        self.pending.pop();
        Ok(())
    }

    /// Stops at the first failing statement (on SQL Server, anything after it would run
    /// outside the rolled-back transaction), rolls back, then records the failure.
    async fn fail<D: DdlDialect>(
        &mut self,
        op: &str,
        st: &Statement,
        started_at: NaiveDateTime,
        clock: Instant,
        source: CanyonError,
    ) -> MigrationError {
        let session = &self.staged.session;
        let rolled_back = self.transactional && session.rollback().await.is_ok();
        // The server's text may quote row values: only at debug level and in the
        // returned error, the history keeps the error code
        let error = chain(&source);
        tracing::error!(
            datasource = %self.staged.datasource,
            run_id = self.run_id,
            op,
            "migration statement failed"
        );
        tracing::debug!(
            datasource = %self.staged.datasource,
            run_id = self.run_id,
            op,
            %error,
            "migration statement failed with"
        );
        // After a rollback only a history table that was there before the run is left;
        // on MySQL the statements that ran stay, and their rows go in first
        let can_record = if self.transactional {
            self.pending.clear();
            self.staged.has_history_table()
        } else {
            self.history_ready
        };
        if can_record {
            let code = history_error(&source);
            let failed = self.history::<D>(op, st, started_at, clock, Some(&code));
            for row in std::mem::take(&mut self.pending).iter().chain([&failed]) {
                if let Err(e) = session.execute(&row.sql, &row.query_params()).await {
                    tracing::warn!(datasource = %self.staged.datasource, run_id = self.run_id, "recording the migration history failed");
                    tracing::debug!(datasource = %self.staged.datasource, run_id = self.run_id, error = %e, "recording the migration history failed with");
                    break;
                }
            }
        }
        MigrationError::Apply {
            datasource: self.staged.datasource.clone(),
            op: op.to_owned(),
            statement: st.sql.clone(),
            source: Box::new(source),
            transactional: self.transactional,
            rolled_back,
        }
    }
}
