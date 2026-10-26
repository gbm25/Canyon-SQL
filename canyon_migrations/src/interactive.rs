//! Conflicts turned into questions for `migrate`, and the answers into decisions.

use crate::memory::Declined;
use crate::model::{ColumnDef, Ident, Logical, QualifiedName};
use crate::op::ConfirmKey;
use crate::planning::{Conflict, Decisions, Info, sql_type_words, type_words};
use canyon_core::types::SqlType;
use std::io::{BufRead, Write};

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Question {
    TableRename {
        datasource: String,
        old: QualifiedName,
        candidates: Vec<QualifiedName>,
    },
    ColumnRename {
        datasource: String,
        table: QualifiedName,
        removed: ColumnDef,
        added: Vec<ColumnDef>,
    },
    Confirm {
        datasource: String,
        key: ConfirmKey,
        what: String,
    },
    Tighten {
        datasource: String,
        table: QualifiedName,
        column: Ident,
        from: Logical,
        to: SqlType,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answer {
    Choose(usize),
    Yes,
    No,
    Abort,
}

/// Asks one question. It blocks, so the runner calls it from `spawn_blocking` with an
/// owned `Box<dyn Prompt + Send>`.
pub trait Prompt: Send {
    fn ask(&mut self, question: &Question) -> Answer;

    /// Says why the last answer can't be used, before the question is asked again.
    fn tell(&mut self, _reason: &str) {}
}

/// What to do after an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Abort,
}

/// The questions for the conflicts and the tighten offers.
///
/// `declined` are the offers answered "no" before: they are not asked again while the
/// column stays as it was.
///
/// `Err` has the conflicts no answer can fix, and nothing is asked then. Canyon never
/// asks for a value to put in existing rows: a NOT NULL column over rows with no value
/// is one of them, unless a rename is still open: its answer may remove the column, so
/// the conflict waits for the plan made with it.
pub fn questions(
    datasource: &str,
    conflicts: &[Conflict],
    infos: &[Info],
    declined: &[Declined],
) -> Result<Vec<Question>, Vec<Conflict>> {
    let ds = || datasource.to_owned();
    // A rename answer may take a NOT NULL column out of the plan: those conflicts wait
    // for the plan made with the answers
    let renames_open = conflicts.iter().any(|c| {
        matches!(
            c,
            Conflict::AmbiguousTableRename { .. } | Conflict::AmbiguousColumnRename { .. }
        )
    });
    let mut out = Vec::new();
    let mut unfixable = Vec::new();
    for conflict in conflicts {
        match conflict {
            Conflict::AmbiguousTableRename { old, candidates } => {
                out.push(Question::TableRename {
                    datasource: ds(),
                    old: old.clone(),
                    candidates: candidates.clone(),
                });
            }
            Conflict::AmbiguousColumnRename {
                table,
                removed,
                added,
            } => {
                for column in removed {
                    out.push(Question::ColumnRename {
                        datasource: ds(),
                        table: table.clone(),
                        removed: column.clone(),
                        added: added.clone(),
                    });
                }
            }
            Conflict::Destructive { op } => out.push(Question::Confirm {
                datasource: ds(),
                key: op.confirm_key(),
                what: op.describe(),
            }),
            Conflict::LargeRebuild {
                table,
                estimated_rows,
            } => out.push(Question::Confirm {
                datasource: ds(),
                key: ConfirmKey::rebuild(table),
                what: format!("rebuild table {table} (about {estimated_rows} rows)"),
            }),
            Conflict::NotNullOnTableWithRows { .. } | Conflict::NullsBlockNotNull { .. }
                if renames_open => {}
            // Nothing the user can answer: the model, the plan or the rows have to change
            Conflict::InvalidModel(_)
            | Conflict::RenameCycle { .. }
            | Conflict::RenameTargetTaken { .. }
            | Conflict::ColumnRenameTargetTaken { .. }
            | Conflict::ValuesOutOfRange { .. }
            | Conflict::ValuesTooLong { .. }
            | Conflict::DuplicateKeyValues { .. }
            | Conflict::OrphanRows { .. }
            | Conflict::GeneratedColumn { .. }
            | Conflict::GeneratedColumnDependency { .. }
            | Conflict::DependedOnByGenerated { .. }
            | Conflict::NotNullOnTableWithRows { .. }
            | Conflict::NullsBlockNotNull { .. }
            | Conflict::FillValueType { .. }
            | Conflict::FillByHand { .. }
            | Conflict::Internal(_) => unfixable.push(conflict.clone()),
        }
    }
    if !unfixable.is_empty() {
        return Err(unfixable);
    }
    for info in infos {
        match info {
            Info::Tightenable {
                table,
                column,
                column_type,
                canonical,
            } => {
                let asked_before = declined.iter().any(|d| {
                    &d.table == table && &d.column == column && d.column_type == *canonical
                });
                if !asked_before {
                    out.push(Question::Tighten {
                        datasource: ds(),
                        table: table.clone(),
                        column: column.clone(),
                        from: column_type.clone(),
                        to: *canonical,
                    });
                }
            }
            Info::Unmanaged { .. } => {}
        }
    }
    Ok(out)
}

/// The question as it stands after the earlier answers, or `None` when they settled it.
///
/// A new name chosen for one removed column or table isn't offered for another;
/// when nothing is left, the answer is "not a rename" and it is recorded here.
pub fn still_open(question: &Question, d: &mut Decisions) -> Option<Question> {
    match question {
        Question::TableRename {
            datasource,
            old,
            candidates,
        } => {
            let left: Vec<QualifiedName> = (candidates.iter())
                .filter(|c| !d.table_renames.values().flatten().any(|taken| taken == *c))
                .cloned()
                .collect();
            if left.is_empty() {
                d.table_renames.insert(old.clone(), None);
                return None;
            }
            Some(Question::TableRename {
                datasource: datasource.clone(),
                old: old.clone(),
                candidates: left,
            })
        }
        Question::ColumnRename {
            datasource,
            table,
            removed,
            added,
        } => {
            let left: Vec<ColumnDef> = (added.iter())
                .filter(|a| {
                    !(d.column_renames.iter())
                        .any(|((t, _), new)| t == table && new.as_ref() == Some(&a.name))
                })
                .cloned()
                .collect();
            if left.is_empty() {
                d.column_renames
                    .insert((table.clone(), removed.name.clone()), None);
                return None;
            }
            Some(Question::ColumnRename {
                datasource: datasource.clone(),
                table: table.clone(),
                removed: removed.clone(),
                added: left,
            })
        }
        other => Some(other.clone()),
    }
}

/// Records the answer. `Err` has the reason when it can't be used: ask again.
/// A "no" to a tighten offer is added to `declined`, for the snapshot.
pub fn apply_answer(
    question: &Question,
    answer: &Answer,
    d: &mut Decisions,
    declined: &mut Vec<Declined>,
) -> Result<Flow, String> {
    match (question, answer) {
        (_, Answer::Abort) => return Ok(Flow::Abort),
        (
            Question::TableRename {
                old, candidates, ..
            },
            Answer::Choose(i),
        ) => {
            let chosen = match *i {
                i if i < candidates.len() => Some(candidates[i].clone()),
                i if i == candidates.len() => None,
                _ => return Err(format!("there is no option {}", i.saturating_add(1))),
            };
            d.table_renames.insert(old.clone(), chosen);
        }
        (
            Question::ColumnRename {
                table,
                removed,
                added,
                ..
            },
            Answer::Choose(i),
        ) => {
            let chosen = match *i {
                i if i < added.len() => Some(added[i].name.clone()),
                i if i == added.len() => None,
                _ => return Err(format!("there is no option {}", i.saturating_add(1))),
            };
            d.column_renames
                .insert((table.clone(), removed.name.clone()), chosen);
        }
        (Question::Confirm { key, .. }, Answer::Yes) => {
            d.confirmed.insert(key.clone());
        }
        (Question::Confirm { .. }, Answer::No) => {}
        (Question::Tighten { table, column, .. }, Answer::Yes) => {
            d.tighten.insert((table.clone(), column.clone()));
        }
        (
            Question::Tighten {
                table, column, to, ..
            },
            Answer::No,
        ) => {
            let decline = Declined {
                table: table.clone(),
                column: column.clone(),
                column_type: *to,
            };
            if !declined.contains(&decline) {
                declined.push(decline);
            }
        }
        _ => return Err("that doesn't answer the question".into()),
    }
    Ok(Flow::Continue)
}

/// How a question reads on the terminal, without the line for the answer.
pub fn render(question: &Question) -> String {
    let mut out = String::new();
    match question {
        Question::TableRename {
            datasource,
            old,
            candidates,
        } => {
            out += &format!("{datasource}: table `{old}` is gone. Was it renamed?\n");
            let options = (candidates.iter())
                .map(|c| format!("rename {old} → {c} (keeps the data)"))
                .chain([format!("not a rename: leave `{old}` as it is")]);
            push_options(&mut out, options);
        }
        Question::ColumnRename {
            datasource,
            table,
            removed,
            added,
        } => {
            let new: Vec<String> = (added.iter())
                .map(|a| format!("`{}` ({})", a.name, type_words(&a.ty)))
                .collect();
            out += &format!(
                "{datasource} / {table}: column `{}` ({}) was removed and {} added.\n",
                removed.name,
                type_words(&removed.ty),
                new.join(", ")
            );
            let options = (added.iter())
                .map(|a| format!("rename {} → {} (converts the data)", removed.name, a.name))
                .chain([format!(
                    "drop {} (its data is lost), the added columns start empty",
                    removed.name
                )]);
            push_options(&mut out, options);
        }
        Question::Confirm {
            datasource, what, ..
        } => {
            out += &format!("{datasource}: {what}. Go ahead? [y/N, a to abort] ");
        }
        Question::Tighten {
            datasource,
            table,
            column,
            from,
            to,
        } => {
            out += &format!(
                "{datasource} / {table}.{column} is {}, the field needs {}. Tighten it? [y/N, a to abort] ",
                type_words(from),
                sql_type_words(*to)
            );
        }
    }
    out
}

fn push_options(out: &mut String, options: impl Iterator<Item = String>) {
    for (i, option) in options.enumerate() {
        out.push_str(&format!("  [{}] {option}\n", i + 1));
    }
    out.push_str("  [a] abort\n> ");
}

/// How many options a choice question has, "not a rename" included.
fn option_count(question: &Question) -> usize {
    match question {
        Question::TableRename { candidates, .. } => candidates.len() + 1,
        Question::ColumnRename { added, .. } => added.len() + 1,
        _ => 0,
    }
}

/// Asks on `input` and `output`; a closed input aborts.
pub fn ask_on(input: &mut impl BufRead, output: &mut impl Write, question: &Question) -> Answer {
    let _ = write!(output, "\n{}", render(question));
    let _ = output.flush();
    loop {
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => return Answer::Abort,
            Ok(_) => {}
        }
        let line = line.trim();
        match question {
            Question::Confirm { .. } | Question::Tighten { .. } => {
                return match line.to_ascii_lowercase().as_str() {
                    "y" | "yes" => Answer::Yes,
                    "a" | "abort" => Answer::Abort,
                    _ => Answer::No,
                };
            }
            Question::TableRename { .. } | Question::ColumnRename { .. } => {
                if line.eq_ignore_ascii_case("a") {
                    return Answer::Abort;
                }
                match line.parse::<usize>() {
                    Ok(n) if (1..=option_count(question)).contains(&n) => {
                        return Answer::Choose(n - 1);
                    }
                    _ => {
                        let _ = write!(output, "  pick a number from the list, or a\n> ");
                        let _ = output.flush();
                    }
                }
            }
        }
    }
}

