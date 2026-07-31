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
use std::io;
use std::sync::Arc;

use allocative::Allocative;
use buck2_fs::paths::file_name::FileName;
use buck2_fs::paths::forward_rel_path::ForwardRelativePath;
use dupe::Dupe;
use pagable::Pagable;
use serde::Deserialize;
use serde::Serialize;

use crate::cells::CellResolver;
use crate::cells::cell_path::CellPath;
use crate::cells::cell_path::CellPathRef;
use crate::cells::execution_name::CellExecutionNames;
use crate::cells::name::CellName;
use crate::cells::paths::CellRelativePath;
use crate::content_hash::ContentBasedPathHash;
use crate::fs::buck_out_path::BuckOutNamespace;
use crate::fs::buck_out_path::BuckOutPathResolver;
use crate::fs::buck_out_path::BuildArtifactPath;
use crate::fs::project::ProjectRoot;
use crate::fs::project_rel_path::ProjectRelativePathBuf;
use crate::package::source_path::SourcePathRef;

const CELL_SOURCES_V1_PREFIX: &str = "cell_sources/v1";
const CELL_SOURCES_V1_COMPONENT_PREFIX: &str = "c_";
const CELL_SOURCES_V1_MAX_EXECUTION_NAME_BYTES: usize = 126;

#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    PartialEq,
    Allocative,
    Pagable,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "snake_case")]
pub enum CellSourcePathMode {
    #[default]
    Physical,
    CanonicalV1,
}

impl std::fmt::Display for CellSourcePathMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Physical => f.write_str("physical"),
            Self::CanonicalV1 => f.write_str("canonical_v1"),
        }
    }
}

impl std::str::FromStr for CellSourcePathMode {
    type Err = buck2_error::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "physical" => Ok(Self::Physical),
            "canonical_v1" => Ok(Self::CanonicalV1),
            value => Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Invalid `[buck2] cell_execution_paths` value `{value}`; expected `physical` or `canonical_v1`"
            )),
        }
    }
}

#[derive(Clone, Allocative)]
pub struct ArtifactFs {
    cell_resolver: CellResolver,
    buck_out_path_resolver: BuckOutPathResolver,
    project_filesystem: ProjectRoot,
    cell_source_path_mode: CellSourcePathMode,
    cell_execution_names: Arc<CellExecutionNames>,
}

impl ArtifactFs {
    pub fn new(
        buck_path_resolver: CellResolver,
        buck_out_path_resolver: BuckOutPathResolver,
        project_filesystem: ProjectRoot,
    ) -> Self {
        Self::new_with_cell_source_path_mode(
            buck_path_resolver,
            buck_out_path_resolver,
            project_filesystem,
            CellSourcePathMode::Physical,
        )
    }

    pub fn new_with_cell_source_path_mode(
        buck_path_resolver: CellResolver,
        buck_out_path_resolver: BuckOutPathResolver,
        project_filesystem: ProjectRoot,
        cell_source_path_mode: CellSourcePathMode,
    ) -> Self {
        Self::new_with_execution_names(
            buck_path_resolver,
            buck_out_path_resolver,
            project_filesystem,
            cell_source_path_mode,
            Arc::new(CellExecutionNames::identity()),
        )
    }

    pub fn new_with_execution_names(
        buck_path_resolver: CellResolver,
        buck_out_path_resolver: BuckOutPathResolver,
        project_filesystem: ProjectRoot,
        cell_source_path_mode: CellSourcePathMode,
        cell_execution_names: Arc<CellExecutionNames>,
    ) -> Self {
        Self {
            cell_resolver: buck_path_resolver,
            buck_out_path_resolver,
            project_filesystem,
            cell_source_path_mode,
            cell_execution_names,
        }
    }

    pub fn cell_source_path_mode(&self) -> CellSourcePathMode {
        self.cell_source_path_mode
    }

    /// The name this cell contributes to its canonical execution path. Callers that render or
    /// compare execution paths must go through this rather than [`CellName::as_str`].
    pub fn cell_execution_name(&self, cell: CellName) -> &str {
        self.cell_execution_names.execution_name(cell)
    }

    /// `cells` are `(name, project-relative root)` pairs; the first entry is the root alias cell.
    pub fn testing_new_with_mode(mode: CellSourcePathMode, cells: &[(&str, &str)]) -> ArtifactFs {
        Self::testing_new_with_mode_and_external(mode, cells, &[])
    }

    /// Cells named in `external` are given a fixed git origin.
    pub fn testing_new_with_mode_and_external(
        mode: CellSourcePathMode,
        cells: &[(&str, &str)],
        external: &[&str],
    ) -> ArtifactFs {
        Self::testing_new_full(mode, cells, external, &[], None)
    }

    /// `execution_names` are `(cell, execution name)` pairs; cells left out keep their own name.
    /// A pair naming a cell this `ArtifactFs` does not have is an error rather than a silent
    /// no-op, so a typo cannot turn a test of this feature into a test of the identity mapping.
    pub fn testing_with_execution_names(
        self,
        execution_names: &[(&str, &str)],
    ) -> buck2_error::Result<ArtifactFs> {
        let mut names: BTreeMap<CellName, String> = self
            .cell_resolver
            .cells()
            .map(|(cell, _)| (cell, cell.as_str().to_owned()))
            .collect();
        for (cell, name) in execution_names {
            let cell = CellName::unchecked_new(cell)?;
            self.cell_resolver.get(cell)?;
            names.insert(cell, (*name).to_owned());
        }
        Ok(ArtifactFs {
            cell_execution_names: Arc::new(CellExecutionNames::new(names)?),
            ..self
        })
    }

