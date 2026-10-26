//! Schema-change scenarios on the three databases: a start schema with rows, a target
//! model, and what the plan must be (or why it is refused). Every scenario checks the
//! planned ops, then the database after the run: it reads back as the target (a second
//! plan is empty) and the data survived.

use super::introspection::read;
use super::runner::{id, q, settings};
use crate::support::per_backend;
use crate::support::test_db::TestDb;
use canyon_migrations::memory::SnapshotDoc;
use canyon_migrations::model::{Generated, Ident, TableDef};
use canyon_migrations::op::Param;
use canyon_migrations::planning::{Conflict, Decisions, Info};
use canyon_migrations::report::{DatasourcePlan, PlanReport};
use canyon_migrations::runner::{ApplyOptions, MigrationsConfig, prepare_one};
use canyon_sql::CanyonResult;
use canyon_sql::connection::{DatabaseType, DbConnection};
use canyon_sql::core::SqlType;
use canyon_sql::date_time::NaiveDate;
use canyon_sql::migrations::{DbIdentity, MigrationError, Target, plan_models, run_models_with};
use canyon_sql::query::QueryParameter;
use std::future::Future;
use std::pin::Pin;

const HISTORY: &str = "canyon_migration_history";
const SNAPSHOT: &str = "canyon_schema_snapshot";

type Check = for<'a> fn(&'a TestDb, usize) -> Pin<Box<dyn Future<Output = CanyonResult<()>> + 'a>>;

pub(crate) struct Scenario {
    /// Applied first, with `allow_destructive`.
    start: Vec<TableDef>,
    /// Table, then (column, value).
    rows: Vec<(&'static str, Vec<(&'static str, Param)>)>,
    target: Vec<TableDef>,
    settings: MigrationsConfig,
    decisions: Option<Decisions>,
    expect: Expect,
}

pub(crate) enum Expect {
    /// `Op::describe()` of the planned ops, in order (Canyon's own tables left out). The
    /// check gets the statements the run executed.
    Applies {
        ops: Vec<&'static str>,
        mssql_ops: Option<Vec<&'static str>>,
        mysql_ops: Option<Vec<&'static str>>,
        then: Option<Check>,
    },
    /// The `Conflict` variants, in order.
    Conflicts(Vec<&'static str>),
}

impl Scenario {
    fn new(start: Vec<TableDef>, target: Vec<TableDef>, expect: Expect) -> Self {
        Self {
            start,
            rows: Vec::new(),
            target,
            settings: settings(5),
            decisions: None,
            expect,
        }
    }

    fn rows(mut self, table: &'static str, rows: Vec<Vec<(&'static str, Param)>>) -> Self {
        self.rows.extend(rows.into_iter().map(|r| (table, r)));
        self
    }

    fn settings(mut self, f: impl FnOnce(&mut MigrationsConfig)) -> Self {
        f(&mut self.settings);
        self
    }
}