/// Reads answers from the terminal.
pub struct StdinPrompt;

impl Prompt for StdinPrompt {
    fn ask(&mut self, question: &Question) -> Answer {
        ask_on(
            &mut std::io::stdin().lock(),
            &mut std::io::stdout(),
            question,
        )
    }

    fn tell(&mut self, reason: &str) {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "  {reason}");
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::fixtures::{id, q};
    use crate::model::{NativeType, TableDef};
    use crate::op::Op;
    use crate::planning::ModelIssue;
    use std::io::Cursor;

    /// Answers in order; remembers what it was asked.
    struct Scripted {
        answers: std::vec::IntoIter<Answer>,
        asked: Vec<Question>,
    }

    impl Prompt for Scripted {
        fn ask(&mut self, question: &Question) -> Answer {
            self.asked.push(question.clone());
            self.answers.next().unwrap_or(Answer::Abort)
        }
    }

    fn scripted(answers: Vec<Answer>) -> Scripted {
        Scripted {
            answers: answers.into_iter(),
            asked: Vec::new(),
        }
    }

    /// The loop a runner does: ask, apply, ask again on a bad answer.
    fn run(
        qs: &[Question],
        prompt: &mut Scripted,
        d: &mut Decisions,
        declined: &mut Vec<Declined>,
    ) -> Flow {
        for question in qs {
            let Some(question) = still_open(question, d) else {
                continue;
            };
            loop {
                let answer = prompt.ask(&question);
                match apply_answer(&question, &answer, d, declined) {
                    Ok(Flow::Continue) => break,
                    Ok(Flow::Abort) => return Flow::Abort,
                    Err(_) => {}
                }
            }
        }
        Flow::Continue
    }

