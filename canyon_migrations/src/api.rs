//! Migrations over every datasource at once: plan, run, or ask and run.
//!
//! Every datasource is read and planned before any of them is written. If one has a
//! conflict, none gets a statement: only the read-only checks ran. Each session (and its
//! lock) is given back on every path; a dropped future drops the sessions, and the
//! server frees their locks.
//!
//! Events through `tracing`:
//! - `info`: a datasource was applied (run id, statements, time).
//! - `error`: a statement or one of Canyon's own writes failed (datasource, run id, op).
//!   The database's message goes to `debug` only, it may quote row values.
//! - `warn`: giving back a session, or recording the history of a failure, failed.
//! - `debug`: each statement applied.

pub use crate::error::MigrationError;
pub use crate::interactive::{Answer, Prompt, Question, StdinPrompt};
pub use crate::report::{DatasourcePlan, DatasourceReport, PlanReport, RenderedOp, Report};
pub use crate::runner::MigrationsConfig;
#[doc(hidden)]
pub use crate::{planning::Decisions, runner::ApplyOptions};

use crate::interactive::{Flow, apply_answer, questions, still_open};
use crate::memory::{DATASOURCE_MAX_LEN, Declined, new_run_id};
use crate::model::TableDef;
use crate::planning::Conflict;
use crate::runner::{Staged, prepare_one};
use canyon_core::canyon::Canyon;
use canyon_core::connection::datasources::DatasourceConfig;
use canyon_core::connection::db_connector::DatabaseConnector;

/// Runs `$body` with `$D` set to the dialect of the backend `$db`.
macro_rules! with_dialect {
    ($db:expr, $D:ident => $body:expr) => {
        match $db {
            #[cfg(feature = "postgres")]
            canyon_core::connection::database_type::DatabaseType::PostgreSql => {
                type $D = canyon_core::query::querybuilder::syntax::dialect::PgDialect;
                $body
            }
            #[cfg(feature = "mssql")]
            canyon_core::connection::database_type::DatabaseType::SqlServer => {
                type $D = canyon_core::query::querybuilder::syntax::dialect::MsSql;
                $body
            }
            #[cfg(feature = "mysql")]
            canyon_core::connection::database_type::DatabaseType::MySQL => {
                type $D = canyon_core::query::querybuilder::syntax::dialect::MySql;
                $body
            }
        }
    };
}

/// Rounds of questions `interactive_models` asks while the plan still has conflicts
/// (new ones can show up when someone else migrates in between) before it gives up.
#[doc(hidden)]
pub const MAX_ROUNDS: usize = 5;

/// Answers to one question that can't be used before `interactive_models` stops.
#[doc(hidden)]
pub const MAX_TRIES: usize = 5;

/// Which database a datasource points at: backend, host (lowercase), effective port and
/// name. Its order is the order in which the locks are taken. The default value (unknown,
/// port 0) keeps the configured order.
///
/// Host names are compared as written: `localhost` and `127.0.0.1` count as two
/// databases, they are not resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DbIdentity {
    db_type: String,
    host: String,
    port: u16,
    db_name: String,
}

impl DbIdentity {
    /// The database `ds` points at. A datasource without `port` gets its backend's
    /// default port, the one it connects to.
    pub fn of(ds: &DatasourceConfig) -> Self {
        Self {
            db_type: format!("{:?}", ds.get_db_type()),
            host: ds.properties.host.to_lowercase(),
            port: ds.get_port_or_default_by_db(),
            db_name: ds.properties.db_name.clone(),
        }
    }
}

/// One datasource to migrate.
pub struct Target<'a> {
    pub datasource: &'a str,
    pub conn: &'a DatabaseConnector,
    pub settings: MigrationsConfig,
    /// The database behind `conn`, for the order of the locks ([`DbIdentity::of`]).
    pub identity: DbIdentity,
}