    /// Cells named in `external` get a git origin, cells named in `bundled` a bundled origin.
    pub fn testing_new_full(
        mode: CellSourcePathMode,
        cells: &[(&str, &str)],
        external: &[&str],
        bundled: &[&str],
        project_root: Option<ProjectRoot>,
    ) -> ArtifactFs {
        use crate::cells::CellAliasResolver;
        use crate::cells::cell_root_path::CellRootPathBuf;
        use crate::cells::external::ExternalCellOrigin;
        use crate::cells::external::GitCellSetup;
        use crate::cells::instance::CellInstance;
        use crate::cells::nested::NestedCells;

        let cells: Vec<(CellName, CellRootPathBuf)> = cells
            .iter()
            .map(|(name, path)| {
                (
                    CellName::unchecked_new(name).unwrap(),
                    CellRootPathBuf::testing_new(path),
                )
            })
            .collect();
        let roots: Vec<_> = cells
            .iter()
            .map(|(name, path)| (*name, path.as_path()))
            .collect();
        let instances = cells
            .iter()
            .map(|(name, path)| {
                let origin = if bundled.contains(&name.as_str()) {
                    Some(ExternalCellOrigin::Bundled(*name))
                } else if external.contains(&name.as_str()) {
                    Some(ExternalCellOrigin::Git(GitCellSetup {
                        git_origin: std::sync::Arc::from(
                            format!("https://example.com/{name}.git").as_str(),
                        ),
                        commit: std::sync::Arc::from("0123456789abcdef0123456789abcdef01234567"),
                        object_format: None,
                    }))
                } else {
                    None
                };
                CellInstance::new(
                    *name,
                    path.clone(),
                    origin,
                    NestedCells::from_cell_roots(&roots, path),
                )
                .unwrap()
            })
            .collect();
        let resolver = CellResolver::new(
            instances,
            CellAliasResolver::new(cells[0].0, Default::default()).unwrap(),
        )
        .unwrap();
        let project_root = project_root.unwrap_or_else(|| {
            ProjectRoot::new_unchecked(
                buck2_fs::paths::abs_norm_path::AbsNormPathBuf::new(
                    std::path::Path::new(if cfg!(windows) {
                        "C:\\project"
                    } else {
                        "/project"
                    })
                    .to_owned(),
                )
                .unwrap(),
            )
        });
        ArtifactFs::new_with_cell_source_path_mode(
            resolver,
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck-out/v2".into())),
            project_root,
            mode,
        )
    }

    pub fn retrieve_unhashed_location(
        &self,
        path: &BuildArtifactPath,
    ) -> Option<ProjectRelativePathBuf> {
        self.buck_out_path_resolver.unhashed_gen(path)
    }

    pub fn resolve_build(
        &self,
        path: &BuildArtifactPath,
        content_hash: Option<&ContentBasedPathHash>,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        self.buck_out_path_resolver.resolve_gen(path, content_hash)
    }

    pub fn resolve_build_configuration_hash_path(
        &self,
        path: &BuildArtifactPath,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        self.buck_out_path_resolver
            .resolve_gen_configuration_hash_path(path)
    }

    pub fn resolve_cell_path(
        &self,
        path: CellPathRef,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        self.cell_resolver.resolve_path(path)
    }