    fn col(name: &str, ty: SqlType) -> ColumnDef {
        let t = TableDef::builder("T", "t")
            .column(name, ty, true)
            .build()
            .unwrap();
        t.columns[0].clone()
    }

    fn ask_all(conflicts: &[Conflict]) -> Result<Vec<Question>, Vec<Conflict>> {
        questions("pg", conflicts, &[], &[])
    }

    fn drop_column() -> Op {
        Op::DropColumn {
            table: q("player"),
            column: id("age"),
        }
    }

    fn all_questions() -> Vec<Question> {
        vec![
            Question::TableRename {
                datasource: "pg".into(),
                old: q("player"),
                candidates: vec![q("athlete"), q("sport.athlete")],
            },
            Question::ColumnRename {
                datasource: "pg".into(),
                table: q("player"),
                removed: col("age", SqlType::Int),
                added: vec![
                    col("birth_date", SqlType::Date),
                    col("nick", SqlType::Text { max_len: Some(20) }),
                ],
            },
            Question::Confirm {
                datasource: "pg".into(),
                key: drop_column().confirm_key(),
                what: drop_column().describe(),
            },
            Question::Confirm {
                datasource: "pg".into(),
                key: ConfirmKey::rebuild(&q("player")),
                what: "rebuild table player (about 250000 rows)".into(),
            },
            Question::Tighten {
                datasource: "pg".into(),
                table: q("player"),
                column: id("name"),
                from: Logical::Known(SqlType::Text { max_len: None }),
                to: SqlType::Text { max_len: Some(50) },
            },
        ]
    }