fn applies(ops: &[&'static str]) -> Expect {
    Expect::Applies {
        ops: ops.to_vec(),
        mssql_ops: None,
        mysql_ops: None,
        then: None,
    }
}

fn applies_then(ops: &[&'static str], then: Check) -> Expect {
    Expect::Applies {
        ops: ops.to_vec(),
        mssql_ops: None,
        mysql_ops: None,
        then: Some(then),
    }
}

fn conflicts(names: &[&'static str]) -> Expect {
    Expect::Conflicts(names.to_vec())
}

// ---- model helpers

/// A column with its key flags, for `table`.
#[derive(Clone)]
struct Col {
    name: &'static str,
    ty: SqlType,
    nullable: bool,
    pk: Option<Option<Generated>>,
    fk: Option<(&'static str, &'static str)>,
}

fn col(name: &'static str, ty: SqlType) -> Col {
    Col {
        name,
        ty,
        nullable: false,
        pk: None,
        fk: None,
    }
}

impl Col {
    fn null(mut self) -> Self {
        self.nullable = true;
        self
    }
    fn pk(mut self) -> Self {
        self.pk = Some(None);
        self
    }
    fn identity(mut self) -> Self {
        self.pk = Some(Some(Generated::Identity));
        self
    }
    fn fk(mut self, table: &'static str, column: &'static str) -> Self {
        self.fk = Some((table, column));
        self
    }
}

fn table(entity: &str, name: &str, cols: Vec<Col>) -> TableDef {
    let mut b = TableDef::builder(entity, name);
    for c in cols {
        b = b.column(c.name, c.ty, c.nullable);
        if let Some(generated) = c.pk {
            b = b.primary_key(generated);
        }
        if let Some((t, column)) = c.fk {
            b = b.foreign_key(t, column);
        }
    }
    b.build().expect("a valid table")
}

const INT: SqlType = SqlType::Int;
const BIG: SqlType = SqlType::BigInt;
const SMALL: SqlType = SqlType::SmallInt;

fn text(n: u32) -> SqlType {
    SqlType::Text { max_len: Some(n) }
}

fn i(v: i32) -> Param {
    Param::Int(v)
}

fn big(v: i64) -> Param {
    Param::BigInt(v)
}

fn t(v: &str) -> Param {
    Param::Text(v.to_owned())
}

/// `player(id pk, name text(100), age i32?)` and friends.
fn player(cols: Vec<Col>) -> TableDef {
    table("Player", "player", cols)
}

fn id_name() -> Vec<Col> {
    vec![col("id", INT).pk(), col("name", text(100))]
}

fn with(mut cols: Vec<Col>, extra: Col) -> Vec<Col> {
    cols.push(extra);
    cols
}

fn league_t(identity: bool, id_ty: SqlType) -> TableDef {
    let id = col("id", id_ty);
    let id = if identity { id.identity() } else { id.pk() };
    table("League", "league", vec![id, col("name", text(50)).null()])
}

fn tournament_t(league_ty: SqlType, fk: bool) -> TableDef {
    let l = col("league", league_ty);
    let l = if fk { l.fk("league", "id") } else { l };
    table("Tournament", "tournament", vec![col("id", INT).pk(), l])
}

// ---- the harness

fn is_mssql(db: &TestDb) -> bool {
    #[cfg(feature = "mssql")]
    {
        db.db_type() == DatabaseType::SqlServer
    }
    #[cfg(not(feature = "mssql"))]
    {
        let _ = db;
        false
    }
}

fn placeholders(db: &TestDb, n: usize) -> Vec<String> {
    (1..=n)
        .map(|k| match db.db_type() {
            #[cfg(feature = "postgres")]
            DatabaseType::PostgreSql => format!("${k}"),
            #[cfg(feature = "mssql")]
            DatabaseType::SqlServer => format!("@P{k}"),
            #[cfg(feature = "mysql")]
            DatabaseType::MySQL => {
                let _ = k;
                "?".to_owned()
            }
        })
        .collect()
}

async fn insert(db: &TestDb, table: &str, values: &[(&str, Param)]) -> CanyonResult<()> {
    let names: Vec<&str> = values.iter().map(|(c, _)| *c).collect();
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({})",
        names.join(", "),
        placeholders(db, values.len()).join(", ")
    );
    let params: Vec<&dyn QueryParameter> = values.iter().map(|(_, p)| p.as_query_param()).collect();
    db.conn().execute(&sql, &params).await?;
    Ok(())
}

/// One number: `SELECT CAST(<expr> AS bigint) FROM <from>`.
pub(crate) async fn num(db: &TestDb, expr: &str, from: &str) -> CanyonResult<i64> {
    let ty = match db.db_type() {
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => "SIGNED",
        _ => "BIGINT",
    };
    db.conn()
        .query_one_for(&format!("SELECT CAST({expr} AS {ty}) FROM {from}"), &[])
        .await
}

async fn count(db: &TestDb, from: &str) -> CanyonResult<i64> {
    num(db, "COUNT(*)", from).await
}

async fn string(db: &TestDb, expr: &str, from: &str) -> CanyonResult<String> {
    db.conn()
        .query_one_for(&format!("SELECT {expr} FROM {from}"), &[])
        .await
}

/// Rows of Canyon's history and snapshot tables (0 for one that isn't there yet).
async fn canyon_rows(
    db: &TestDb,
    catalog: &canyon_migrations::introspect::Catalog,
) -> CanyonResult<(i64, i64)> {
    let mut found = [0, 0];
    for (slot, table) in found.iter_mut().zip([HISTORY, SNAPSHOT]) {
        if catalog.canyon_tables.iter().any(|t| t == table) {
            *slot = count(db, table).await?;
        }
    }
    Ok((found[0], found[1]))
}

fn conflict_name(c: &Conflict) -> String {
    format!("{c:?}")
        .split(['{', '(', ' '])
        .next()
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn target<'a>(db: &'a TestDb, settings: MigrationsConfig) -> [Target<'a>; 1] {
    [Target {
        datasource: "scenario",
        conn: db.conn(),
        settings,
        identity: DbIdentity::default(),
    }]
}

/// What `plan_models` plans, without Canyon's own tables.
pub(crate) async fn planned(
    db: &TestDb,
    settings: &MigrationsConfig,
    desired: &[TableDef],
    decisions: Option<&Decisions>,
) -> CanyonResult<(Result<Vec<String>, Vec<String>>, Vec<Info>)> {
    // `plan_models` takes its decisions from the settings only
    let plan = match decisions {
        None => plan_models(&target(db, *settings), desired).await?,
        Some(decisions) => plan_with(db, settings, desired, decisions).await?,
    };
    let one = &plan.datasources[0];
    let outcome = match &one.outcome {
        Ok(ops) => Ok(ops
            .iter()
            .map(|o| o.description.clone())
            .filter(|d| {
                // Canyon's own tables; a schema named `canyon_…` must not match
                d != &format!("create table {HISTORY}") && d != &format!("create table {SNAPSHOT}")
            })
            .collect()),
        Err(c) => Err(c.iter().map(conflict_name).collect()),
    };
    Ok((outcome, one.infos.clone()))
}

async fn plan_with(
    db: &TestDb,
    settings: &MigrationsConfig,
    desired: &[TableDef],
    decisions: &Decisions,
) -> Result<PlanReport, MigrationError> {
    use canyon_sql::query::querybuilder::syntax::dialect::*;
    macro_rules! stage {
        ($d:ty) => {{
            let staged =
                prepare_one::<$d>(db.conn(), "scenario", desired, settings, decisions, false)
                    .await?;
            let outcome = match staged.conflicts() {
                [] => staged.render::<$d>().map(Ok)?,
                conflicts => Err(conflicts.to_vec()),
            };
            DatasourcePlan {
                datasource: "scenario".to_owned(),
                outcome,
                infos: staged.infos().to_vec(),
                warnings: staged.warnings().to_vec(),
            }
        }};
    }
    let one = match db.db_type() {
        #[cfg(feature = "postgres")]
        DatabaseType::PostgreSql => stage!(PgDialect),
        #[cfg(feature = "mssql")]
        DatabaseType::SqlServer => stage!(MsSql),
        #[cfg(feature = "mysql")]
        DatabaseType::MySQL => stage!(MySql),
    };
    Ok(PlanReport {
        datasources: vec![one],
    })
}

async fn run(
    db: &TestDb,
    settings: &MigrationsConfig,
    desired: &[TableDef],
    decisions: Option<&Decisions>,
) -> Result<canyon_sql::migrations::Report, MigrationError> {
    run_models_with(
        &target(db, *settings),
        desired,
        decisions,
        &ApplyOptions::default(),
    )
    .await
}

pub(crate) async fn run_scenario(datasource: &str, name: &str, s: Scenario) -> CanyonResult<()> {
    TestDb::with(datasource, name, async |db| exec(db, name, &s).await).await
}

async fn exec(db: &TestDb, name: &str, s: &Scenario) -> CanyonResult<()> {
    {
        // Start schema, with everything allowed
        let mut start_settings = settings(5);
        start_settings.allow_destructive = true;
        if !s.start.is_empty() {
            run(db, &start_settings, &s.start, None).await?;
        }
        for (table, values) in &s.rows {
            insert(db, table, values).await?;
        }

        let before = read(db, &[]).await;
        let history_before = canyon_rows(db, &before).await?;
        let (outcome, _) = planned(db, &s.settings, &s.target, s.decisions.as_ref()).await?;
        match &s.expect {
            Expect::Applies {
                ops,
                mssql_ops,
                mysql_ops,
                then,
            } => {
                let want = match (is_mssql(db), mssql_ops, on_mysql(db), mysql_ops) {
                    (true, Some(ops), ..) | (_, _, true, Some(ops)) => ops,
                    _ => ops,
                };
                assert_eq!(
                    outcome.as_ref().ok(),
                    Some(&want.iter().map(|d| (*d).to_owned()).collect()),
                    "{name}: planned ops"
                );
                let report = run(db, &s.settings, &s.target, s.decisions.as_ref()).await?;
                // Converged and idempotent: nothing left to do
                let (again, _) = planned(db, &s.settings, &s.target, s.decisions.as_ref()).await?;
                assert_eq!(
                    again,
                    Ok(Vec::new()),
                    "{name}: the database reads back as the target"
                );
                let model = read(db, &[]).await.model;
                assert!(
                    s.target.iter().all(|t| model.table(&t.name).is_some()),
                    "{name}: every target table exists"
                );
                if let Some(then) = then {
                    then(db, report.datasources[0].statements).await?;
                }
            }
            Expect::Conflicts(want) => {
                let want: Vec<String> = want.iter().map(|c| (*c).to_owned()).collect();
                assert_eq!(outcome, Err(want.clone()), "{name}: planned conflicts");
                let err = run(db, &s.settings, &s.target, s.decisions.as_ref())
                    .await
                    .expect_err("a plan with conflicts");
                let MigrationError::Conflicts(found) = &err else {
                    panic!("{name}: not a conflicts error: {err}");
                };
                let found: Vec<String> = found
                    .iter()
                    .flat_map(|(_, c)| c.iter().map(conflict_name))
                    .collect();
                assert_eq!(found, want, "{name}: conflicts of the run");
                let after = read(db, &[]).await;
                assert_eq!(
                    canyon_rows(db, &after).await?,
                    history_before,
                    "{name}: history and snapshot rows unchanged"
                );
                assert_eq!(after.model, before.model, "{name}: model unchanged");
                assert_eq!(
                    after.canyon_tables, before.canyon_tables,
                    "{name}: Canyon's tables unchanged"
                );
            }
        }
        Ok(())
    }
}

/// A test per backend for a scenario built by `$build`.
macro_rules! scenario {
    ($name:ident, $build:expr) => {
        pub(crate) async fn $name(datasource: &str) -> CanyonResult<()> {
            let build: fn() -> Scenario = $build;
            run_scenario(datasource, stringify!($name), build()).await
        }
        per_backend!($name, super::$name);
    };
}

// ---- 1-5: creating and adding

scenario!(create_from_empty, || Scenario::new(
    vec![],
    vec![player(with(id_name(), col("age", INT).null()))],
    applies(&["create table player"]),
));

scenario!(add_nullable_column, || Scenario::new(
    vec![player(id_name())],
    vec![player(with(id_name(), col("age", INT).null()))],
    applies_then(&["add column player.age (integer)"], |db, _| {
        Box::pin(async move {
            assert_eq!(count(db, "player").await?, 2);
            assert_eq!(count(db, "player WHERE age IS NULL").await?, 2);
            Ok(())
        })
    }),
)
.rows("player", two_players()));

scenario!(add_not_null_empty_table, || Scenario::new(
    vec![player(id_name())],
    vec![player(with(id_name(), col("score", INT)))],
    applies(&["add column player.score (integer)"]),
));

fn two_players() -> Vec<Vec<(&'static str, Param)>> {
    vec![
        vec![("id", i(1)), ("name", t("a"))],
        vec![("id", i(2)), ("name", t("b"))],
    ]
}

scenario!(add_not_null_with_rows, || Scenario::new(
    vec![player(id_name())],
    vec![player(with(id_name(), col("score", INT)))],
    conflicts(&["NotNullOnTableWithRows"]),
)
.rows("player", two_players()));

fn both_scores_zero<'a>(
    db: &'a TestDb,
    _: usize,
) -> Pin<Box<dyn Future<Output = CanyonResult<()>> + 'a>> {
    Box::pin(async move {
        assert_eq!(count(db, "player WHERE score = 0").await?, 2);
        Ok(())
    })
}