    /// Resolves a cell path to the path that should be observed by an action.
    pub fn resolve_cell_path_for_execution(
        &self,
        path: CellPathRef,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        match self.cell_source_path_mode {
            CellSourcePathMode::Physical => self.resolve_cell_path(path),
            CellSourcePathMode::CanonicalV1 => {
                let root = self.validate_canonical_cell_layout(path.cell())?;
                if self.is_reserved_buck_out_source_path(path.path()) {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "Source path `{path}` is beneath the reserved top-level `buck-out` subtree in `canonical_v1`",
                    ));
                }
                Ok(root.join(path.path()))
            }
        }
    }

    /// Missing roots remain the responsibility of normal source lookup, but existing local root
    /// components must not redirect canonical source identity through a symlink or junction.
    pub fn validate_canonical_cell(&self, cell: CellName) -> buck2_error::Result<()> {
        if self.cell_source_path_mode == CellSourcePathMode::Physical {
            return Ok(());
        }

        self.validate_canonical_cell_layout(cell)?;
        let instance = self.cell_resolver.get(cell)?;
        if instance.external().is_none() {
            self.validate_local_cell_root_no_follow(
                cell,
                instance.path().as_project_relative_path(),
            )?;
        }
        Ok(())
    }

    /// Runs the lexical checks eagerly so a misconfigured workspace fails before any source
    /// `ArtifactValue`, Action, or upload is constructed, rather than at first lazy resolution.
    pub fn validate_canonical_layout(&self) -> buck2_error::Result<()> {
        if self.cell_source_path_mode == CellSourcePathMode::Physical {
            return Ok(());
        }
        // `CellExecutionNames` enforces uniqueness over the cells it was built from, but it is
        // constructed separately from this resolver and need not cover the same set. A cell it
        // omits falls back to its own name, which can collide with a name it did assign, so
        // uniqueness of the actual roots is checked here rather than assumed.
        let mut roots: BTreeMap<ProjectRelativePathBuf, CellName> = BTreeMap::new();
        for (cell, _) in self.cell_resolver.cells() {
            let root = self.validate_canonical_cell_layout(cell)?;
            if let Some(previous) = roots.insert(root.clone(), cell) {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Cells `{previous}` and `{cell}` both resolve to canonical execution root `{root}`; execution names must be unique across cells",
                ));
            }
        }
        Ok(())
    }

    /// Returns the cell's canonical execution root, which the checks already have to compute.
    fn validate_canonical_cell_layout(
        &self,
        cell: CellName,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        self.validate_canonical_buck_out_root()?;
        let root = self.resolve_cell_source_root_for_execution(cell)?;
        let instance = self.cell_resolver.get(cell)?;
        if instance.external().is_none() {
            let physical_root = instance.path().as_project_relative_path();
            let buck_out_root = self.buck_out_path_resolver.root();
            if !physical_root.is_empty()
                && (project_path_has_prefix(physical_root, buck_out_root)
                    || project_path_has_prefix(buck_out_root, physical_root))
            {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Local cell `{cell}` has physical source root `{physical_root}`, which overlaps reserved Buck-out root `{buck_out_root}` in `canonical_v1`",
                ));
            }
            // A local cell rooted beneath any isolation dir, not just this daemon's, would treat
            // another daemon's live materializer or view state as portable source.
            if let Some(first) = physical_root.iter().next()
                && self.is_reserved_buck_out_top_level_name(first)
            {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Local cell `{cell}` has physical source root `{physical_root}` beneath reserved top-level `buck-out` in `canonical_v1`",
                ));
            }
        }
        Ok(root)
    }

    pub fn is_reserved_buck_out_source_path(&self, path: &CellRelativePath) -> bool {
        path.iter()
            .next()
            .is_some_and(|component| self.is_reserved_buck_out_top_level_name(component))
    }

    pub fn is_reserved_buck_out_top_level_name(&self, name: &FileName) -> bool {
        source_component_eq(name.as_str(), "buck-out")
    }

    pub fn resolve_cell_source_root_physical(
        &self,
        cell: CellName,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        let instance = self.cell_resolver.get(cell)?;
        if let Some(origin) = instance.external() {
            Ok(self
                .buck_out_path_resolver
                .resolve_external_cell_source(CellRelativePath::empty(), origin.dupe()))
        } else {
            Ok(instance.path().as_project_relative_path().to_buf())
        }
    }

    /// Revalidating here closes a local cell-root symlink rebinding race between initial
    /// configuration and the later upload or local-view handoff.
    pub fn resolve_cell_source_root_for_consumption(
        &self,
        cell: CellName,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        self.validate_canonical_cell(cell)?;
        self.resolve_cell_source_root_physical(cell)
    }

    pub fn resolve_source(
        &self,
        source_artifact_path: SourcePathRef,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        let cell_resolver = self.cell_resolver();
        if let Some(origin) = cell_resolver
            .get(source_artifact_path.package().cell_name())?
            .external()
        {
            Ok(self.buck_out_path_resolver.resolve_external_cell_source(
                source_artifact_path.to_cell_path().path(),
                origin.dupe(),
            ))
        } else {
            Ok(cell_resolver
                .resolve_path(source_artifact_path.package().as_cell_path())?
                .join(source_artifact_path.path()))
        }
    }

    pub fn resolve_source_for_execution(
        &self,
        source_artifact_path: SourcePathRef,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        match self.cell_source_path_mode {
            CellSourcePathMode::Physical => self.resolve_source(source_artifact_path),
            CellSourcePathMode::CanonicalV1 => {
                let cell_path = source_artifact_path.to_cell_path();
                self.resolve_cell_path_for_execution(cell_path.as_ref())
            }
        }
    }

    /// In canonical mode, malformed paths below the reserved prefix are rejected rather than
    /// treated as generated paths.
    pub fn decode_source_execution_path(
        &self,
        path: &crate::fs::project_rel_path::ProjectRelativePath,
    ) -> buck2_error::Result<Option<CellPath>> {
        if self.cell_source_path_mode == CellSourcePathMode::Physical {
            return Ok(None);
        }

        self.validate_canonical_buck_out_root()?;
        let namespace = self
            .buck_out_path_resolver
            .namespace_path(BuckOutNamespace::CellSources);
        let Some(namespace_relative) = strip_project_prefix_for_source_identity(path, &namespace)
        else {
            return Ok(None);
        };
        if namespace_relative.is_empty() {
            // Directory walkers visit the reserved namespace root before its version child.
            return Ok(None);
        }
        let mut namespace_components = namespace_relative.iter();
        let version = namespace_components.next().expect("checked non-empty path");
        if !source_component_eq(version.as_str(), "v1") {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Malformed canonical cell execution path `{path}`: unsupported entry `{version}` in the reserved cell-sources namespace"
            ));
        }
        let relative = namespace_components.as_path();
        if relative.is_empty() {
            return Ok(None);
        }
        let mut components = relative.iter();
        let encoded_component = components.next().expect("checked non-empty path");
        let encoded = encoded_component
            .as_str()
            .strip_prefix(CELL_SOURCES_V1_COMPONENT_PREFIX)
            .ok_or_else(|| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Malformed canonical cell execution path `{path}`: expected a `c_` cell component"
                )
            })?;
        let name_bytes = hex::decode(encoded).map_err(|e| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Malformed canonical cell execution path `{path}`: invalid execution-name encoding: {e}"
            )
        })?;
        let name = std::str::from_utf8(&name_bytes).map_err(|e| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Malformed canonical cell execution path `{path}`: execution name is not UTF-8: {e}"
            )
        })?;
        if name_bytes.len() > CELL_SOURCES_V1_MAX_EXECUTION_NAME_BYTES
            || format!(
                "{CELL_SOURCES_V1_COMPONENT_PREFIX}{}",
                hex::encode(&name_bytes)
            ) != encoded_component.as_str()
        {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Malformed canonical cell execution path `{path}`: execution-name encoding is not canonical"
            ));
        }
        let cell = self
            .cell_execution_names
            .cell_for_execution_name(name)
            .ok_or_else(|| {
                buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "Canonical cell execution path `{path}` uses execution name `{name}`, which no configured cell uses"
                )
            })?;
        self.cell_resolver.get(cell).map_err(|e| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Canonical cell execution path `{path}` names cell `{cell}`, which is not configured: {e}"
            )
        })?;
        let suffix = CellRelativePath::new(components.as_path()).to_buf();
        if self.is_reserved_buck_out_source_path(&suffix) {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Malformed canonical cell execution path `{path}`: source suffix is beneath reserved top-level `buck-out`"
            ));
        }
        Ok(Some(CellPath::new(cell, suffix)))
    }

    /// Like [`Self::decode_source_execution_path`], but a leaf occupying the reserved namespace
    /// root or version directory is an error rather than `None`: only directory walkers may
    /// traverse those intermediate names.
    pub fn decode_source_execution_leaf_path(
        &self,
        path: &crate::fs::project_rel_path::ProjectRelativePath,
    ) -> buck2_error::Result<Option<CellPath>> {
        if self.cell_source_path_mode == CellSourcePathMode::Physical {
            return Ok(None);
        }
        match self.decode_source_execution_path(path)? {
            Some(cell_path) => Ok(Some(cell_path)),
            None => {
                let namespace = self
                    .buck_out_path_resolver
                    .namespace_path(BuckOutNamespace::CellSources);
                match strip_project_prefix_for_source_identity(path, &namespace) {
                    Some(_) => Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "Raw leaf input `{path}` occupies the reserved cell-sources namespace"
                    )),
                    None => Ok(None),
                }
            }
        }
    }

    fn resolve_cell_source_root_for_execution(
        &self,
        cell: CellName,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        let name = self.cell_execution_names.execution_name(cell);
        if name.len() > CELL_SOURCES_V1_MAX_EXECUTION_NAME_BYTES {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Canonical execution name `{name}` for cell `{cell}` is {} UTF-8 bytes; `canonical_v1` supports at most {CELL_SOURCES_V1_MAX_EXECUTION_NAME_BYTES} bytes so its execution path remains portable",
                name.len(),
            ));
        }
        let component = format!(
            "{CELL_SOURCES_V1_COMPONENT_PREFIX}{}",
            hex::encode(name.as_bytes())
        );
        Ok(self
            .canonical_cell_sources_root()
            .join(ForwardRelativePath::unchecked_new(&component)))
    }

    fn validate_canonical_buck_out_root(&self) -> buck2_error::Result<()> {
        let root = self.buck_out_path_resolver.root();
        let mut components = root.iter();
        let first = components.next().map(|x| x.as_str());
        if first != Some("buck-out") || components.next().is_none() || components.next().is_some() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "`canonical_v1` requires a production-shaped Buck-out root `buck-out/<isolation>`, got `{root}`",
            ));
        }
        Ok(())
    }

    fn validate_local_cell_root_no_follow(
        &self,
        cell: CellName,
        physical_root: &crate::fs::project_rel_path::ProjectRelativePath,
    ) -> buck2_error::Result<()> {
        let mut current = self.project_filesystem.root().as_path().to_path_buf();
        for component in physical_root.iter() {
            current.push(component.as_str());
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink()
                        || buck2_fs::fs_util::is_reparse_point(&metadata)
                    {
                        return Err(buck2_error::buck2_error!(
                            buck2_error::ErrorTag::Input,
                            "Local cell `{cell}` has source root `{physical_root}` with symlink/junction component `{}`; canonical_v1 requires ordinary local cell roots to use real directory components",
                            current.display(),
                        ));
                    }
                    if !metadata.is_dir() {
                        return Err(buck2_error::buck2_error!(
                            buck2_error::ErrorTag::Input,
                            "Local cell `{cell}` has source root `{physical_root}` with non-directory component `{}`",
                            current.display(),
                        ));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) => {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "Failed to inspect local cell `{cell}` source-root component `{}`: {e}",
                        current.display(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn canonical_cell_sources_root(&self) -> ProjectRelativePathBuf {
        self.buck_out_path_resolver
            .root()
            .join(ForwardRelativePath::unchecked_new(CELL_SOURCES_V1_PREFIX))
    }

    pub fn resolve_offline_output_cache_path(
        &self,
        path: &BuildArtifactPath,
    ) -> buck2_error::Result<ProjectRelativePathBuf> {
        self.buck_out_path_resolver.resolve_offline_cache(path)
    }

    pub fn fs(&self) -> &ProjectRoot {
        &self.project_filesystem
    }

    pub fn buck_out_path_resolver(&self) -> &BuckOutPathResolver {
        &self.buck_out_path_resolver
    }

    pub fn cell_resolver(&self) -> &CellResolver {
        &self.cell_resolver
    }
}

/// Borrows the suffix: this is on the path of every source resolution in `canonical_v1`, so a
/// yes/no answer must not cost an allocation.
fn strip_project_prefix_for_source_identity<'a>(
    path: &'a crate::fs::project_rel_path::ProjectRelativePath,
    prefix: &crate::fs::project_rel_path::ProjectRelativePath,
) -> Option<&'a ForwardRelativePath> {
    let mut components = path.iter();
    for expected in prefix.iter() {
        let actual = components.next()?;
        if !source_component_eq(actual.as_str(), expected.as_str()) {
            return None;
        }
    }
    Some(components.as_path())
}

fn project_path_has_prefix(
    path: &crate::fs::project_rel_path::ProjectRelativePath,
    prefix: &crate::fs::project_rel_path::ProjectRelativePath,
) -> bool {
    strip_project_prefix_for_source_identity(path, prefix).is_some()
}

pub(crate) fn source_component_eq(left: &str, right: &str) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        left.eq_ignore_ascii_case(right)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        left == right
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::ArtifactFs;
    use super::CellSourcePathMode;
    use crate::cells::cell_path::CellPath;
    use crate::cells::execution_name::CellExecutionNames;
    use crate::cells::name::CellName;
    use crate::cells::paths::CellRelativePathBuf;
    use crate::fs::buck_out_path::BuckOutPathResolver;
    use crate::fs::project_rel_path::ProjectRelativePath;
    use crate::fs::project_rel_path::ProjectRelativePathBuf;
    use crate::package::source_path::SourcePath;

    fn external_artifact_fs(mode: CellSourcePathMode) -> buck2_error::Result<ArtifactFs> {
        Ok(ArtifactFs::testing_new_full(
            mode,
            &[("root", ""), ("sample", "declared/sample")],
            &["sample"],
            &[],
            None,
        ))
    }

    fn local_artifact_fs_with_cell_name(name: &str) -> buck2_error::Result<ArtifactFs> {
        local_artifact_fs_with_cell_name_and_path(name, "declared/cell")
    }

    fn local_artifact_fs_with_cell_name_and_path(
        name: &str,
        path: &str,
    ) -> buck2_error::Result<ArtifactFs> {
        Ok(ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), (name, path)],
        ))
    }

    #[test]
    fn execution_resolution_preserves_legacy_external_paths() -> buck2_error::Result<()> {
        let fs = external_artifact_fs(CellSourcePathMode::Physical)?;
        let source = SourcePath::testing_new("sample//pkg", "src.cpp");

        let physical = fs.resolve_source(source.as_ref())?;
        assert_eq!(physical, fs.resolve_source_for_execution(source.as_ref())?);
        assert_eq!(
            &*physical,
            ProjectRelativePath::unchecked_new(
                "buck-out/v2/external_cells/git/0123456789abcdef0123456789abcdef01234567/pkg/src.cpp"
            )
        );

        let cell_path = source.to_cell_path();
        let declared = fs.resolve_cell_path(cell_path.as_ref())?;
        assert_eq!(
            declared,
            fs.resolve_cell_path_for_execution(cell_path.as_ref())?
        );
        assert_eq!(
            &*declared,
            ProjectRelativePath::unchecked_new("declared/sample/pkg/src.cpp")
        );

        Ok(())
    }

    #[test]
    fn canonical_execution_resolution_ignores_external_origin() -> buck2_error::Result<()> {
        let fs = external_artifact_fs(CellSourcePathMode::CanonicalV1)?;
        let source = SourcePath::testing_new("sample//pkg", "src.cpp");

        let execution = fs.resolve_source_for_execution(source.as_ref())?;
        assert_eq!(
            &*execution,
            ProjectRelativePath::unchecked_new(
                "buck-out/v2/cell_sources/v1/c_73616d706c65/pkg/src.cpp"
            )
        );
        assert_eq!(
            fs.decode_source_execution_path(&execution)?,
            Some(source.to_cell_path().to_owned())
        );
        let decoded = fs
            .decode_source_execution_path(&execution)?
            .expect("canonical source path");
        assert_eq!(
            fs.resolve_cell_source_root_for_consumption(decoded.cell())?
                .join(decoded.path()),
            ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2/external_cells/git/0123456789abcdef0123456789abcdef01234567/pkg/src.cpp"
                    .into()
            )
        );
        assert_eq!(
            fs.resolve_source(source.as_ref())?,
            ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2/external_cells/git/0123456789abcdef0123456789abcdef01234567/pkg/src.cpp"
                    .into()
            )
        );

        let local_fs = local_artifact_fs_with_cell_name("sample")?;
        assert_eq!(
            local_fs.resolve_source_for_execution(source.as_ref())?,
            execution
        );
        assert_eq!(
            local_fs.resolve_source(source.as_ref())?,
            ProjectRelativePathBuf::unchecked_new("declared/cell/pkg/src.cpp".into())
        );

        let root_source = SourcePath::testing_new("root//pkg", "src.cpp");
        let root_execution = fs.resolve_source_for_execution(root_source.as_ref())?;
        assert_eq!(
            root_execution,
            ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2/cell_sources/v1/c_726f6f74/pkg/src.cpp".into()
            )
        );
        assert_eq!(
            fs.decode_source_execution_path(&root_execution)?,
            Some(root_source.to_cell_path().to_owned())
        );
        let decoded_root = fs
            .decode_source_execution_path(&root_execution)?
            .expect("canonical root source path");
        assert_eq!(
            fs.resolve_cell_source_root_for_consumption(decoded_root.cell())?
                .join(decoded_root.path()),
            ProjectRelativePathBuf::unchecked_new("pkg/src.cpp".into())
        );
        Ok(())
    }

    #[test]
    fn physical_mode_does_not_decode_reserved_prefix() -> buck2_error::Result<()> {
        let fs = external_artifact_fs(CellSourcePathMode::Physical)?;
        let path = ProjectRelativePath::unchecked_new(
            "buck-out/v2/cell_sources/v1/c_73616d706c65/pkg/src.cpp",
        );
        assert_eq!(fs.decode_source_execution_path(path)?, None);
        Ok(())
    }

    /// The feature's reason to exist: renaming the cell while pinning its execution name leaves
    /// every source execution path, and therefore every Action digest, byte-identical.
    #[test]
    fn execution_name_survives_a_cell_rename() -> buck2_error::Result<()> {
        let before = ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), ("sample", "third-party/sample")],
        );
        let after = ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), ("sample_v2", "third-party/sample")],
        )
        .testing_with_execution_names(&[("sample_v2", "sample")])?;

        let before_path = before.resolve_source_for_execution(
            SourcePath::testing_new("sample//pkg", "src.cpp").as_ref(),
        )?;
        let after_path = after.resolve_source_for_execution(
            SourcePath::testing_new("sample_v2//pkg", "src.cpp").as_ref(),
        )?;
        assert_eq!(before_path, after_path);
        assert_eq!(
            &*after_path,
            ProjectRelativePath::unchecked_new(
                "buck-out/v2/cell_sources/v1/c_73616d706c65/pkg/src.cpp"
            )
        );

        // The path still decodes, now to the renamed cell.
        assert_eq!(
            after.decode_source_execution_path(&after_path)?,
            Some(SourcePath::testing_new("sample_v2//pkg", "src.cpp").to_cell_path())
        );
        Ok(())
    }

    /// Accepting the cell's own name after it was given a different execution name would give one
    /// cell two execution paths, and so two Action digests for one source.
    #[test]
    fn canonical_decoder_rejects_a_retired_cell_name() -> buck2_error::Result<()> {
        let fs = ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), ("sample_v2", "third-party/sample")],
        )
        .testing_with_execution_names(&[("sample_v2", "sample")])?;
        let retired = ProjectRelativePath::new(&format!(
            "buck-out/v2/cell_sources/v1/c_{}/pkg/src.cpp",
            hex::encode("sample_v2")
        ))?
        .to_buf();
        assert!(
            fs.decode_source_execution_path(&retired).is_err(),
            "retired cell name must not decode"
        );
        Ok(())
    }

    /// An external cell and a local checkout of the same sources must reach the same execution
    /// path when pinned to one name: that migration is the reason canonical paths exist.
    #[test]
    fn external_and_local_cells_share_a_pinned_execution_name() -> buck2_error::Result<()> {
        let external = ArtifactFs::testing_new_with_mode_and_external(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), ("sample_v2", "declared/sample")],
            &["sample_v2"],
        )
        .testing_with_execution_names(&[("sample_v2", "sample")])?;
        let local = ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), ("sample_v2", "third-party/sample")],
        )
        .testing_with_execution_names(&[("sample_v2", "sample")])?;
        let source = SourcePath::testing_new("sample_v2//pkg", "src.cpp");
        assert_eq!(
            external.resolve_source_for_execution(source.as_ref())?,
            local.resolve_source_for_execution(source.as_ref())?
        );
        assert_eq!(
            &*external.resolve_source_for_execution(source.as_ref())?,
            ProjectRelativePath::unchecked_new(
                "buck-out/v2/cell_sources/v1/c_73616d706c65/pkg/src.cpp"
            )
        );
        // The physical sides still differ; only the execution path is shared.
        assert_ne!(
            external.resolve_source(source.as_ref())?,
            local.resolve_source(source.as_ref())?
        );
        Ok(())
    }

    /// Two cells sharing an execution name would collide in one forest directory.
    #[test]
    fn colliding_execution_names_are_rejected() {
        assert!(
            ArtifactFs::testing_new_with_mode(
                CellSourcePathMode::CanonicalV1,
                &[("root", ""), ("a", "cells/a"), ("b", "cells/b")],
            )
            .testing_with_execution_names(&[("a", "shared"), ("b", "shared")])
            .is_err()
        );
    }

    /// `CellExecutionNames` can only enforce uniqueness over the cells it was built from. Here `a`
    /// is renamed onto `b`'s own name while `b` was never offered to it, so the collision is only
    /// visible against the resolver: both cells would publish into one forest directory.
    #[test]
    fn execution_names_not_covering_every_cell_are_rejected() -> buck2_error::Result<()> {
        let fs = ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), ("a", "cells/a"), ("b", "cells/b")],
        );
        let partial = CellExecutionNames::new([(CellName::testing_new("a"), "b".to_owned())])?;
        let fs = ArtifactFs::new_with_execution_names(
            fs.cell_resolver().clone(),
            fs.buck_out_path_resolver().clone(),
            fs.fs().clone(),
            CellSourcePathMode::CanonicalV1,
            Arc::new(partial),
        );

        assert_eq!(fs.cell_execution_name(CellName::testing_new("a")), "b");
        assert_eq!(fs.cell_execution_name(CellName::testing_new("b")), "b");
        assert!(
            fs.validate_canonical_layout().is_err(),
            "two cells resolving to one execution root must be rejected"
        );
        Ok(())
    }

    #[test]
    fn canonical_decoder_rejects_noncanonical_encoding() -> buck2_error::Result<()> {
        let fs = external_artifact_fs(CellSourcePathMode::CanonicalV1)?;
        assert_eq!(
            fs.decode_source_execution_path(ProjectRelativePath::unchecked_new(
                "buck-out/v2/cell_sources/v1"
            ))?,
            None
        );
        let uppercase = ProjectRelativePath::unchecked_new(
            "buck-out/v2/cell_sources/v1/c_73616D706C65/pkg/src.cpp",
        );
        assert!(fs.decode_source_execution_path(uppercase).is_err());
        let malformed =
            ProjectRelativePath::unchecked_new("buck-out/v2/cell_sources/v1/sample/pkg/src.cpp");
        assert!(fs.decode_source_execution_path(malformed).is_err());
        for reserved in [
            "buck-out/v2/cell_sources/v2/c_73616d706c65/pkg/src.cpp",
            "buck-out/v2/cell_sources/.owners/c_73616d706c65",
        ] {
            assert!(
                fs.decode_source_execution_path(ProjectRelativePath::unchecked_new(reserved))
                    .is_err(),
                "reserved cell-sources entry `{reserved}` must not fall through as generated"
            );
        }
        Ok(())
    }

    #[test]
    fn canonical_sources_reject_reserved_buck_out_subtree() -> buck2_error::Result<()> {
        let fs = external_artifact_fs(CellSourcePathMode::CanonicalV1)?;
        let colliding = SourcePath::testing_new(
            "root//buck-out/v2/cell_sources/v1/c_73616d706c65",
            "pkg/src.cpp",
        );
        assert!(fs.resolve_source_for_execution(colliding.as_ref()).is_err());
        assert!(
            fs.resolve_cell_path_for_execution(
                CellPath::new(
                    CellName::testing_new("root"),
                    CellRelativePathBuf::unchecked_new("buck-out/v2/cell_sources".into()),
                )
                .as_ref(),
            )
            .is_err()
        );
        assert_eq!(
            fs.resolve_cell_path_for_execution(
                CellPath::new(
                    CellName::testing_new("root"),
                    CellRelativePathBuf::unchecked_new(String::new()),
                )
                .as_ref(),
            )?,
            ProjectRelativePathBuf::unchecked_new("buck-out/v2/cell_sources/v1/c_726f6f74".into()),
        );

        let non_root = SourcePath::testing_new("sample//buck-out", "source");
        assert!(fs.resolve_source_for_execution(non_root.as_ref()).is_err());

        let physical = external_artifact_fs(CellSourcePathMode::Physical)?;
        assert_eq!(
            physical.resolve_source_for_execution(colliding.as_ref())?,
            ProjectRelativePathBuf::unchecked_new(
                "buck-out/v2/cell_sources/v1/c_73616d706c65/pkg/src.cpp".into()
            )
        );
        Ok(())
    }

    #[test]
    fn canonical_cell_name_portable_component_limit() -> buck2_error::Result<()> {
        let accepted_name = "a".repeat(126);
        let accepted_fs = local_artifact_fs_with_cell_name(&accepted_name)?;
        let accepted = accepted_fs.resolve_cell_path_for_execution(
            CellPath::new(
                CellName::unchecked_new(&accepted_name)?,
                CellRelativePathBuf::unchecked_new("source".into()),
            )
            .as_ref(),
        )?;
        assert_eq!(accepted.file_name().unwrap().as_str(), "source",);
        assert!(
            accepted
                .as_str()
                .contains(&format!("c_{}", "61".repeat(126)))
        );

        let rejected_name = "a".repeat(127);
        let rejected_fs = local_artifact_fs_with_cell_name(&rejected_name)?;
        assert!(
            rejected_fs
                .resolve_cell_path_for_execution(
                    CellPath::new(
                        CellName::unchecked_new(&rejected_name)?,
                        CellRelativePathBuf::unchecked_new("source".into()),
                    )
                    .as_ref(),
                )
                .is_err()
        );

        // The ROOT cell takes the same encoded `c_*` component, so the limit applies to it too.
        let root_cell_path = |name: &str| -> buck2_error::Result<CellPath> {
            Ok(CellPath::new(
                CellName::unchecked_new(name)?,
                CellRelativePathBuf::unchecked_new("source".into()),
            ))
        };
        let accepted_root_fs = ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[(&accepted_name, "")],
        );
        assert_eq!(
            accepted_root_fs
                .resolve_cell_path_for_execution(root_cell_path(&accepted_name)?.as_ref())?
                .as_str(),
            format!("buck-out/v2/cell_sources/v1/c_{}/source", "61".repeat(126))
        );
        let rejected_root_fs = ArtifactFs::testing_new_with_mode(
            CellSourcePathMode::CanonicalV1,
            &[(&rejected_name, "")],
        );
        assert!(
            rejected_root_fs
                .resolve_cell_path_for_execution(root_cell_path(&rejected_name)?.as_ref())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn canonical_decoder_rejects_nested_owners_namespace() -> buck2_error::Result<()> {
        // `.owners` is a sibling of the `c_*` forests inside `v1`, not a generated path.
        let fs = external_artifact_fs(CellSourcePathMode::CanonicalV1)?;
        for reserved in [
            "buck-out/v2/cell_sources/v1/.owners",
            "buck-out/v2/cell_sources/v1/.owners/c_73616d706c65",
        ] {
            let path = ProjectRelativePath::unchecked_new(reserved);
            assert!(
                fs.decode_source_execution_path(path).is_err(),
                "nested owner record `{reserved}` must not decode as a source or generated path"
            );
            assert!(
                fs.decode_source_execution_leaf_path(path).is_err(),
                "nested owner leaf `{reserved}` must not decode as a source or generated path"
            );
        }
        Ok(())
    }

    #[test]
    fn canonical_leaf_decoder_rejects_leaves_at_namespace_roots() -> buck2_error::Result<()> {
        let fs = external_artifact_fs(CellSourcePathMode::CanonicalV1)?;

        // Directory walkers may traverse these names, but a leaf occupying one of them would
        // silently squat the reserved execution-view namespace.
        for intermediate in ["buck-out/v2/cell_sources", "buck-out/v2/cell_sources/v1"] {
            let path = ProjectRelativePath::unchecked_new(intermediate);
            assert_eq!(fs.decode_source_execution_path(path)?, None);
            assert!(
                fs.decode_source_execution_leaf_path(path).is_err(),
                "leaf at reserved namespace name `{intermediate}` must be rejected"
            );
        }

        assert_eq!(
            fs.decode_source_execution_leaf_path(ProjectRelativePath::unchecked_new(
                "buck-out/v2/art/root/pkg/__target__/out.txt"
            ))?,
            None
        );
        assert_eq!(
            fs.decode_source_execution_leaf_path(ProjectRelativePath::unchecked_new(
                "buck-out/v2/cell_sources/v1/c_73616d706c65/pkg/src.cpp"
            ))?,
            Some(CellPath::new(
                CellName::testing_new("sample"),
                CellRelativePathBuf::unchecked_new("pkg/src.cpp".into()),
            ))
        );
        Ok(())
    }

    #[test]
    fn canonical_local_cell_rejects_reserved_buck_out_overlap() -> buck2_error::Result<()> {
        for root in [
            "buck-out",
            "buck-out/v2",
            "buck-out/v2/cell_sources/v1/c_73616d706c65",
            "buck-out/v2/art/source",
            "buck-out/v2/external_cells/git/source",
            "buck-out/v2/gen-anon/source",
            "buck-out/v2/tmp",
            // Another daemon's isolation dir: reserved too, despite not overlapping `buck-out/v2`.
            "buck-out/v3",
            "buck-out/v3/cell",
        ] {
            let fs = local_artifact_fs_with_cell_name_and_path("sample", root)?;
            assert!(
                fs.validate_canonical_layout().is_err(),
                "physical root `{root}` should conflict with reserved Buck-out storage"
            );
        }
        Ok(())
    }

    #[test]
    fn canonical_mode_requires_production_buck_out_root() -> buck2_error::Result<()> {
        for root in ["buck-out", "other/v2", "base/buck-out/v2"] {
            let mut fs = external_artifact_fs(CellSourcePathMode::CanonicalV1)?;
            fs.buck_out_path_resolver =
                BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new(root.to_owned()));
            assert!(
                fs.resolve_cell_path_for_execution(
                    CellPath::new(
                        CellName::testing_new("root"),
                        CellRelativePathBuf::unchecked_new("source".into()),
                    )
                    .as_ref(),
                )
                .is_err(),
                "Buck-out root `{root}` should be rejected"
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn physical_handoff_revalidates_local_cell_root() -> buck2_error::Result<()> {
        use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;

        use crate::fs::project::ProjectRoot;

        let temp = tempfile::tempdir()?;
        std::fs::create_dir(temp.path().join("cell"))?;
        std::fs::create_dir(temp.path().join("replacement"))?;

        let mut fs = local_artifact_fs_with_cell_name_and_path("sample", "cell")?;
        fs.project_filesystem =
            ProjectRoot::new_unchecked(AbsNormPathBuf::new(temp.path().to_path_buf()).unwrap());
        let cell = CellName::testing_new("sample");
        fs.validate_canonical_cell(cell)?;

        std::fs::remove_dir(temp.path().join("cell"))?;
        std::os::unix::fs::symlink("replacement", temp.path().join("cell"))?;
        assert!(
            fs.validate_canonical_cell(cell).is_err(),
            "a cell root rebound through a symlink must not validate"
        );
        assert!(
            fs.resolve_cell_source_root_for_consumption(cell).is_err(),
            "physical byte lookup must revalidate at the consumption boundary"
        );
        Ok(())
    }
}