    #[test]
    fn every_question_reads_plainly() {
        let text: Vec<String> = all_questions().iter().map(render).collect();
        insta::assert_snapshot!(text.join("\n---\n"));
    }

    #[test]
    fn conflicts_become_questions() {
        let conflicts = [
            Conflict::AmbiguousTableRename {
                old: q("a"),
                candidates: vec![q("b"), q("c")],
            },
            Conflict::AmbiguousColumnRename {
                table: q("player"),
                removed: vec![col("x", SqlType::Int), col("y", SqlType::Int)],
                added: vec![col("z", SqlType::Int)],
            },
            Conflict::Destructive { op: drop_column() },
            Conflict::LargeRebuild {
                table: q("player"),
                estimated_rows: 7,
            },
        ];
        let qs = ask_all(&conflicts).unwrap();
        assert!(
            matches!(&qs[0], Question::TableRename { old, candidates, .. } if *old == q("a") && candidates.len() == 2)
        );
        assert!(
            matches!(&qs[1], Question::ColumnRename { removed, .. } if removed.name == id("x"))
        );
        assert!(
            matches!(&qs[2], Question::ColumnRename { removed, .. } if removed.name == id("y"))
        );
        assert!(matches!(&qs[3], Question::Confirm { key, what, .. }
            if *key == drop_column().confirm_key() && what == "drop column player.age"));
        assert!(matches!(&qs[4], Question::Confirm { key, what, .. }
            if *key == ConfirmKey::rebuild(&q("player")) && what == "rebuild table player (about 7 rows)"));
        assert_eq!(qs.len(), 5);
    }

