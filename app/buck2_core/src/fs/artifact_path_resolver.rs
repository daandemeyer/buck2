/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

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
const CELL_SOURCES_V1_MAX_CELL_NAME_BYTES: usize = 126;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CanonicalSourcePathClass {
    ReservedBuckOut,
    Ordinary,
}
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
        Self {
            cell_resolver: buck_path_resolver,
            buck_out_path_resolver,
            project_filesystem,
            cell_source_path_mode,
        }
    }

    pub fn cell_source_path_mode(&self) -> CellSourcePathMode {
        self.cell_source_path_mode
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
                let origin = external.contains(&name.as_str()).then(|| {
                    ExternalCellOrigin::Git(GitCellSetup {
                        git_origin: std::sync::Arc::from(
                            format!("https://example.com/{name}.git").as_str(),
                        ),
                        commit: std::sync::Arc::from("0123456789abcdef0123456789abcdef01234567"),
                        object_format: None,
                    })
                });
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
        let project_root = ProjectRoot::new_unchecked(
            buck2_fs::paths::abs_norm_path::AbsNormPathBuf::new(
                std::path::Path::new(if cfg!(windows) {
                    "C:\\project"
                } else {
                    "/project"
                })
                .to_owned(),
            )
            .unwrap(),
        );
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
                self.validate_canonical_cell_layout(path.cell())?;
                if self.classify_source_cell_path(path.path())
                    == CanonicalSourcePathClass::ReservedBuckOut
                {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "Source path `{path}` is beneath the reserved top-level `buck-out` subtree in `canonical_v1`",
                    ));
                }
                Ok(self
                    .resolve_cell_source_root_for_execution(path.cell())?
                    .join(path.path()))
            }
        }
    }

    /// Runs the lexical checks eagerly so a misconfigured workspace fails before any source
    /// `ArtifactValue`, Action, or upload is constructed, rather than at first lazy resolution.
    pub fn validate_canonical_layout(&self) -> buck2_error::Result<()> {
        if self.cell_source_path_mode == CellSourcePathMode::Physical {
            return Ok(());
        }
        for (cell, _) in self.cell_resolver.cells() {
            self.validate_canonical_cell_layout(cell)?;
        }
        Ok(())
    }

    fn validate_canonical_cell_layout(&self, cell: CellName) -> buck2_error::Result<()> {
        self.validate_canonical_buck_out_root()?;
        self.resolve_cell_source_root_for_execution(cell)?;
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
            if let Some(first) = physical_root.iter().next() {
                if self.classify_source_top_level_name(first)
                    == CanonicalSourcePathClass::ReservedBuckOut
                {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Input,
                        "Local cell `{cell}` has physical source root `{physical_root}` beneath reserved top-level `buck-out` in `canonical_v1`",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn classify_source_cell_path(&self, path: &CellRelativePath) -> CanonicalSourcePathClass {
        match path.iter().next() {
            Some(component)
                if self.classify_source_top_level_name(component)
                    == CanonicalSourcePathClass::ReservedBuckOut =>
            {
                CanonicalSourcePathClass::ReservedBuckOut
            }
            _ => CanonicalSourcePathClass::Ordinary,
        }
    }

    pub fn classify_source_top_level_name(&self, name: &FileName) -> CanonicalSourcePathClass {
        if source_component_eq(name.as_str(), "buck-out") {
            CanonicalSourcePathClass::ReservedBuckOut
        } else {
            CanonicalSourcePathClass::Ordinary
        }
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
        let relative = ProjectRelativePathBuf::unchecked_new(
            namespace_components.as_path().as_str().to_owned(),
        );
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
                "Malformed canonical cell execution path `{path}`: invalid cell-name encoding: {e}"
            )
        })?;
        let name = std::str::from_utf8(&name_bytes).map_err(|e| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Malformed canonical cell execution path `{path}`: cell name is not UTF-8: {e}"
            )
        })?;
        if name_bytes.len() > CELL_SOURCES_V1_MAX_CELL_NAME_BYTES
            || format!(
                "{CELL_SOURCES_V1_COMPONENT_PREFIX}{}",
                hex::encode(&name_bytes)
            ) != encoded_component.as_str()
        {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Malformed canonical cell execution path `{path}`: cell-name encoding is not canonical"
            ));
        }
        let cell = CellName::unchecked_new(name)?;
        self.cell_resolver.get(cell).map_err(|e| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Canonical cell execution path `{path}` names cell `{cell}`, which is not configured: {e}"
            )
        })?;
        let mut components = relative.iter();
        let _encoded = components.next().expect("checked non-empty path");
        let suffix = CellRelativePath::new(components.as_path()).to_buf();
        if self.classify_source_cell_path(&suffix) == CanonicalSourcePathClass::ReservedBuckOut {
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
        let name = cell.as_str();
        if name.len() > CELL_SOURCES_V1_MAX_CELL_NAME_BYTES {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Canonical cell name `{name}` is {} UTF-8 bytes; `canonical_v1` supports at most {CELL_SOURCES_V1_MAX_CELL_NAME_BYTES} bytes so its execution path remains portable",
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

fn strip_project_prefix_for_source_identity(
    path: &crate::fs::project_rel_path::ProjectRelativePath,
    prefix: &crate::fs::project_rel_path::ProjectRelativePath,
) -> Option<ProjectRelativePathBuf> {
    if let Some(suffix) = path.strip_prefix_opt(prefix) {
        return Some(ProjectRelativePathBuf::unchecked_new(
            suffix.as_str().to_owned(),
        ));
    }
    if !project_path_has_prefix(path, prefix) {
        return None;
    }

    let mut components = path.iter();
    for _ in prefix.iter() {
        components
            .next()
            .expect("prefix was checked component-wise");
    }
    Some(ProjectRelativePathBuf::unchecked_new(
        components.as_path().as_str().to_owned(),
    ))
}

fn project_path_has_prefix(
    path: &crate::fs::project_rel_path::ProjectRelativePath,
    prefix: &crate::fs::project_rel_path::ProjectRelativePath,
) -> bool {
    let mut components = path.iter();
    prefix.iter().all(|expected| {
        components
            .next()
            .is_some_and(|actual| source_component_eq(actual.as_str(), expected.as_str()))
    })
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
    use super::ArtifactFs;
    use super::CellSourcePathMode;
    use crate::cells::cell_path::CellPath;
    use crate::cells::name::CellName;
    use crate::cells::paths::CellRelativePathBuf;
    use crate::fs::buck_out_path::BuckOutPathResolver;
    use crate::fs::project_rel_path::ProjectRelativePath;
    use crate::fs::project_rel_path::ProjectRelativePathBuf;
    use crate::package::source_path::SourcePath;

    fn external_artifact_fs(mode: CellSourcePathMode) -> buck2_error::Result<ArtifactFs> {
        Ok(ArtifactFs::testing_new_with_mode_and_external(
            mode,
            &[("root", ""), ("sample", "declared/sample")],
            &["sample"],
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
}
