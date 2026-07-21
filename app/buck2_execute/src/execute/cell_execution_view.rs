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
use std::collections::BTreeSet;

use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;

/// Realizes the local filesystem roots used by canonical source execution paths. Path policy
/// stays in [`ArtifactFs`]. Implementations are daemon-owned because a root already published to
/// an action must not be rebound while workers or concurrent actions can retain it.
pub trait CellExecutionView: Send + Sync + 'static {
    fn prepare(
        &self,
        artifact_fs: &ArtifactFs,
        requirements: &CellExecutionViewRequirements,
    ) -> buck2_error::Result<()>;
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct CellExecutionViewRequirements {
    by_cell: BTreeMap<CellName, CellRequirements>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct CellRequirements {
    top_level_entries: BTreeSet<CellRelativePathBuf>,
    empty_directories: BTreeSet<CellRelativePathBuf>,
}

impl CellExecutionViewRequirements {
    pub fn merge(&mut self, other: Self) {
        for (cell, other) in other.by_cell {
            let requirements = self.by_cell.entry(cell).or_default();
            requirements
                .top_level_entries
                .extend(other.top_level_entries);
            requirements
                .empty_directories
                .extend(other.empty_directories);
        }
    }

    pub fn add_cell(&mut self, cell: CellName) {
        self.by_cell.entry(cell).or_default();
    }

    pub fn add_top_level_entry(&mut self, path: CellPath) {
        self.by_cell
            .entry(path.cell())
            .or_default()
            .top_level_entries
            .insert(path.path().to_buf());
    }

    pub fn add_empty_directory(&mut self, path: CellPath) {
        self.by_cell
            .entry(path.cell())
            .or_default()
            .empty_directories
            .insert(path.path().to_buf());
    }

    pub fn iter(&self) -> impl Iterator<Item = (CellName, &CellRequirements)> {
        self.by_cell
            .iter()
            .map(|(&cell, requirements)| (cell, requirements))
    }

    pub fn cells(&self) -> impl Iterator<Item = CellName> + '_ {
        self.by_cell.keys().copied()
    }
}

impl CellRequirements {
    pub fn top_level_entries(&self) -> impl Iterator<Item = &CellRelativePathBuf> {
        self.top_level_entries.iter()
    }

    pub fn empty_directories(&self) -> impl Iterator<Item = &CellRelativePathBuf> {
        self.empty_directories.iter()
    }
}