    #[test]
    fn unfixable_conflicts_ask_nothing() {
        let unfixable = vec![
            Conflict::InvalidModel(ModelIssue::SchemaCase {
                schema: id("PUBLIC"),
                default: "public".into(),
            }),
            Conflict::InvalidModel(ModelIssue::NullableKey {
                table: q("player"),
                column: id("id"),
            }),
            Conflict::RenameCycle {
                tables: vec![q("a"), q("b")],
            },
            Conflict::RenameTargetTaken {
                old: q("a"),
                new: q("b"),
            },
            Conflict::ColumnRenameTargetTaken {
                table: q("a"),
                old: id("x"),
                new: id("y"),
            },
            Conflict::ValuesOutOfRange {
                table: q("a"),
                column: id("x"),
                to: SqlType::SmallInt,
            },
            Conflict::ValuesTooLong {
                table: q("a"),
                column: id("x"),
                max_len: 3,
            },
            Conflict::DuplicateKeyValues {
                table: q("a"),
                columns: vec![id("x")],
            },
            Conflict::OrphanRows {
                table: q("a"),
                columns: vec![id("x")],
                target: q("b"),
            },
            Conflict::GeneratedColumn {
                table: q("a"),
                column: id("x"),
                op: "drop column a.x".into(),
            },
            // Canyon never asks for a value for the existing rows
            Conflict::NotNullOnTableWithRows {
                table: q("a"),
                column: id("x"),
            },
            Conflict::NullsBlockNotNull {
                table: q("a"),
                column: id("x"),
            },
            Conflict::FillValueType {
                table: q("a"),
                column: id("x"),
                expected: Logical::Known(SqlType::SmallInt),
            },
            Conflict::GeneratedColumnDependency {
                table: q("a"),
                column: id("x"),
                generated: vec![id("y")],
            },
            Conflict::DependedOnByGenerated {
                table: q("a"),
                column: id("x"),
                generated: vec![id("y")],
            },
            Conflict::FillByHand {
                table: q("a"),
                column: id("x"),
                column_type: Logical::Other(NativeType::named("uuid", "uuid")),
            },
            Conflict::Internal("bug".into()),
        ];
        for c in &unfixable {
            assert_eq!(
                ask_all(std::slice::from_ref(c)).unwrap_err(),
                std::slice::from_ref(c)
            );
        }
        // Mixed: only the unfixable ones come back
        let mut mixed = vec![Conflict::Destructive { op: drop_column() }];
        mixed.extend(unfixable.clone());
        assert_eq!(ask_all(&mixed).unwrap_err(), unfixable);
    }