scenario!(add_not_null_with_fill_value, || {
    let mut s = Scenario::new(
        vec![player(id_name())],
        vec![player(with(id_name(), col("score", INT)))],
        applies_then(
            &[
                "add column player.score (integer)",
                "fill empty values of player.score",
                "make player.score NOT NULL",
            ],
            both_scores_zero,
        ),
    )
    .rows("player", two_players());
    let mut decisions = Decisions::default();
    decisions
        .fill_values
        .insert((q("player"), id("score")), Param::Int(0));
    s.decisions = Some(decisions);
    s
});

// ---- 6: destructive

fn with_nick() -> Vec<Col> {
    with(id_name(), col("nick", text(20)).null())
}

scenario!(drop_column_needs_confirmation, || Scenario::new(
    vec![player(with_nick())],
    vec![player(id_name())],
    conflicts(&["Destructive"]),
));

scenario!(drop_column_when_allowed, || Scenario::new(
    vec![player(with_nick())],
    vec![player(id_name())],
    applies(&["drop column player.nick"]),
)
.settings(|s| s.allow_destructive = true));

// ---- 7-11: renames

fn kept_names<'a>(
    db: &'a TestDb,
    table: &'static str,
    column: &'static str,
) -> Pin<Box<dyn Future<Output = CanyonResult<()>> + 'a>> {
    Box::pin(async move {
        assert_eq!(count(db, table).await?, 2);
        assert_eq!(
            string(db, column, &format!("{table} WHERE id = 2")).await?,
            "b"
        );
        Ok(())
    })
}

scenario!(rename_table_by_table_name, || Scenario::new(
    vec![player(id_name())],
    vec![table("Player", "players", id_name())],
    applies_then(&["rename table player to players"], |db, _| kept_names(
        db, "players", "name"
    )),
)
.rows("player", two_players()));

scenario!(rename_struct_same_columns, || Scenario::new(
    vec![player(id_name())],
    vec![table("Gamer", "gamer", id_name())],
    applies_then(&["rename table player to gamer"], |db, _| kept_names(
        db, "gamer", "name"
    )),
)
.rows("player", two_players()));

scenario!(rename_struct_and_add_column, || Scenario::new(
    vec![player(id_name())],
    vec![table(
        "Gamer",
        "gamer",
        with(id_name(), col("age", INT).null())
    )],
    applies_then(
        &[
            "rename table player to gamer",
            "add column gamer.age (integer)"
        ],
        |db, _| kept_names(db, "gamer", "name")
    ),
)
.rows("player", two_players()));

scenario!(rename_column, || Scenario::new(
    vec![player(id_name())],
    vec![player(vec![
        col("id", INT).pk(),
        col("full_name", text(100))
    ])],
    applies_then(&["rename column player.name to full_name"], |db, _| {
        kept_names(db, "player", "full_name")
    }),
)
.rows("player", two_players()));

scenario!(ambiguous_column_rename, || Scenario::new(
    vec![player(with(id_name(), col("age", INT)))],
    vec![player(with(id_name(), col("birth_date", SqlType::Date)))],
    conflicts(&["AmbiguousColumnRename", "Destructive"]),
));

// ---- 12-17: column types and nullability

fn ages(rows: &[(i32, Param)]) -> Vec<Vec<(&'static str, Param)>> {
    rows.iter()
        .map(|(n, a)| vec![("id", i(*n)), ("name", t("x")), ("age", a.clone())])
        .collect()
}

