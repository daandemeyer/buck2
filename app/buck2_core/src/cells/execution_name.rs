/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;

use allocative::Allocative;
use pagable::Pagable;

use crate::cells::name::CellName;

/// Maps each cell to the name it contributes to its canonical execution path, decoupling action
/// identity from the label the workspace happens to mount the cell under. Renaming a cell while
/// pinning its execution name keeps every action digest, and two workspaces that mount one
/// repository under different names can still share an action cache.
///
/// The execution name defaults to the cell name, so only cells given a different one are stored:
/// the map is empty in a workspace that configures none, which is the common case and the only
/// one `physical` mode ever needs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Allocative, Pagable)]
pub struct CellExecutionNames {
    renamed: BTreeMap<CellName, String>,
}

impl CellExecutionNames {
    /// Every cell keeps its own name.
    pub fn identity() -> CellExecutionNames {
        CellExecutionNames::default()
    }

    /// `names` must list every configured cell exactly once. Both halves matter: uniqueness of the
    /// names is only decidable against the full set, since an override may collide with a cell that
    /// kept its own name, and a repeated cell would leave it reachable by two execution paths.
    pub fn new(
        names: impl IntoIterator<Item = (CellName, String)>,
    ) -> buck2_error::Result<CellExecutionNames> {
        let mut assigned: BTreeMap<CellName, String> = BTreeMap::new();
        let mut owner_of_name: BTreeMap<String, CellName> = BTreeMap::new();
        for (cell, name) in names {
            validate_execution_name(cell, &name)?;
            if let Some(previous) = assigned.insert(cell, name.clone()) {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Cell `{cell}` was assigned execution names `{previous}` and `{name}`; a cell must have exactly one so it has one execution path",
                ));
            }
            if let Some(previous) = owner_of_name.insert(name.clone(), cell) {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Cells `{previous}` and `{cell}` both use execution name `{name}`; execution names must be unique so a canonical execution path names one cell",
                ));
            }
        }
        Ok(CellExecutionNames {
            renamed: assigned
                .into_iter()
                .filter(|(cell, name)| name != cell.as_str())
                .collect(),
        })
    }

    pub fn execution_name(&self, cell: CellName) -> &str {
        match self.renamed.get(&cell) {
            Some(name) => name,
            None => cell.as_str(),
        }
    }

    /// Inverse of [`Self::execution_name`]. Returns `None` for a name no cell uses, including the
    /// name of a cell that was given a different execution name: that spelling is retired, and
    /// accepting it would give one cell two execution paths.
    ///
    /// The scan is over renamed cells only, so it is empty work unless the workspace configures
    /// execution names, which matters because this is on the path of every source resolution.
    pub fn cell_for_execution_name(&self, name: &str) -> Option<CellName> {
        if let Some((&cell, _)) = self.renamed.iter().find(|(_, renamed)| *renamed == name) {
            return Some(cell);
        }
        let cell = CellName::unchecked_new(name).ok()?;
        if self.renamed.contains_key(&cell) {
            return None;
        }
        Some(cell)
    }
}

fn validate_execution_name(cell: CellName, name: &str) -> buck2_error::Result<()> {
    if name.is_empty() {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Cell `{cell}` has an empty execution name",
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| *c == '/' || *c == '\\' || c.is_control())
    {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Input,
            "Cell `{cell}` has execution name `{name}` containing `{}`; execution names name a single path component",
            bad.escape_debug(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(name: &str) -> CellName {
        CellName::testing_new(name)
    }

    fn names(entries: &[(&str, &str)]) -> buck2_error::Result<CellExecutionNames> {
        CellExecutionNames::new(
            entries
                .iter()
                .map(|(c, n)| (cell(c), (*n).to_owned()))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn identity_names_round_trip() -> buck2_error::Result<()> {
        let names = names(&[("root", "root"), ("sample", "sample")])?;
        assert_eq!(names.execution_name(cell("sample")), "sample");
        assert_eq!(
            names.cell_for_execution_name("sample"),
            Some(cell("sample"))
        );
        // An unconfigured name still decodes here; `ArtifactFs` rejects it against the resolver.
        assert_eq!(names.cell_for_execution_name("other"), Some(cell("other")));
        Ok(())
    }

    #[test]
    fn renamed_cell_keeps_its_execution_name() -> buck2_error::Result<()> {
        let names = names(&[("root", "root"), ("sample_v2", "sample")])?;
        assert_eq!(names.execution_name(cell("sample_v2")), "sample");
        assert_eq!(
            names.cell_for_execution_name("sample"),
            Some(cell("sample_v2"))
        );
        Ok(())
    }

    /// The point of the feature: the execution name is stable across the rename that motivates it.
    #[test]
    fn rename_preserves_the_execution_name() -> buck2_error::Result<()> {
        let before = names(&[("root", "root"), ("sample", "sample")])?;
        let after = names(&[("root", "root"), ("sample_v2", "sample")])?;
        assert_eq!(
            before.execution_name(cell("sample")),
            after.execution_name(cell("sample_v2"))
        );
        Ok(())
    }

    /// Otherwise the renamed cell would answer to both its old and its new spelling.
    #[test]
    fn retired_spelling_is_rejected() -> buck2_error::Result<()> {
        let names = names(&[("root", "root"), ("sample", "other")])?;
        assert_eq!(names.cell_for_execution_name("sample"), None);
        assert_eq!(names.cell_for_execution_name("other"), Some(cell("sample")));
        Ok(())
    }

    #[test]
    fn colliding_overrides_are_rejected() {
        assert!(names(&[("root", "root"), ("a", "shared"), ("b", "shared")]).is_err());
    }

    /// Guards the `new` contract for callers outside this crate: a repeated cell would otherwise
    /// silently take whichever name came last.
    #[test]
    fn repeated_cell_is_rejected() {
        assert!(names(&[("root", "root"), ("a", "x"), ("a", "y")]).is_err());
    }

    /// The collision that only the full cell set can see: an override lands on a name another
    /// cell kept by default.
    #[test]
    fn override_colliding_with_a_default_name_is_rejected() {
        assert!(names(&[("root", "root"), ("a", "b"), ("b", "b")]).is_err());
    }

    #[test]
    fn malformed_execution_names_are_rejected() {
        assert!(names(&[("root", "root"), ("a", "")]).is_err());
        assert!(names(&[("root", "root"), ("a", "nested/name")]).is_err());
        assert!(names(&[("root", "root"), ("a", "back\\slash")]).is_err());
        assert!(names(&[("root", "root"), ("a", "new\nline")]).is_err());
    }

    /// Swapping two cells' execution names is a permutation, not a collision.
    #[test]
    fn swapped_names_are_permitted() -> buck2_error::Result<()> {
        let names = names(&[("root", "root"), ("a", "b"), ("b", "a")])?;
        assert_eq!(names.execution_name(cell("a")), "b");
        assert_eq!(names.execution_name(cell("b")), "a");
        assert_eq!(names.cell_for_execution_name("a"), Some(cell("b")));
        assert_eq!(names.cell_for_execution_name("b"), Some(cell("a")));
        Ok(())
    }
}