    fn tightenable(column: &str) -> Info {
        Info::Tightenable {
            table: q("player"),
            column: id(column),
            column_type: Logical::Known(SqlType::Text { max_len: None }),
            canonical: SqlType::Text { max_len: Some(50) },
        }
    }

    #[test]
    fn tighten_offers_become_questions_unless_declined_before() {
        let declined = [Declined {
            table: q("player"),
            column: id("name"),
            column_type: SqlType::Text { max_len: Some(50) },
        }];
        let infos = [
            tightenable("name"),
            tightenable("nick"),
            Info::Unmanaged { table: q("old") },
        ];
        let qs = questions("pg", &[], &infos, &declined).unwrap();
        assert!(matches!(&qs[..], [Question::Tighten { column, to, .. }]
            if *column == id("nick") && *to == SqlType::Text { max_len: Some(50) }));
        // A decline of another canonical type doesn't count
        let other = [Declined {
            column_type: SqlType::Text { max_len: Some(10) },
            ..declined[0].clone()
        }];
        let qs = questions("pg", &[], &infos[..1], &other).unwrap();
        assert_eq!(qs.len(), 1);
    }

    #[test]
    fn not_null_conflicts_wait_for_an_open_rename() {
        let rename = Conflict::AmbiguousColumnRename {
            table: q("player"),
            removed: vec![col("x", SqlType::Int)],
            added: vec![col("y", SqlType::Int), col("z", SqlType::Int)],
        };
        let not_null = [
            Conflict::NotNullOnTableWithRows {
                table: q("player"),
                column: id("z"),
            },
            Conflict::NullsBlockNotNull {
                table: q("player"),
                column: id("age"),
            },
        ];
        let mut conflicts = vec![rename];
        conflicts.extend(not_null.clone());
        let qs = ask_all(&conflicts).unwrap();
        assert!(
            matches!(&qs[..], [Question::ColumnRename { removed, .. }] if removed.name == id("x"))
        );
        // Without the rename they stop the run
        assert_eq!(ask_all(&not_null).unwrap_err(), not_null);
        // Other unfixable conflicts don't wait
        conflicts.push(Conflict::Internal("bug".into()));
        assert_eq!(
            ask_all(&conflicts).unwrap_err(),
            [Conflict::Internal("bug".into())]
        );
    }

    #[test]
    fn unfixable_conflicts_come_before_tighten_offers() {
        let c = Conflict::Internal("x".into());
        let r = questions("pg", std::slice::from_ref(&c), &[tightenable("name")], &[]);
        assert_eq!(r.unwrap_err(), [c]);
    }

    fn decide(question: &Question, answer: Answer) -> (Decisions, Vec<Declined>, Flow) {
        let mut d = Decisions::default();
        let mut declined = Vec::new();
        let flow = apply_answer(question, &answer, &mut d, &mut declined).unwrap();
        (d, declined, flow)
    }

    #[test]
    fn table_rename_answers() {
        let qs = all_questions();
        let (d, ..) = decide(&qs[0], Answer::Choose(1));
        assert_eq!(d.table_renames[&q("player")], Some(q("sport.athlete")));
        let (d, ..) = decide(&qs[0], Answer::Choose(2));
        assert_eq!(d.table_renames[&q("player")], None);
        let mut d = Decisions::default();
        assert!(apply_answer(&qs[0], &Answer::Choose(3), &mut d, &mut vec![]).is_err());
        assert!(d.table_renames.is_empty());
    }

    #[test]
    fn column_rename_answers() {
        let qs = all_questions();
        let key = (q("player"), id("age"));
        let (d, ..) = decide(&qs[1], Answer::Choose(1));
        assert_eq!(d.column_renames[&key], Some(id("nick")));
        let (d, ..) = decide(&qs[1], Answer::Choose(2));
        assert_eq!(d.column_renames[&key], None);
        let mut d = Decisions::default();
        assert!(apply_answer(&qs[1], &Answer::Choose(9), &mut d, &mut vec![]).is_err());
    }