fn age_is<'a>(
    db: &'a TestDb,
    want: &'static [i64],
) -> Pin<Box<dyn Future<Output = CanyonResult<()>> + 'a>> {
    Box::pin(async move {
        for (n, v) in want.iter().enumerate() {
            let got = num(db, "age", &format!("player WHERE id = {}", n + 1)).await?;
            assert_eq!(got, *v, "age of player {}", n + 1);
        }
        Ok(())
    })
}

fn aged(ty: SqlType, null: bool) -> TableDef {
    let c = col("age", ty);
    player(with(id_name(), if null { c.null() } else { c }))
}

scenario!(widen_integer, || Scenario::new(
    vec![aged(INT, false)],
    vec![aged(BIG, false)],
    applies_then(&["change type of player.age to bigint"], |db, _| age_is(
        db,
        &[1, 2]
    )),
)
.rows("player", ages(&[(1, i(1)), (2, i(2))])));

scenario!(narrow_integer_fits, || Scenario::new(
    vec![aged(BIG, false)],
    vec![aged(INT, false)],
    applies_then(&["change type of player.age to integer"], |db, _| age_is(
        db,
        &[1, 2]
    )),
)
.rows("player", ages(&[(1, big(1)), (2, big(2))])));

scenario!(narrow_integer_out_of_range, || Scenario::new(
    vec![aged(BIG, false)],
    vec![aged(INT, false)],
    conflicts(&["ValuesOutOfRange"]),
)
.rows("player", ages(&[(1, big(1)), (2, big(1 << 40))])));

scenario!(narrow_int_to_smallint, || Scenario::new(
    vec![aged(INT, false)],
    vec![aged(SMALL, false)],
    applies_then(&["change type of player.age to smallint"], |db, _| age_is(
        db,
        &[1, 2]
    )),
)
.rows("player", ages(&[(1, i(1)), (2, i(2))])));

scenario!(narrow_int_to_smallint_out_of_range, || Scenario::new(
    vec![aged(INT, false)],
    vec![aged(SMALL, false)],
    conflicts(&["ValuesOutOfRange"]),
)
.rows("player", ages(&[(1, i(1)), (2, i(40_000))])));

fn named(len: u32) -> TableDef {
    player(vec![col("id", INT).pk(), col("name", text(len))])
}

scenario!(text_longer, || Scenario::new(
    vec![named(100)],
    vec![named(200)],
    applies_then(&["change type of player.name to text(200)"], |db, _| {
        kept_names(db, "player", "name")
    }),
)
.rows("player", two_players()));

scenario!(text_shorter_fits, || Scenario::new(
    vec![named(200)],
    vec![named(5)],
    applies_then(&["change type of player.name to text(5)"], |db, _| {
        kept_names(db, "player", "name")
    }),
)
.rows("player", two_players()));

scenario!(text_shorter_too_long, || Scenario::new(
    vec![named(200)],
    vec![named(5)],
    conflicts(&["ValuesTooLong"]),
)
.rows("player", vec![vec![("id", i(1)), ("name", t("toolong"))]]));

scenario!(not_null_with_nulls, || Scenario::new(
    vec![aged(INT, true)],
    vec![aged(INT, false)],
    conflicts(&["NullsBlockNotNull"]),
)
.rows(
    "player",
    vec![
        vec![("id", i(1)), ("name", t("x")), ("age", i(3))],
        vec![("id", i(2)), ("name", t("y"))]
    ]
));

scenario!(nullable_to_not_null, || Scenario::new(
    vec![aged(INT, true)],
    vec![aged(INT, false)],
    applies_then(&["make player.age NOT NULL"], |db, _| age_is(db, &[1, 2])),
)
.rows("player", ages(&[(1, i(1)), (2, i(2))])));

scenario!(not_null_to_nullable, || Scenario::new(
    vec![aged(INT, false)],
    vec![aged(INT, true)],
    applies_then(&["make player.age nullable"], |db, _| age_is(db, &[1, 2])),
)
.rows("player", ages(&[(1, i(1)), (2, i(2))])));

// ---- 18: primary key

fn no_pk() -> TableDef {
    player(vec![col("id", INT), col("name", text(100))])
}

scenario!(primary_key_add, || Scenario::new(
    vec![no_pk()],
    vec![player(id_name())],
    applies_then(&["add primary key to player (id)"], |db, _| {
        Box::pin(async move {
            assert_eq!(count(db, "player").await?, 2);
            let dup = insert(db, "player", &[("id", i(1)), ("name", t("c"))]).await;
            assert!(dup.is_err(), "the key refuses a repeated id");
            Ok(())
        })
    }),
)
.rows("player", two_players()));

scenario!(primary_key_add_duplicates, || Scenario::new(
    vec![no_pk()],
    vec![player(id_name())],
    conflicts(&["DuplicateKeyValues"]),
)
.rows(
    "player",
    vec![
        vec![("id", i(1)), ("name", t("a"))],
        vec![("id", i(1)), ("name", t("b"))]
    ]
));

/// `no_pk()` plus a new `code` column as the key, filled with `"a"`.
fn filled_new_key(rows: usize) -> Scenario {
    let target = player(vec![
        col("id", INT),
        col("name", text(100)),
        col("code", text(20)).pk(),
    ]);
    let expect = if rows == 1 {
        applies_then(
            &[
                "add column player.code (text(20))",
                "fill empty values of player.code",
                "make player.code NOT NULL",
                "add primary key to player (code)",
            ],
            |db, _| {
                Box::pin(async move {
                    assert_eq!(string(db, "code", "player WHERE id = 1").await?, "a");
                    Ok(())
                })
            },
        )
    } else {
        conflicts(&["DuplicateKeyValues"])
    };
    let mut s = Scenario::new(vec![no_pk()], vec![target], expect)
        .rows("player", two_players().into_iter().take(rows).collect());
    let mut decisions = Decisions::default();
    decisions
        .fill_values
        .insert((q("player"), id("code")), t("a"));
    s.decisions = Some(decisions);
    s
}

// One row can't repeat the fill value; two can
scenario!(primary_key_from_a_fill_one_row, || filled_new_key(1));
scenario!(primary_key_from_a_fill_two_rows, || filled_new_key(2));

// ---- 19-20: identity

fn identity_player(identity: bool) -> TableDef {
    let c = col("id", INT);
    player(vec![
        if identity { c.identity() } else { c.pk() },
        col("name", text(100)),
    ])
}

