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
use dupe::Dupe;

use crate::cells::CellResolver;
use crate::cells::cell_path::CellPathRef;
use crate::content_hash::ContentBasedPathHash;
use crate::fs::buck_out_path::BuckOutPathResolver;
use crate::fs::buck_out_path::BuildArtifactPath;
use crate::fs::project::ProjectRoot;
use crate::fs::project_rel_path::ProjectRelativePathBuf;
use crate::package::source_path::SourcePathRef;

#[derive(Clone, Allocative)]
pub struct ArtifactFs {
    cell_resolver: CellResolver,
    buck_out_path_resolver: BuckOutPathResolver,
    project_filesystem: ProjectRoot,
}

impl ArtifactFs {
    pub fn new(
        buck_path_resolver: CellResolver,
        buck_out_path_resolver: BuckOutPathResolver,
        project_filesystem: ProjectRoot,
    ) -> Self {
        Self {
            cell_resolver: buck_path_resolver,
            buck_out_path_resolver,
            project_filesystem,
        }
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
        self.resolve_cell_path(path)
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
        self.resolve_source(source_artifact_path)
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;

    use super::ArtifactFs;
    use crate::cells::CellAliasResolver;
    use crate::cells::CellResolver;
    use crate::cells::cell_root_path::CellRootPathBuf;
    use crate::cells::external::ExternalCellOrigin;
    use crate::cells::external::GitCellSetup;
    use crate::cells::instance::CellInstance;
    use crate::cells::name::CellName;
    use crate::cells::nested::NestedCells;
    use crate::fs::buck_out_path::BuckOutPathResolver;
    use crate::fs::project::ProjectRoot;
    use crate::fs::project_rel_path::ProjectRelativePath;
    use crate::fs::project_rel_path::ProjectRelativePathBuf;
    use crate::package::source_path::SourcePath;

    fn external_artifact_fs() -> buck2_error::Result<ArtifactFs> {
        let root_name = CellName::testing_new("root");
        let external_name = CellName::testing_new("sample");
        let root_path = CellRootPathBuf::testing_new("");
        let external_path = CellRootPathBuf::testing_new("declared/sample");
        let roots = [
            (root_name, root_path.as_path()),
            (external_name, external_path.as_path()),
        ];
        let cells = CellResolver::new(
            vec![
                CellInstance::new(
                    root_name,
                    root_path.clone(),
                    None,
                    NestedCells::from_cell_roots(&roots, &root_path),
                )?,
                CellInstance::new(
                    external_name,
                    external_path.clone(),
                    Some(ExternalCellOrigin::Git(GitCellSetup {
                        git_origin: Arc::from("https://example.com/sample.git"),
                        commit: Arc::from("0123456789abcdef0123456789abcdef01234567"),
                        object_format: None,
                    })),
                    NestedCells::from_cell_roots(&roots, &external_path),
                )?,
            ],
            CellAliasResolver::new(root_name, Default::default())?,
        )?;
        let project_root = ProjectRoot::new_unchecked(
            AbsNormPathBuf::new(
                Path::new(if cfg!(windows) {
                    "C:\\project"
                } else {
                    "/project"
                })
                .to_owned(),
            )
            .unwrap(),
        );
        Ok(ArtifactFs::new(
            cells,
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck-out/v2".into())),
            project_root,
        ))
    }

    #[test]
    fn execution_resolution_preserves_legacy_external_paths() -> buck2_error::Result<()> {
        let fs = external_artifact_fs()?;
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
}