    #[test]
    fn the_largest_choice_is_refused_without_overflow() {
        let qs = all_questions();
        let mut d = Decisions::default();
        for q in &qs[..2] {
            let err = apply_answer(q, &Answer::Choose(usize::MAX), &mut d, &mut vec![])
                .expect_err("no such option");
            assert_eq!(err, format!("there is no option {}", usize::MAX));
        }
    }

    #[test]
    fn confirmations_store_the_key() {
        let qs = all_questions();
        let (d, ..) = decide(&qs[2], Answer::Yes);
        assert!(d.allows(&drop_column()));
        assert_eq!(d.confirmed.len(), 1);
        let (d, ..) = decide(&qs[2], Answer::No);
        assert!(d.confirmed.is_empty());
        let (d, ..) = decide(&qs[3], Answer::Yes);
        assert!(d.confirmed.contains(&ConfirmKey::rebuild(&q("player"))));
    }

    #[test]
    fn a_confirmation_is_the_ops_own_key() {
        let op = Op::DropColumn {
            table: q("public.player"),
            column: id("age"),
        };
        let question = Question::Confirm {
            datasource: "pg".into(),
            key: op.confirm_key(),
            what: op.describe(),
        };
        let (d, ..) = decide(&question, Answer::Yes);
        assert!(d.allows(&op));
        assert_eq!(d.confirmed.iter().next(), Some(&op.confirm_key()));
    }

    #[test]
    fn tighten_answers() {
        let qs = all_questions();
        let (d, declined, _) = decide(&qs[4], Answer::Yes);
        assert!(d.tighten.contains(&(q("player"), id("name"))));
        assert!(declined.is_empty());
        let (d, declined, _) = decide(&qs[4], Answer::No);
        assert!(d.tighten.is_empty());
        assert_eq!(
            declined,
            [Declined {
                table: q("player"),
                column: id("name"),
                column_type: SqlType::Text { max_len: Some(50) },
            }]
        );
        // Declining twice keeps one
        let mut d = Decisions::default();
        let mut declined = declined;
        apply_answer(&qs[4], &Answer::No, &mut d, &mut declined).unwrap();
        assert_eq!(declined.len(), 1);
    }

    #[test]
    fn abort_stops_and_wrong_kinds_are_refused() {
        for question in all_questions() {
            let (d, declined, flow) = decide(&question, Answer::Abort);
            assert_eq!(flow, Flow::Abort);
            assert!(d.confirmed.is_empty() && d.tighten.is_empty() && declined.is_empty());
        }
        let qs = all_questions();
        let mut d = Decisions::default();
        assert!(apply_answer(&qs[0], &Answer::Yes, &mut d, &mut vec![]).is_err());
        assert!(apply_answer(&qs[2], &Answer::Choose(0), &mut d, &mut vec![]).is_err());
        assert!(apply_answer(&qs[4], &Answer::Choose(0), &mut d, &mut vec![]).is_err());
    }