/// The datasources of `canyon.toml` with migrations enabled, in their order.
///
/// # Errors
/// [`MigrationError::Config`] for an enabled datasource with no name, a name longer than
/// the history table takes, no connection, or that points at the same database as an
/// earlier one (each would wait for the other's lock).
pub fn configured_targets(canyon: &'static Canyon) -> Result<Vec<Target<'static>>, MigrationError> {
    enabled_datasources(canyon.datasources())?
        .into_iter()
        .map(|(ds, settings, identity)| {
            let conn = canyon
                .get_connection(&ds.name)
                .map_err(|_| config_error(ds, "the datasource has no connection"))?;
            Ok(Target {
                datasource: &ds.name,
                conn,
                settings,
                identity,
            })
        })
        .collect()
}

/// The datasources with migrations enabled, with their settings and identity, after the
/// checks that need no connection.
fn enabled_datasources(
    datasources: &[DatasourceConfig],
) -> Result<Vec<(&DatasourceConfig, MigrationsConfig, DbIdentity)>, MigrationError> {
    let mut enabled: Vec<(&DatasourceConfig, MigrationsConfig, DbIdentity)> = Vec::new();
    for ds in datasources {
        let Some(settings) = ds.migrations_settings() else {
            continue;
        };
        // An empty name would get the default datasource's connection
        if ds.name.is_empty() {
            return Err(config_error(ds, "the datasource has no name"));
        }
        if ds.name.chars().count() > DATASOURCE_MAX_LEN {
            return Err(config_error(
                ds,
                &format!(
                    "the datasource name is longer than {DATASOURCE_MAX_LEN} characters, \
                     the migration history can't store it"
                ),
            ));
        }
        let identity = DbIdentity::of(ds);
        let seen = enabled.iter().map(|(d, _, i)| (d.name.as_str(), i));
        if let Some(first) = first_with(seen, &identity) {
            return Err(config_error(
                ds,
                &format!("points at the same database as `{first}`"),
            ));
        }
        enabled.push((ds, settings, identity));
    }
    Ok(enabled)
}

fn config_error(ds: &DatasourceConfig, detail: &str) -> MigrationError {
    MigrationError::Config {
        datasource: ds.name.clone(),
        detail: detail.to_owned(),
    }
}

fn first_with<'n, 'i>(
    mut seen: impl Iterator<Item = (&'n str, &'i DbIdentity)>,
    identity: &DbIdentity,
) -> Option<&'n str> {
    seen.find(|(_, i)| *i == identity).map(|(name, _)| name)
}

/// Target indices in the order to take their locks: by database identity, so two runs
/// over the same databases never wait on each other in a circle. Equal (or unknown)
/// identities keep the configured order.
fn lock_order(targets: &[Target<'_>]) -> Vec<usize> {
    order_of(&targets.iter().map(|t| &t.identity).collect::<Vec<_>>())
}

fn order_of(identities: &[&DbIdentity]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..identities.len()).collect();
    order.sort_by(|&a, &b| identities[a].cmp(identities[b]));
    order
}