fn next_id_is_3<'a>(
    db: &'a TestDb,
    _: usize,
) -> Pin<Box<dyn Future<Output = CanyonResult<()>> + 'a>> {
    Box::pin(async move {
        insert(db, "player", &[("name", t("c"))]).await?;
        assert_eq!(num(db, "id", "player WHERE name = 'c'").await?, 3);
        assert_eq!(count(db, "player").await?, 3);
        Ok(())
    })
}

scenario!(identity_add, || {
    let mut s = Scenario::new(
        vec![identity_player(false)],
        vec![identity_player(true)],
        Expect::Applies {
            ops: vec!["make player.id generated by the database"],
            mssql_ops: Some(vec!["rebuild table player"]),
            mysql_ops: None,
            then: Some(next_id_is_3),
        },
    );
    s = s.rows("player", two_players());
    s
});

scenario!(identity_drop, || Scenario::new(
    vec![identity_player(true)],
    vec![identity_player(false)],
    Expect::Applies {
        ops: vec!["stop generating player.id"],
        mssql_ops: Some(vec!["rebuild table player"]),
        mysql_ops: None,
        then: Some(|db, _| kept_names(db, "player", "name")),
    },
)
.rows(
    "player",
    vec![vec![("name", t("a"))], vec![("name", t("b"))]]
));

// ---- 21: identity under a foreign key

fn league_rows() -> Vec<Vec<(&'static str, Param)>> {
    vec![vec![("id", i(1)), ("name", t("l"))]]
}

scenario!(identity_add_with_fk_pointing_at_it, || {
    Scenario::new(
        vec![league_t(false, INT), tournament_t(INT, true)],
        vec![league_t(true, INT), tournament_t(INT, true)],
        Expect::Applies {
            ops: vec!["make league.id generated by the database"],
            mssql_ops: Some(vec![
                "drop foreign key tournament_league_fkey of tournament",
                "rebuild table league",
                "add foreign key tournament (league) to league",
            ]),
            // MySQL won't add AUTO_INCREMENT to a column an FK uses
            mysql_ops: Some(vec![
                "drop foreign key tournament_league_fkey of tournament",
                "make league.id generated by the database",
                "add foreign key tournament (league) to league",
            ]),
            then: Some(|db, _| {
                Box::pin(async move {
                    assert_eq!(count(db, "tournament WHERE league = 1").await?, 1);
                    insert(db, "league", &[("name", t("m"))]).await?;
                    assert_eq!(num(db, "id", "league WHERE name = 'm'").await?, 2);
                    Ok(())
                })
            }),
        },
    )
    .rows("league", league_rows())
    .rows("tournament", vec![vec![("id", i(1)), ("league", i(1))]])
});

scenario!(identity_drop_with_fk_pointing_at_it, || {
    Scenario::new(
        vec![league_t(true, INT), tournament_t(INT, true)],
        vec![league_t(false, INT), tournament_t(INT, true)],
        Expect::Applies {
            ops: vec!["stop generating league.id"],
            mssql_ops: Some(vec![
                "drop foreign key tournament_league_fkey of tournament",
                "rebuild table league",
                "add foreign key tournament (league) to league",
            ]),
            mysql_ops: Some(vec![
                "drop foreign key tournament_league_fkey of tournament",
                "stop generating league.id",
                "add foreign key tournament (league) to league",
            ]),
            then: Some(|db, _| {
                Box::pin(async move {
                    assert_eq!(count(db, "tournament WHERE league = 1").await?, 1);
                    Ok(())
                })
            }),
        },
    )
    .rows("league", vec![vec![("name", t("l"))]])
    .rows("tournament", vec![vec![("id", i(1)), ("league", i(1))]])
});

// ---- 22: foreign keys

fn player_in_league(fk: bool) -> TableDef {
    let l = col("league", INT);
    player(vec![
        col("id", INT).pk(),
        if fk { l.fk("league", "id") } else { l },
    ])
}

scenario!(foreign_key_add_orphan, || Scenario::new(
    vec![league_t(false, INT), player_in_league(false)],
    vec![league_t(false, INT), player_in_league(true)],
    conflicts(&["OrphanRows"]),
)
.rows("league", league_rows())
.rows(
    "player",
    vec![
        vec![("id", i(1)), ("league", i(1))],
        vec![("id", i(2)), ("league", i(9))]
    ]
));

scenario!(foreign_key_add_clean, || Scenario::new(
    vec![league_t(false, INT), player_in_league(false)],
    vec![league_t(false, INT), player_in_league(true)],
    applies_then(&["add foreign key player (league) to league"], |db, _| {
        Box::pin(async move {
            assert_eq!(count(db, "player WHERE league = 1").await?, 1);
            let orphan = insert(db, "player", &[("id", i(2)), ("league", i(9))]).await;
            assert!(orphan.is_err(), "the key refuses an orphan");
            Ok(())
        })
    }),
)
.rows("league", league_rows())
.rows("player", vec![vec![("id", i(1)), ("league", i(1))]]));

scenario!(foreign_key_drop, || Scenario::new(
    vec![league_t(false, INT), player_in_league(true)],
    vec![league_t(false, INT), player_in_league(false)],
    applies_then(
        &["drop foreign key player_league_fkey of player"],
        |db, _| {
            Box::pin(async move {
                assert_eq!(count(db, "player WHERE league = 1").await?, 1);
                insert(db, "player", &[("id", i(2)), ("league", i(9))]).await?;
                assert_eq!(count(db, "player").await?, 2);
                Ok(())
            })
        }
    ),
)
.rows("league", league_rows())
.rows("player", vec![vec![("id", i(1)), ("league", i(1))]]));

// ---- 23: a cycle of foreign keys

scenario!(foreign_key_cycle, || Scenario::new(
    vec![],
    vec![
        table(
            "A",
            "a",
            vec![col("id", INT).pk(), col("b_id", INT).null().fk("b", "id")]
        ),
        table(
            "B",
            "b",
            vec![col("id", INT).pk(), col("a_id", INT).null().fk("a", "id")]
        ),
    ],
    applies(&[
        "create table a",
        "create table b",
        "add foreign key a (b_id) to b",
        "add foreign key b (a_id) to a",
    ]),
));

// ---- 24: key type change under a foreign key

scenario!(pk_type_change_with_fk, || Scenario::new(
    vec![league_t(false, INT), tournament_t(INT, true)],
    vec![league_t(false, BIG), tournament_t(BIG, true)],
    Expect::Applies {
        ops: vec![
            "drop foreign key tournament_league_fkey of tournament",
            "change type of league.id to bigint",
            "change type of tournament.league to bigint",
            "add foreign key tournament (league) to league",
        ],
        mssql_ops: Some(vec![
            "drop foreign key tournament_league_fkey of tournament",
            "drop primary key of league",
            "change type of league.id to bigint",
            "change type of tournament.league to bigint",
            "add primary key to league (id)",
            "add foreign key tournament (league) to league",
        ]),
        mysql_ops: None,
        then: Some(|db, _| {
            Box::pin(async move {
                assert_eq!(count(db, "tournament WHERE league = 1").await?, 1);
                Ok(())
            })
        }),
    },
)
.rows("league", league_rows())
.rows("tournament", vec![vec![("id", i(1)), ("league", i(1))]]));