    #[test]
    fn a_chosen_column_is_not_offered_again() {
        let removed = |n: &str| col(n, SqlType::Int);
        let added = vec![col("a", SqlType::Int), col("b", SqlType::Int)];
        let ask = |n: &str| Question::ColumnRename {
            datasource: "pg".into(),
            table: q("player"),
            removed: removed(n),
            added: added.clone(),
        };
        let qs = [ask("x"), ask("y")];
        let mut p = scripted(vec![Answer::Choose(0), Answer::Choose(0)]);
        let mut d = Decisions::default();
        assert_eq!(run(&qs, &mut p, &mut d, &mut vec![]), Flow::Continue);
        // The second question only had `b` left
        assert!(matches!(&p.asked[1], Question::ColumnRename { added, .. }
            if added.len() == 1 && added[0].name == id("b")));
        assert_eq!(d.column_renames[&(q("player"), id("x"))], Some(id("a")));
        assert_eq!(d.column_renames[&(q("player"), id("y"))], Some(id("b")));

        // Nothing left: not asked, recorded as not a rename
        let mut p = scripted(vec![Answer::Choose(0), Answer::Choose(0)]);
        let ask3 = |n: &str| Question::ColumnRename {
            datasource: "pg".into(),
            table: q("player"),
            removed: removed(n),
            added: vec![col("a", SqlType::Int)],
        };
        let mut d = Decisions::default();
        run(&[ask3("x"), ask3("y")], &mut p, &mut d, &mut vec![]);
        assert_eq!(p.asked.len(), 1);
        assert_eq!(d.column_renames[&(q("player"), id("y"))], None);
    }

    #[test]
    fn a_chosen_table_is_not_offered_again() {
        let ask = |old: &str| Question::TableRename {
            datasource: "pg".into(),
            old: q(old),
            candidates: vec![q("new")],
        };
        let mut p = scripted(vec![Answer::Choose(0)]);
        let mut d = Decisions::default();
        run(&[ask("a"), ask("b")], &mut p, &mut d, &mut vec![]);
        assert_eq!(p.asked.len(), 1);
        assert_eq!(d.table_renames[&q("a")], Some(q("new")));
        assert_eq!(d.table_renames[&q("b")], None);
    }

    #[test]
    fn a_scripted_run_builds_the_decisions() {
        let qs = all_questions();
        let mut p = scripted(vec![
            Answer::Choose(9),
            Answer::Choose(0),
            Answer::Choose(2),
            Answer::Yes,
            Answer::No,
            Answer::No,
        ]);
        let mut d = Decisions::default();
        let mut declined = Vec::new();
        assert_eq!(run(&qs, &mut p, &mut d, &mut declined), Flow::Continue);
        assert_eq!(p.asked.len(), 6); // the bad choice was asked twice
        assert_eq!(d.table_renames[&q("player")], Some(q("athlete")));
        assert_eq!(d.column_renames[&(q("player"), id("age"))], None);
        assert!(d.allows(&drop_column()));
        assert!(!d.confirmed.contains(&ConfirmKey::rebuild(&q("player"))));
        assert!(d.fill_values.is_empty());
        assert_eq!(declined.len(), 1);

        let mut p = scripted(vec![Answer::Abort]);
        let mut d = Decisions::default();
        assert_eq!(run(&qs, &mut p, &mut d, &mut vec![]), Flow::Abort);
    }

    #[test]
    fn terminal_answers() {
        let qs = all_questions();
        let on = |input: &str, q: &Question| {
            let mut out = Vec::new();
            let a = ask_on(&mut Cursor::new(input.to_owned()), &mut out, q);
            (a, String::from_utf8(out).unwrap())
        };
        assert_eq!(on("2\n", &qs[0]).0, Answer::Choose(1));
        assert_eq!(on("3\n", &qs[0]).0, Answer::Choose(2));
        assert_eq!(on("a\n", &qs[0]).0, Answer::Abort);
        // A number outside the list asks again
        let (answer, shown) = on("9\nx\n1\n", &qs[1]);
        assert_eq!(answer, Answer::Choose(0));
        assert!(shown.contains("pick a number"));
        assert_eq!(on("y\n", &qs[2]).0, Answer::Yes);
        assert_eq!(on("\n", &qs[2]).0, Answer::No);
        assert_eq!(on("a\n", &qs[3]).0, Answer::Abort);
        assert_eq!(on("YES\n", &qs[4]).0, Answer::Yes);
        // A closed input aborts instead of looping
        for q in &qs {
            assert_eq!(on("", q).0, Answer::Abort);
        }
        assert_eq!(on("9\n", &qs[0]).0, Answer::Abort);
    }
}