/// What the migrations would do on each datasource, without the lock and without
/// writing anything. Conflicts are in each [`DatasourcePlan::outcome`], not an error.
///
/// # Errors
/// A database can't be read, or the plan can't be written as SQL.
pub async fn plan_models(
    targets: &[Target<'_>],
    desired: &[TableDef],
) -> Result<PlanReport, MigrationError> {
    let mut datasources = Vec::with_capacity(targets.len());
    for t in targets {
        let staged = stage(t, desired, &config_decisions(&t.settings), false).await?;
        let outcome = match staged.conflicts() {
            [] => with_dialect!(staged.db_type(), D => staged.render::<D>()).map(Ok),
            conflicts => Ok(Err(conflicts.to_vec())),
        };
        let plan = outcome.map(|outcome| DatasourcePlan {
            datasource: staged.datasource().to_owned(),
            outcome,
            infos: staged.infos().to_vec(),
            warnings: staged.warnings().to_vec(),
        });
        datasources.push(finish(staged, plan).await?);
    }
    Ok(PlanReport { datasources })
}

/// Applies the migrations on every datasource, with the decisions `canyon.toml` gives.
///
/// # Errors
/// [`MigrationError::Conflicts`] with every datasource's conflicts, and then nothing
/// was written anywhere. Any other error stops the run; datasources applied before it
/// stay applied ([`MigrationError::PartlyApplied`]).
pub async fn run_models(
    targets: &[Target<'_>],
    desired: &[TableDef],
) -> Result<Report, MigrationError> {
    run_models_with(targets, desired, None, &ApplyOptions::default()).await
}

/// [`run_models`] with the same `decisions` for every datasource instead of the ones
/// from `canyon.toml`, and test options for each apply.
#[doc(hidden)]
pub async fn run_models_with(
    targets: &[Target<'_>],
    desired: &[TableDef],
    decisions: Option<&Decisions>,
    options: &ApplyOptions,
) -> Result<Report, MigrationError> {
    let decisions: Vec<Decisions> = (targets.iter())
        .map(|t| {
            decisions
                .cloned()
                .unwrap_or_else(|| config_decisions(&t.settings))
        })
        .collect();
    let staged = stage_all(targets, desired, &decisions, true).await?;
    let conflicts = conflicts_of(&staged);
    if !conflicts.is_empty() {
        release_all(staged).await;
        return Err(MigrationError::Conflicts(conflicts));
    }
    apply_all(staged, options).await
}

/// Plans without the lock, asks `prompt` about the conflicts and the tighten offers,
/// then takes the lock, reads again and plans with the answers. Conflicts that show up
/// then (someone else migrated) are asked about too, up to [`MAX_ROUNDS`]; with none
/// left, everything is applied.
///
/// Tighten offers that show up only under the lock, or that a rename round left unasked,
/// are asked too (the sessions are released for it, then the plan is made again).
///
/// The prompt is called on a blocking thread, so it may read the terminal. Nothing is
/// held on the databases while it waits for an answer.
///
/// A blocking read of stdin can't be interrupted: if this future is dropped while a
/// question waits (a Ctrl-C handler, a timeout), the read goes on until a line comes in,
/// and dropping the runtime waits for it. Shut the runtime down with
/// `Runtime::shutdown_timeout` or `Runtime::shutdown_background` in that case.
///
/// # Errors
/// [`MigrationError::Conflicts`] when a conflict has no answer that fixes it, an op
/// isn't confirmed, the user stops, or the rounds run out; nothing was written then.
/// [`MigrationError::Aborted`] when the user stops with no conflict left. Otherwise as
/// [`run_models`].
pub async fn interactive_models(
    targets: &[Target<'_>],
    desired: &[TableDef],
    prompt: Box<dyn Prompt + Send>,
) -> Result<Report, MigrationError> {
    let mut decisions: Vec<Decisions> = (targets.iter())
        .map(|t| config_decisions(&t.settings))
        .collect();
    let mut declined: Vec<Vec<Declined>> = vec![Vec::new(); targets.len()];
    let mut prompt = Some(prompt);
    let mut staged = stage_all(targets, desired, &decisions, false).await?;
    let mut locked = false;
    let mut rounds = 0;
    loop {
        let conflicts = conflicts_of(&staged);
        let asked = match collect_questions(&staged, &decisions, &declined) {
            Ok(asked) => asked,
            Err(()) => {
                release_all(staged).await;
                return Err(MigrationError::Conflicts(conflicts));
            }
        };
        // Offers nobody was asked about yet (a rename round can clear every conflict
        // before they were reached) are asked before the apply
        if locked && conflicts.is_empty() && asked.is_empty() {
            for (s, new) in staged.iter_mut().zip(&declined) {
                for d in new {
                    if !s.declined.contains(d) {
                        s.declined.push(d.clone());
                    }
                }
            }
            return apply_all(staged, &ApplyOptions::default()).await;
        }
        release_all(staged).await;
        if !conflicts.is_empty() && rounds == MAX_ROUNDS {
            return Err(MigrationError::Conflicts(conflicts));
        }
        let stopped = |conflicts: Vec<(String, Vec<Conflict>)>| {
            if conflicts.is_empty() {
                MigrationError::Aborted
            } else {
                MigrationError::Conflicts(conflicts)
            }
        };
        match ask_all(asked, &mut prompt, &mut decisions, &mut declined).await {
            Asked::Progress => {}
            // Asking again would get the same plan
            Asked::Nothing if !conflicts.is_empty() => {
                return Err(MigrationError::Conflicts(conflicts));
            }
            Asked::Nothing => {}
            Asked::Stop => return Err(stopped(conflicts)),
        }
        if !conflicts.is_empty() {
            rounds += 1;
        }
        staged = stage_all(targets, desired, &decisions, true).await?;
        locked = true;
    }
}

fn config_decisions(settings: &MigrationsConfig) -> Decisions {
    Decisions::from_config(settings.allow_destructive, settings.rebuild_confirm_rows)
}

async fn stage(
    t: &Target<'_>,
    desired: &[TableDef],
    decisions: &Decisions,
    lock: bool,
) -> Result<Staged, MigrationError> {
    with_dialect!(t.conn.get_db_type(), D => {
        prepare_one::<D>(t.conn, t.datasource, desired, &t.settings, decisions, lock).await
    })
}

/// Stages every target, one after the other in [`lock_order`] (a lock is held while the
/// next one is waited for, so the order must not depend on the configuration). The result
/// is in the configured order. On an error, the ones staged so far are released.
async fn stage_all(
    targets: &[Target<'_>],
    desired: &[TableDef],
    decisions: &[Decisions],
    lock: bool,
) -> Result<Vec<Staged>, MigrationError> {
    let mut slots: Vec<Option<Staged>> = (0..targets.len()).map(|_| None).collect();
    for i in lock_order(targets) {
        match stage(&targets[i], desired, &decisions[i], lock).await {
            Ok(s) => slots[i] = Some(s),
            Err(error) => {
                release_all(slots.into_iter().flatten().collect()).await;
                return Err(error);
            }
        }
    }
    Ok(slots.into_iter().flatten().collect())
}

fn conflicts_of(staged: &[Staged]) -> Vec<(String, Vec<Conflict>)> {
    (staged.iter())
        .filter(|s| !s.conflicts().is_empty())
        .map(|s| (s.datasource().to_owned(), s.conflicts().to_vec()))
        .collect()
}

/// Gives back every session and lock. A path that releases these already has its
/// outcome, so a failed release is only logged (the server frees the lock anyway).
async fn release_all(staged: Vec<Staged>) {
    for s in staged {
        let datasource = s.datasource().to_owned();
        if let Err(error) = s.release().await {
            tracing::warn!(datasource, %error, "releasing the migration session failed");
        }
    }
}

/// Releases `staged`, then gives `result`; a release error only counts when `result`
/// is fine.
async fn finish<T>(staged: Staged, result: Result<T, MigrationError>) -> Result<T, MigrationError> {
    match result {
        Ok(value) => staged.release().await.map(|()| value),
        Err(error) => {
            release_all(vec![staged]).await;
            Err(error)
        }
    }
}

/// Applies each datasource in turn under one run id. Stops at the first failure and
/// releases the ones not applied yet.
async fn apply_all(staged: Vec<Staged>, options: &ApplyOptions) -> Result<Report, MigrationError> {
    let run_id = new_run_id();
    let mut datasources = Vec::with_capacity(staged.len());
    let mut rest = staged.into_iter();
    while let Some(s) = rest.next() {
        // `apply_with` gives back its own session, also when it fails
        let applied = with_dialect!(s.db_type(), D => s.apply_with::<D>(&run_id, options).await);
        match applied {
            Ok(report) => datasources.push(report),
            Err(error) => {
                let not_attempted: Vec<Staged> = rest.collect();
                let names = (not_attempted.iter())
                    .map(|s| s.datasource().to_owned())
                    .collect();
                release_all(not_attempted).await;
                return Err(if datasources.is_empty() {
                    error
                } else {
                    MigrationError::PartlyApplied {
                        applied: datasources,
                        not_attempted: names,
                        error: Box::new(error),
                    }
                });
            }
        }
    }
    Ok(Report { datasources })
}

/// Every datasource's questions, by datasource index; only the rename ones when there
/// are any. `Err` when any datasource has a conflict no answer fixes: then nothing is
/// asked anywhere.
fn collect_questions(
    staged: &[Staged],
    decisions: &[Decisions],
    declined: &[Vec<Declined>],
) -> Result<Vec<(usize, Question)>, ()> {
    let mut out = Vec::new();
    let mut unfixable = false;
    for (i, s) in staged.iter().enumerate() {
        let mut asked_before = s.declined.clone();
        asked_before.extend(declined[i].iter().cloned());
        match questions(s.datasource(), s.conflicts(), s.infos(), &asked_before) {
            Ok(qs) => out.extend(
                (qs.into_iter())
                    .filter(|q| !already_tightened(q, &decisions[i]))
                    .map(|q| (i, q)),
            ),
            Err(_) => unfixable = true,
        }
    }
    if unfixable {
        return Err(());
    }
    // A rename answer changes the rest of the plan (the old name isn't dropped, the new
    // one isn't added, so its NOT NULL conflict may go away): ask those first, and the
    // others on the plan made with them
    if out.iter().any(|(_, q)| is_rename(q)) {
        out.retain(|(_, q)| is_rename(q));
    }
    Ok(out)
}

fn is_rename(question: &Question) -> bool {
    matches!(
        question,
        Question::TableRename { .. } | Question::ColumnRename { .. }
    )
}

fn already_tightened(question: &Question, d: &Decisions) -> bool {
    matches!(question, Question::Tighten { table, column, .. }
        if d.tighten.contains(&(table.clone(), column.clone())))
}

enum Asked {
    /// An answer (or an earlier one) changed the decisions.
    Progress,
    Nothing,
    /// The user stopped, didn't confirm an op, or gave no usable answer.
    Stop,
}

async fn ask_all(
    asked: Vec<(usize, Question)>,
    prompt: &mut Option<Box<dyn Prompt + Send>>,
    decisions: &mut [Decisions],
    declined: &mut [Vec<Declined>],
) -> Asked {
    let mut progress = false;
    for (i, question) in asked {
        let (d, declined) = (&mut decisions[i], &mut declined[i]);
        // An earlier answer may have settled it, or taken one of its options
        let Some(question) = still_open(&question, d) else {
            progress = true;
            continue;
        };
        let mut reason = None;
        let mut tries = 0;
        loop {
            if tries == MAX_TRIES {
                return Asked::Stop;
            }
            tries += 1;
            let answer = ask(prompt, reason.take(), &question).await;
            match apply_answer(&question, &answer, d, declined) {
                Ok(Flow::Abort) => return Asked::Stop,
                // The conflict stays: nothing else can make the plan go through
                Ok(Flow::Continue)
                    if matches!((&question, &answer), (Question::Confirm { .. }, Answer::No)) =>
                {
                    return Asked::Stop;
                }
                Ok(Flow::Continue) => {
                    progress = true;
                    break;
                }
                Err(why) => reason = Some(why),
            }
        }
    }
    if progress {
        Asked::Progress
    } else {
        Asked::Nothing
    }
}

/// Asks on a blocking thread: the prompt moves there and comes back with the answer.
/// A prompt that panicked is gone, and counts as the user stopping.
async fn ask(
    prompt: &mut Option<Box<dyn Prompt + Send>>,
    reason: Option<String>,
    question: &Question,
) -> Answer {
    let Some(mut p) = prompt.take() else {
        return Answer::Abort;
    };
    let question = question.clone();
    let asked = tokio::task::spawn_blocking(move || {
        if let Some(reason) = reason {
            p.tell(&reason);
        }
        let answer = p.ask(&question);
        (p, answer)
    })
    .await;
    match asked {
        Ok((p, answer)) => {
            *prompt = Some(p);
            answer
        }
        Err(error) => {
            tracing::error!(%error, "the migration prompt failed");
            Answer::Abort
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::fixtures::{id, q};
    use crate::model::{ColumnDef, Logical};
    use crate::op::ConfirmKey;
    #[cfg(all(not(feature = "postgres"), not(feature = "mssql"), feature = "mysql"))]
    use canyon_core::connection::datasources::MySQLAuth;
    #[cfg(feature = "postgres")]
    use canyon_core::connection::datasources::PostgresAuth;
    #[cfg(all(not(feature = "postgres"), feature = "mssql"))]
    use canyon_core::connection::datasources::SqlServerAuth;
    use canyon_core::connection::datasources::{
        Auth, DatasourceProperties, Migrations, MigrationsSwitch,
    };
    use canyon_core::types::SqlType;
    use std::sync::{Arc, Mutex};

    /// Answers in order (then stops); remembers what it was asked and told.
    struct Scripted {
        answers: std::vec::IntoIter<Answer>,
        log: Log,
    }

    impl Prompt for Scripted {
        fn ask(&mut self, question: &Question) -> Answer {
            if let Ok(mut log) = self.log.lock() {
                log.push(format!(
                    "ask {}",
                    crate::interactive::render(question)
                        .lines()
                        .next()
                        .unwrap_or("")
                ));
            }
            self.answers.next().unwrap_or(Answer::Abort)
        }

        fn tell(&mut self, reason: &str) {
            if let Ok(mut log) = self.log.lock() {
                log.push(format!("tell {reason}"));
            }
        }
    }

    type Log = Arc<Mutex<Vec<String>>>;

    fn db(host: &str, port: u16, name: &str) -> DbIdentity {
        DbIdentity {
            db_type: "PostgreSql".into(),
            host: host.to_lowercase(),
            port,
            db_name: name.into(),
        }
    }

    #[test]
    fn locks_are_taken_by_database_not_by_configuration() {
        let (b, a, a2) = (db("h", 5432, "b"), db("H", 5432, "a"), db("h", 5432, "a"));
        assert_eq!(order_of(&[&b, &a]), [1, 0]);
        // Same host in another case is the same database; ties keep the given order
        assert_eq!(a, a2);
        assert_eq!(order_of(&[&a, &b, &a2]), [0, 2, 1]);
        // Unknown identities keep the configured order
        let unknown = DbIdentity::default();
        assert_eq!(order_of(&[&unknown, &unknown]), [0, 1]);
    }

    #[test]
    fn a_second_datasource_on_the_same_database_is_found() {
        let (a, b) = (db("h", 5432, "a"), db("h", 5432, "b"));
        let seen = [("first", &a), ("other", &b)];
        assert_eq!(
            first_with(seen.into_iter(), &db("H", 5432, "a")),
            Some("first")
        );
        assert_eq!(first_with(seen.into_iter(), &db("h", 5433, "a")), None);
        assert_eq!(first_with(seen.into_iter(), &db("h", 5432, "c")), None);
    }

    /// The auth of the first backend built in, and its default port.
    fn any_auth() -> (Auth, u16) {
        #[cfg(feature = "postgres")]
        return (
            Auth::Postgres(PostgresAuth::Basic {
                username: "u".into(),
                password: "p".into(),
            }),
            5432,
        );
        #[cfg(all(not(feature = "postgres"), feature = "mssql"))]
        return (
            Auth::SqlServer(SqlServerAuth::Basic {
                username: "u".into(),
                password: "p".into(),
            }),
            1433,
        );
        #[cfg(all(not(feature = "postgres"), not(feature = "mssql"), feature = "mysql"))]
        return (
            Auth::MySQL(MySQLAuth::Basic {
                username: "u".into(),
                password: "p".into(),
            }),
            3306,
        );
    }

    fn datasource(name: &str, port: Option<u16>, db_name: &str, enabled: bool) -> DatasourceConfig {
        let switch = if enabled {
            MigrationsSwitch::Enabled
        } else {
            MigrationsSwitch::Disabled
        };
        DatasourceConfig {
            name: name.into(),
            auth: any_auth().0,
            properties: DatasourceProperties {
                host: "localhost".into(),
                port,
                db_name: db_name.into(),
                migrations: Some(Migrations::Simple(switch)),
                pool: Default::default(),
                #[cfg(feature = "mssql")]
                mssql_tls: Default::default(),
            },
        }
    }

    fn names(datasources: &[DatasourceConfig]) -> Result<Vec<String>, String> {
        match enabled_datasources(datasources) {
            Ok(enabled) => Ok(enabled.iter().map(|(ds, ..)| ds.name.clone()).collect()),
            Err(MigrationError::Config { datasource, detail }) => {
                Err(format!("{datasource}: {detail}"))
            }
            Err(other) => Err(format!("not a config error: {other}")),
        }
    }

    #[test]
    fn disabled_and_unconfigured_datasources_are_skipped() {
        let mut unconfigured = datasource("none", None, "c", true);
        unconfigured.properties.migrations = None;
        let all = [
            datasource("a", None, "a", true),
            datasource("off", None, "a", false),
            unconfigured,
            datasource("b", None, "b", true),
        ];
        assert_eq!(names(&all), Ok(vec!["a".to_owned(), "b".to_owned()]));
    }

    #[test]
    fn an_enabled_datasource_without_name_is_refused() {
        let err = names(&[datasource("", None, "a", true)]).expect_err("no name");
        assert!(err.contains("no name"), "{err}");
        // Disabled, it doesn't matter
        assert_eq!(names(&[datasource("", None, "a", false)]), Ok(vec![]));
    }

    #[test]
    fn a_name_the_history_cant_store_is_refused() {
        let longest = "n".repeat(DATASOURCE_MAX_LEN);
        assert_eq!(
            names(&[datasource(&longest, None, "a", true)]),
            Ok(vec![longest.clone()])
        );
        let err =
            names(&[datasource(&format!("{longest}n"), None, "a", true)]).expect_err("too long");
        assert!(err.contains("longer than 100"), "{err}");
    }

    #[test]
    fn two_datasources_on_the_same_database_are_refused() {
        let default_port = any_auth().1;
        let err = names(&[
            datasource("first", None, "a", true),
            datasource("second", Some(default_port), "a", true),
        ])
        .expect_err("no port is the default port");
        assert_eq!(err, "second: points at the same database as `first`");
        let err = names(&[
            datasource("first", Some(default_port), "a", true),
            datasource("second", Some(default_port), "a", true),
        ])
        .expect_err("same port");
        assert!(err.starts_with("second:"), "{err}");
        // Another port or another database is another one
        let other_port = default_port + 1;
        assert!(
            names(&[
                datasource("first", None, "a", true),
                datasource("second", Some(other_port), "a", true),
                datasource("third", None, "b", true),
            ])
            .is_ok()
        );
        // A disabled one doesn't take the database
        assert!(
            names(&[
                datasource("off", None, "a", false),
                datasource("on", None, "a", true),
            ])
            .is_ok()
        );
    }

    #[test]
    fn the_identity_uses_the_port_the_datasource_connects_to() {
        let default_port = any_auth().1;
        let implicit = DbIdentity::of(&datasource("a", None, "a", true));
        assert_eq!(implicit.port, default_port);
        assert_eq!(
            implicit,
            DbIdentity::of(&datasource("b", Some(default_port), "a", true))
        );
    }

    fn scripted(answers: Vec<Answer>) -> (Option<Box<dyn Prompt + Send>>, Log) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let prompt = Scripted {
            answers: answers.into_iter(),
            log: Arc::clone(&log),
        };
        (Some(Box::new(prompt)), log)
    }

    fn confirm() -> Question {
        Question::Confirm {
            datasource: "a".into(),
            key: ConfirmKey::rebuild(&q("player")),
            what: "rebuild table player".into(),
        }
    }

    fn rename() -> Question {
        Question::TableRename {
            datasource: "a".into(),
            old: q("player"),
            candidates: vec![q("athlete")],
        }
    }

    fn tighten() -> Question {
        Question::Tighten {
            datasource: "a".into(),
            table: q("player"),
            column: id("age"),
            from: Logical::Known(SqlType::BigInt),
            to: SqlType::Int,
        }
    }

    async fn run(
        asked: Vec<Question>,
        answers: Vec<Answer>,
    ) -> (Asked, Decisions, Vec<Declined>, Vec<String>) {
        let (mut prompt, log) = scripted(answers);
        let mut decisions = vec![Decisions::default()];
        let mut declined = vec![Vec::new()];
        let asked = asked.into_iter().map(|q| (0, q)).collect();
        let outcome = ask_all(asked, &mut prompt, &mut decisions, &mut declined).await;
        let log = log.lock().map(|l| l.clone()).unwrap_or_default();
        let (Some(d), Some(declined)) = (decisions.pop(), declined.pop()) else {
            unreachable!("one datasource")
        };
        (outcome, d, declined, log)
    }

    // A current-thread runtime: asking must not need `block_in_place`
    #[tokio::test(flavor = "current_thread")]
    async fn a_yes_is_recorded() {
        let (outcome, d, _, log) = run(vec![confirm()], vec![Answer::Yes]).await;
        assert!(matches!(outcome, Asked::Progress));
        assert!(d.confirmed.contains(&ConfirmKey::rebuild(&q("player"))));
        assert_eq!(log.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_no_to_a_confirmation_stops() {
        let (outcome, d, _, log) = run(vec![confirm(), tighten()], vec![Answer::No]).await;
        assert!(matches!(outcome, Asked::Stop));
        assert!(d.confirmed.is_empty());
        assert_eq!(log.len(), 1, "nothing asked after the no: {log:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_bad_answer_is_asked_again_with_the_reason() {
        let answers = vec![Answer::Choose(5), Answer::Choose(0)];
        let (outcome, d, _, log) = run(vec![rename()], answers).await;
        assert!(matches!(outcome, Asked::Progress));
        assert_eq!(d.table_renames.get(&q("player")), Some(&Some(q("athlete"))));
        assert_eq!(log.len(), 3, "{log:?}");
        assert!(log[1].starts_with("tell "), "{log:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn endless_bad_answers_stop() {
        let answers = vec![Answer::Yes; MAX_TRIES + 3];
        let (outcome, _, _, log) = run(vec![rename()], answers).await;
        assert!(matches!(outcome, Asked::Stop));
        assert_eq!(
            log.iter().filter(|l| l.starts_with("ask")).count(),
            MAX_TRIES
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_closed_prompt_aborts() {
        let (outcome, ..) = run(vec![tighten(), rename()], vec![Answer::No]).await;
        assert!(matches!(outcome, Asked::Stop));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_declined_tighten_is_kept_for_the_snapshot() {
        let (outcome, d, declined, _) = run(vec![tighten()], vec![Answer::No]).await;
        assert!(matches!(outcome, Asked::Progress));
        assert!(d.tighten.is_empty());
        assert_eq!(
            declined,
            [Declined {
                table: q("player"),
                column: id("age"),
                column_type: SqlType::Int,
            }]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_name_taken_by_an_earlier_answer_is_not_offered_again() {
        let column = |name: &str| ColumnDef {
            name: id(name),
            ty: Logical::Known(SqlType::Int),
            nullable: true,
            default: None,
            extra: Default::default(),
        };
        let rename = |removed: &str| Question::ColumnRename {
            datasource: "a".into(),
            table: q("player"),
            removed: column(removed),
            added: vec![column("c")],
        };
        // `a` takes `c`; `b` has nothing left and is settled without asking
        let (outcome, d, _, log) =
            run(vec![rename("a"), rename("b")], vec![Answer::Choose(0)]).await;
        assert!(matches!(outcome, Asked::Progress));
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(d.column_renames.get(&(q("player"), id("b"))), Some(&None));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nothing_to_ask_is_no_progress() {
        let (outcome, ..) = run(Vec::new(), Vec::new()).await;
        assert!(matches!(outcome, Asked::Nothing));
    }

    #[test]
    fn tighten_offers_already_accepted_are_skipped() {
        let mut d = Decisions::default();
        assert!(!already_tightened(&tighten(), &d));
        d.tighten.insert((q("player"), id("age")));
        assert!(already_tightened(&tighten(), &d));
        assert!(!already_tightened(&rename(), &d));
    }
}