// ---- 25: Canyon's tables and the history

scenario!(canyon_tables_and_history, || Scenario::new(
    vec![],
    vec![player(id_name())],
    applies_then(&["create table player"], |db, statements| {
        Box::pin(async move {
            let catalog = read(db, &[]).await;
            assert_eq!(catalog.canyon_tables, [HISTORY, SNAPSHOT]);
            // The first run: Canyon's two tables and `player`, one history row per statement
            assert!(statements >= 3, "{statements}");
            let statements = i64::try_from(statements).unwrap_or(i64::MAX);
            assert_eq!(count(db, HISTORY).await?, statements);
            assert_eq!(
                count(db, &format!("{HISTORY} WHERE status = 'ok'")).await?,
                statements
            );
            assert_eq!(count(db, SNAPSHOT).await?, 1);
            let json: String = db
                .conn()
                .query_one_for(&format!("SELECT model FROM {SNAPSHOT}"), &[])
                .await?;
            let doc = SnapshotDoc::from_json(&json).expect("a readable snapshot");
            let stored: Vec<_> = doc.tables.tables.keys().cloned().collect();
            assert_eq!(stored, [q("player")], "the model, without Canyon's tables");
            Ok(())
        })
    }),
));

// ---- 26: an entity removed

scenario!(unmanaged_table_kept, || Scenario::new(
    vec![
        player(id_name()),
        table("Old", "old_t", vec![col("id", INT).pk()])
    ],
    vec![player(id_name())],
    applies_then(&[], |db, _| {
        Box::pin(async move {
            let model = read(db, &[]).await.model;
            assert!(model.table(&q("old_t")).is_some(), "the table stays");
            Ok(())
        })
    }),
));

pub(crate) async fn unmanaged_table_is_reported(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "unmanaged_info", async |db| {
        let mut start = settings(5);
        start.allow_destructive = true;
        let old = table("Old", "old_t", vec![col("id", INT).pk()]);
        run(db, &start, &[player(id_name()), old], None).await?;
        let (outcome, infos) = planned(db, &settings(5), &[player(id_name())], None).await?;
        assert_eq!(outcome, Ok(Vec::new()));
        assert_eq!(infos, [Info::Unmanaged { table: q("old_t") }]);
        Ok(())
    })
    .await
}
per_backend!(
    unmanaged_table_is_reported,
    super::unmanaged_table_is_reported
);

// ---- a big rebuild asks first (SQL Server; the others change the column in place)

pub(crate) async fn large_rebuild_needs_confirmation(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "large_rebuild", async |db| {
        let s = Scenario::new(
            vec![identity_player(false)],
            vec![identity_player(true)],
            if is_mssql(db) {
                conflicts(&["LargeRebuild"])
            } else {
                applies(&["make player.id generated by the database"])
            },
        )
        .rows("player", two_players())
        .settings(|s| s.rebuild_confirm_rows = 1);
        exec(db, "large_rebuild", &s).await
    })
    .await
}
per_backend!(
    large_rebuild_needs_confirmation,
    super::large_rebuild_needs_confirmation
);

// ---- a run that stopped half way (MySQL has no transactional DDL) continues from what is there

