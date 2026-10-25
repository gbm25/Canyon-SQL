use crate::model::{Ident, QualifiedName};
use crate::op::{ConfirmKey, Op, Param};
use std::collections::{BTreeMap, BTreeSet};

/// The answers that turn conflicts into a plan: from `canyon.toml` (`run()`) or from the user (`interactive()`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Decisions {
    pub allow_destructive: bool,
    /// `Op::confirm_key()` of each confirmed op.
    pub confirmed: BTreeSet<ConfirmKey>,
    /// Old name to `Some(new name)`, or `None` when it is not a rename.
    pub table_renames: BTreeMap<QualifiedName, Option<QualifiedName>>,
    /// (table, old column) to `Some(new name)`, or `None` when it is not a rename.
    pub column_renames: BTreeMap<(QualifiedName, Ident), Option<Ident>>,
    pub fill_values: BTreeMap<(QualifiedName, Ident), Param>,
    pub tighten: BTreeSet<(QualifiedName, Ident)>,
    pub rebuild_confirm_rows: u64,
}

impl Decisions {
    pub const DEFAULT_REBUILD_CONFIRM_ROWS: u64 = 100_000;

    pub fn from_config(allow_destructive: bool, rebuild_confirm_rows: u64) -> Self {
        Self {
            allow_destructive,
            confirmed: BTreeSet::new(),
            table_renames: BTreeMap::new(),
            column_renames: BTreeMap::new(),
            fill_values: BTreeMap::new(),
            tighten: BTreeSet::new(),
            rebuild_confirm_rows,
        }
    }

    pub fn allows(&self, op: &Op) -> bool {
        self.allow_destructive || self.confirmed.contains(&op.confirm_key())
    }
}

impl Default for Decisions {
    fn default() -> Self {
        Self::from_config(false, Self::DEFAULT_REBUILD_CONFIRM_ROWS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_ops_need_the_flag_or_a_confirmation() {
        let op = Op::DropTable(QualifiedName::parse("t").unwrap());
        let mut d = Decisions::from_config(false, Decisions::DEFAULT_REBUILD_CONFIRM_ROWS);
        assert!(!d.allows(&op));
        d.confirmed.insert(op.confirm_key());
        assert!(d.allows(&op));
        assert!(Decisions::from_config(true, 0).allows(&op));
    }

    #[test]
    fn default_keeps_the_rebuild_threshold() {
        let d = Decisions::default();
        assert!(!d.allow_destructive);
        assert_eq!(
            d.rebuild_confirm_rows,
            Decisions::DEFAULT_REBUILD_CONFIRM_ROWS
        );
    }
}
