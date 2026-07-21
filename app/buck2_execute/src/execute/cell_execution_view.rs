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
use buck2_core::fs::artifact_path_resolver::CellSourcePathMode;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_directory::directory::directory::Directory;
use buck2_directory::directory::directory_iterator::DirectoryIterator;
use buck2_directory::directory::directory_ref::DirectoryRef;
use buck2_directory::directory::entry::DirectoryEntry;

use crate::directory::ActionImmutableDirectory;

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

#[derive(Debug, Default, Eq, PartialEq)]
pub struct CanonicalSourceInputs {
    pub view_requirements: CellExecutionViewRequirements,
    pub physical_paths: Vec<ProjectRelativePathBuf>,
}

pub fn collect_canonical_source_inputs(
    artifact_fs: &ArtifactFs,
    directory: &ActionImmutableDirectory,
) -> buck2_error::Result<CanonicalSourceInputs> {
    let mut view_requirements = CellExecutionViewRequirements::default();
    let mut physical_roots = BTreeMap::new();
    let mut physical_paths = BTreeSet::new();
    for (path, entry) in directory.unordered_walk().with_paths() {
        let path = ProjectRelativePath::new(path.as_str())?;
        let decoded = if matches!(entry, DirectoryEntry::Dir(_)) {
            artifact_fs.decode_source_execution_path(path)?
        } else {
            artifact_fs.decode_source_execution_leaf_path(path)?
        };
        if let Some(cell_path) = decoded {
            view_requirements.add_cell(cell_path.cell());
            if let Some(first) = cell_path.path().iter().next() {
                view_requirements.add_top_level_entry(CellPath::new(
                    cell_path.cell(),
                    CellRelativePathBuf::unchecked_new(first.as_str().to_owned()),
                ));
            }
            if let DirectoryEntry::Dir(directory) = entry
                && directory.entries().next().is_none()
            {
                view_requirements.add_empty_directory(cell_path.clone());
            }
            if !matches!(entry, DirectoryEntry::Dir(_)) {
                let physical_root = match physical_roots.get(&cell_path.cell()) {
                    Some(root) => root,
                    None => {
                        let root = artifact_fs
                            .resolve_cell_source_root_for_consumption(cell_path.cell())?;
                        physical_roots.insert(cell_path.cell(), root);
                        physical_roots
                            .get(&cell_path.cell())
                            .expect("physical root was just inserted")
                    }
                };
                physical_paths.insert(physical_root.join(cell_path.path()));
            }
        }
    }

    Ok(CanonicalSourceInputs {
        view_requirements,
        physical_paths: physical_paths.into_iter().collect(),
    })
}

pub fn prepare_materialized_cell_execution_view(
    view: Option<&dyn CellExecutionView>,
    artifact_fs: &ArtifactFs,
    mut requirements: CellExecutionViewRequirements,
    working_directory: Option<&ProjectRelativePath>,
) -> buck2_error::Result<()> {
    match artifact_fs.cell_source_path_mode() {
        CellSourcePathMode::Physical => Ok(()),
        CellSourcePathMode::CanonicalV1 => {
            let view = view.ok_or_else(|| {
                buck2_error::internal_error!(
                    "Canonical cell execution paths require the daemon-owned cell execution view"
                )
            })?;
            if let Some(working_directory) = working_directory
                && let Some(cwd) = artifact_fs.decode_source_execution_path(working_directory)?
            {
                requirements.add_cell(cwd.cell());
            }
            view.prepare(artifact_fs, &requirements)
        }
    }
}