async fn stops_then_continues(
    db: &TestDb,
    start: &[TableDef],
    target_models: &[TableDef],
    first: &[&str],
    stop_after: usize,
    second: &[&str],
) -> CanyonResult<()> {
    let mut all = settings(5);
    all.allow_destructive = true;
    run(db, &all, start, None).await?;
    insert(db, "player", &[("id", i(1)), ("name", t("a"))]).await?;
    let want = |d: &[&str]| -> Result<Vec<String>, Vec<String>> {
        Ok(d.iter().map(|d| (*d).to_owned()).collect())
    };
    assert_eq!(planned(db, &all, target_models, None).await?.0, want(first));
    let before = read(db, &[]).await;
    let err = run_models_with(
        &target(db, all),
        target_models,
        None,
        &ApplyOptions {
            fail_after: Some(stop_after),
        },
    )
    .await
    .expect_err("a forced failure");
    if !on_mysql(db) {
        // One transaction (or its SQL Server equivalent): the failure leaves nothing behind
        assert!(
            matches!(
                err,
                MigrationError::Apply {
                    rolled_back: true,
                    ..
                }
            ),
            "{err}"
        );
        let after = read(db, &[]).await;
        assert_eq!(after.model, before.model);
        assert_eq!(after.canyon_tables, before.canyon_tables);
        assert_eq!(planned(db, &all, target_models, None).await?.0, want(first));
        run(db, &all, target_models, None).await?;
        assert_eq!(planned(db, &all, target_models, None).await?.0, want(&[]));
        return Ok(());
    }
    assert!(
        matches!(
            err,
            MigrationError::Apply {
                rolled_back: false,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(
        planned(db, &all, target_models, None).await?.0,
        want(second)
    );
    run(db, &all, target_models, None).await?;
    assert_eq!(planned(db, &all, target_models, None).await?.0, want(&[]));
    Ok(())
}

fn on_mysql(db: &TestDb) -> bool {
    #[cfg(feature = "mysql")]
    {
        db.db_type() == DatabaseType::MySQL
    }
    #[cfg(not(feature = "mysql"))]
    {
        let _ = db;
        false
    }
}

pub(crate) async fn resume_after_move(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "resume_move", async |db| {
        let s1 = db.schema("s1");
        let moved = TableDef::builder("Player", "gamer")
            .schema(&s1)
            .column("id", INT, false)
            .primary_key(None)
            .column("name", text(100), false)
            .build()
            .expect("a valid table");
        let create = format!("create schema {s1}");
        let mv = format!("move table player to schema {s1}");
        let rename = format!("rename table {s1}.player to gamer");
        stops_then_continues(
            db,
            &[player(id_name())],
            &[moved],
            &[&create, &mv, &rename],
            2,
            &[&rename],
        )
        .await?;
        assert_eq!(count(db, &format!("{s1}.gamer")).await?, 1, "rows kept");
        Ok(())
    })
    .await
}
per_backend!(resume_after_move, super::resume_after_move);

pub(crate) async fn resume_after_table_rename(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "resume_rename", async |db| {
        let target = table(
            "Player",
            "gamer",
            vec![col("id", INT).pk(), col("full_name", text(100))],
        );
        stops_then_continues(
            db,
            &[player(id_name())],
            &[target],
            &[
                "rename table player to gamer",
                "rename column gamer.name to full_name",
            ],
            1,
            &["rename column gamer.name to full_name"],
        )
        .await?;
        assert_eq!(string(db, "full_name", "gamer").await?, "a");
        Ok(())
    })
    .await
}
per_backend!(resume_after_table_rename, super::resume_after_table_rename);

// ---- identity on a column that has a foreign key itself

fn profile(identity: bool) -> TableDef {
    let c = col("id", INT).fk("league", "id");
    table(
        "Profile",
        "profile",
        vec![if identity { c.identity() } else { c.pk() }],
    )
}

scenario!(identity_add_on_a_foreign_key_column, || {
    Scenario::new(
        vec![league_t(false, INT), profile(false)],
        vec![league_t(false, INT), profile(true)],
        Expect::Applies {
            ops: vec!["make profile.id generated by the database"],
            mssql_ops: Some(vec![
                "drop foreign key profile_id_fkey of profile",
                "rebuild table profile",
                "add foreign key profile (id) to league",
            ]),
            mysql_ops: Some(vec![
                "drop foreign key profile_id_fkey of profile",
                "make profile.id generated by the database",
                "add foreign key profile (id) to league",
            ]),
            then: Some(|db, _| {
                Box::pin(async move {
                    assert_eq!(count(db, "profile WHERE id = 1").await?, 1);
                    Ok(())
                })
            }),
        },
    )
    .rows("league", league_rows())
    .rows("profile", vec![vec![("id", i(1))]])
});

// ---- more type, key and precision changes

scenario!(incompatible_type_text_to_int, || Scenario::new(
    vec![named(20)],
    vec![player(vec![col("id", INT).pk(), col("name", INT)])],
    conflicts(&["Destructive"]),
)
.rows("player", two_players()));

scenario!(incompatible_type_text_to_int_on_empty_table, || {
    Scenario::new(
        vec![named(20)],
        vec![player(vec![col("id", INT).pk(), col("name", INT)])],
        applies(&["change type of player.name to integer"]),
    )
    .settings(|s| s.allow_destructive = true)
});

fn keyed_by(cols: &[&'static str]) -> TableDef {
    let mut v = vec![];
    for c in ["id", "code"] {
        let base = col(c, INT);
        v.push(if cols.contains(&c) { base.pk() } else { base });
    }
    player(v)
}

fn id_code_rows() -> Vec<Vec<(&'static str, Param)>> {
    vec![
        vec![("id", i(1)), ("code", i(10))],
        vec![("id", i(2)), ("code", i(20))],
    ]
}

fn two_rows_left<'a>(
    db: &'a TestDb,
    _: usize,
) -> Pin<Box<dyn Future<Output = CanyonResult<()>> + 'a>> {
    Box::pin(async move {
        assert_eq!(count(db, "player").await?, 2);
        assert_eq!(num(db, "code", "player WHERE id = 2").await?, 20);
        Ok(())
    })
}

scenario!(primary_key_drop, || Scenario::new(
    vec![keyed_by(&["id"])],
    vec![keyed_by(&[])],
    applies_then(&["drop primary key of player"], two_rows_left),
)
.settings(|s| s.allow_destructive = true)
.rows("player", id_code_rows()));

scenario!(primary_key_drop_needs_confirmation, || Scenario::new(
    vec![keyed_by(&["id"])],
    vec![keyed_by(&[])],
    conflicts(&["Destructive"]),
)
.rows("player", id_code_rows()));

scenario!(primary_key_change, || Scenario::new(
    vec![keyed_by(&["id"])],
    vec![keyed_by(&["code"])],
    applies_then(
        &[
            "drop primary key of player",
            "add primary key to player (code)"
        ],
        two_rows_left
    ),
)
.settings(|s| s.allow_destructive = true)
.rows("player", id_code_rows()));

scenario!(primary_key_change_duplicates, || Scenario::new(
    vec![keyed_by(&["id"])],
    vec![keyed_by(&["code"])],
    conflicts(&["DuplicateKeyValues"]),
)
.settings(|s| s.allow_destructive = true)
.rows(
    "player",
    vec![
        vec![("id", i(1)), ("code", i(10))],
        vec![("id", i(2)), ("code", i(10))]
    ]
));

fn stamped(precision: u8) -> TableDef {
    player(vec![
        col("id", INT).pk(),
        col("at", SqlType::Timestamp { precision }),
    ])
}

fn stamp() -> Param {
    Param::Timestamp(
        NaiveDate::from_ymd_opt(2024, 5, 6)
            .and_then(|d| d.and_hms_micro_opt(7, 8, 9, 123_000))
            .expect("a valid timestamp"),
    )
}

scenario!(timestamp_precision_narrower_needs_confirmation, || {
    Scenario::new(
        vec![stamped(6)],
        vec![stamped(3)],
        conflicts(&["Destructive"]),
    )
    .rows("player", vec![vec![("id", i(1)), ("at", stamp())]])
});

fn one_stamp<'a>(db: &'a TestDb, _: usize) -> Pin<Box<dyn Future<Output = CanyonResult<()>> + 'a>> {
    Box::pin(async move {
        assert_eq!(count(db, "player WHERE id = 1").await?, 1);
        Ok(())
    })
}

scenario!(timestamp_precision_narrower_allowed, || Scenario::new(
    vec![stamped(6)],
    vec![stamped(3)],
    applies_then(&["change type of player.at to timestamp(3)"], one_stamp),
)
.settings(|s| s.allow_destructive = true)
.rows("player", vec![vec![("id", i(1)), ("at", stamp())]]));

scenario!(timestamp_precision_wider, || Scenario::new(
    vec![stamped(3)],
    vec![stamped(6)],
    applies_then(&["change type of player.at to timestamp(6)"], one_stamp),
)
.rows("player", vec![vec![("id", i(1)), ("at", stamp())]]));

/// Confirmed, but the database can't convert the values: the run fails in that one
/// statement and leaves the table and its rows as they were.
pub(crate) async fn incompatible_type_with_text_rows_fails_cleanly(
    datasource: &str,
) -> CanyonResult<()> {
    TestDb::with(datasource, "incompatible_rows", async |db| {
        let mut all = settings(5);
        all.allow_destructive = true;
        run(db, &all, &[named(20)], None).await?;
        for values in &two_players() {
            insert(db, "player", values).await?;
        }
        let before = read(db, &[]).await;
        let desired = [player(vec![col("id", INT).pk(), col("name", INT)])];
        let err = run(db, &all, &desired, None)
            .await
            .expect_err("'a' is not a number");
        assert!(matches!(err, MigrationError::Apply { .. }), "{err}");
        let after = read(db, &[]).await;
        assert_eq!(after.model, before.model);
        assert_eq!(string(db, "name", "player WHERE id = 2").await?, "b");
        Ok(())
    })
    .await
}
per_backend!(
    incompatible_type_with_text_rows_fails_cleanly,
    super::incompatible_type_with_text_rows_fails_cleanly
);

// ---- 27-32 and the two-pass read: schemas (their names depend on the database)

fn in_schema(schema: &str, entity: &str, name: &str, cols: Vec<Col>) -> TableDef {
    let mut b = TableDef::builder(entity, name).schema(schema);
    for c in cols {
        b = b.column(c.name, c.ty, c.nullable);
        if let Some(generated) = c.pk {
            b = b.primary_key(generated);
        }
        if let Some((t, column)) = c.fk {
            b = b.foreign_key(t, column);
        }
    }
    b.build().expect("a valid table")
}

/// Applies `start` (everything allowed) and the rows, checks the plan for `target`
/// equals `want`, runs it, and checks a second plan is empty.
async fn schema_case(
    db: &TestDb,
    start: &[TableDef],
    rows: &[(&str, Vec<(&str, Param)>)],
    target_models: &[TableDef],
    want: &[String],
) -> CanyonResult<()> {
    let mut all = settings(5);
    all.allow_destructive = true;
    if !start.is_empty() {
        run(db, &all, start, None).await?;
    }
    for (table, values) in rows {
        insert(db, table, values).await?;
    }
    let (outcome, _) = planned(db, &all, target_models, None).await?;
    assert_eq!(outcome, Ok(want.to_vec()), "planned ops");
    run(db, &all, target_models, None).await?;
    let (again, _) = planned(db, &all, target_models, None).await?;
    assert_eq!(
        again,
        Ok(Vec::new()),
        "the database reads back as the target"
    );
    Ok(())
}

async fn schema_exists(db: &TestDb, schema: &str) -> CanyonResult<bool> {
    let from = format!("information_schema.schemata WHERE schema_name = '{schema}'");
    Ok(num(db, "COUNT(*)", &from).await? == 1)
}

fn row_1() -> Vec<(&'static str, Vec<(&'static str, Param)>)> {
    vec![("player", vec![("id", i(1)), ("name", t("a"))])]
}

pub(crate) async fn create_in_schema(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "schema_create", async |db| {
        let s1 = db.schema("s1");
        let want = [
            format!("create schema {s1}"),
            format!("create table {s1}.player"),
        ];
        schema_case(
            db,
            &[],
            &[],
            &[in_schema(&s1, "Player", "player", id_name())],
            &want,
        )
        .await?;
        assert!(schema_exists(db, &s1).await?);
        assert_eq!(count(db, &format!("{s1}.player")).await?, 0);
        Ok(())
    })
    .await
}
per_backend!(create_in_schema, super::create_in_schema);

pub(crate) async fn schema_equal_to_default(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "schema_default", async |db| {
        let default = read(db, &[]).await.default_schema;
        let explicit = in_schema(&default, "Player", "player", id_name());
        schema_case(db, &[player(id_name())], &row_1(), &[explicit], &[]).await?;
        assert_eq!(count(db, "player").await?, 1);
        Ok(())
    })
    .await
}
per_backend!(schema_equal_to_default, super::schema_equal_to_default);

pub(crate) async fn move_between_schemas(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "schema_move", async |db| {
        let s1 = db.schema("s1");
        let want = [
            format!("create schema {s1}"),
            format!("move table player to schema {s1}"),
        ];
        let moved = in_schema(&s1, "Player", "player", id_name());
        schema_case(db, &[player(id_name())], &row_1(), &[moved], &want).await?;
        assert_eq!(count(db, &format!("{s1}.player")).await?, 1, "rows kept");
        Ok(())
    })
    .await
}
per_backend!(move_between_schemas, super::move_between_schemas);

pub(crate) async fn move_and_rename(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "schema_move_rename", async |db| {
        let s1 = db.schema("s1");
        let want = [
            format!("create schema {s1}"),
            format!("move table player to schema {s1}"),
            format!("rename table {s1}.player to gamer"),
        ];
        let moved = in_schema(&s1, "Player", "gamer", id_name());
        schema_case(db, &[player(id_name())], &row_1(), &[moved], &want).await?;
        assert_eq!(count(db, &format!("{s1}.gamer")).await?, 1, "rows kept");
        Ok(())
    })
    .await
}
per_backend!(move_and_rename, super::move_and_rename);

pub(crate) async fn fk_across_schemas(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "schema_fk", async |db| {
        let s1 = db.schema("s1");
        let tournament = in_schema(
            &s1,
            "Tournament",
            "tournament",
            vec![col("id", INT).pk(), col("league", INT).fk("league", "id")],
        );
        let models = [league_t(false, INT), tournament];
        let mut all = settings(5);
        all.allow_destructive = true;
        run(db, &all, &models, None).await?;
        let (again, _) = planned(db, &all, &models, None).await?;
        assert_eq!(again, Ok(Vec::new()));
        let catalog = read(db, &[Ident::new(&s1).expect("a name")]).await;
        let found = catalog
            .model
            .tables
            .values()
            .find(|t| t.name.to_string().ends_with("tournament"))
            .expect("tournament is read");
        assert_eq!(
            found.foreign_keys.len(),
            1,
            "the foreign key is introspected"
        );
        assert_eq!(found.foreign_keys[0].ref_table.to_string(), "league");
        Ok(())
    })
    .await
}
per_backend!(fk_across_schemas, super::fk_across_schemas);

/// The target does not name `s1` any more: only the snapshot does.
pub(crate) async fn schema_never_dropped(datasource: &str) -> CanyonResult<()> {
    TestDb::with(datasource, "schema_kept", async |db| {
        let s1 = db.schema("s1");
        let start = in_schema(&s1, "Player", "player", id_name());
        let want = [format!("move table {s1}.player to the default schema")];
        let table = format!("{s1}.player");
        let rows = [(table.as_str(), vec![("id", i(1)), ("name", t("a"))])];
        schema_case(db, &[start], &rows, &[player(id_name())], &want).await?;
        assert!(schema_exists(db, &s1).await?, "the schema stays");
        assert_eq!(count(db, "player").await?, 1, "rows kept");
        Ok(())
    })
    .await
}
per_backend!(schema_never_dropped, super::schema_never_dropped);
