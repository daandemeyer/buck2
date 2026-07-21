/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::AtomicU64;
#[cfg(unix)]
use std::sync::atomic::Ordering;
#[cfg(unix)]
use std::time::SystemTime;

use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::external::ExternalCellOrigin;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_core::fs::artifact_path_resolver::ArtifactFs;
use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
use buck2_error::BuckErrorContext;
use buck2_execute::execute::cell_execution_view::CellExecutionView;
use buck2_execute::execute::cell_execution_view::CellExecutionViewRequirements;
use parking_lot::Mutex;

#[cfg(unix)]
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const OWNER_MAGIC: &[u8] = b"buck2-cell-execution-view-v1\0";

/// Each action-visible `c_*` is a real directory of links to the represented top-level source
/// entries: that works for the project root without linking Buck-out back into itself, and gives
/// root and non-root cells one local shape.
pub struct CanonicalCellExecutionView {
    state: Mutex<()>,
}

impl CanonicalCellExecutionView {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(()),
        }
    }

    fn prepare_requirements(
        &self,
        artifact_fs: &ArtifactFs,
        requirements: &CellExecutionViewRequirements,
    ) -> buck2_error::Result<()> {
        let prepared = PreparedRequirements::new(artifact_fs, requirements)?;
        let _guard = self.state.lock();

        // Preflight all source paths before creating any view entry. Namespace validation itself
        // is intentionally delayed until after this side-effect-free pass.
        prepared.preflight()?;
        for cell in &prepared.cells {
            cell.prepare()?;
        }
        Ok(())
    }
}

impl Default for CanonicalCellExecutionView {
    fn default() -> Self {
        Self::new()
    }
}

impl CellExecutionView for CanonicalCellExecutionView {
    fn prepare(
        &self,
        artifact_fs: &ArtifactFs,
        requirements: &CellExecutionViewRequirements,
    ) -> buck2_error::Result<()> {
        self.prepare_requirements(artifact_fs, requirements)
    }
}

struct PreparedRequirements {
    cells: Vec<PreparedCell>,
}

impl PreparedRequirements {
    fn new(
        artifact_fs: &ArtifactFs,
        requirements: &CellExecutionViewRequirements,
    ) -> buck2_error::Result<Self> {
        let mut cells = Vec::new();
        for (cell, requirements) in requirements.iter() {
            // Recheck the physical topology at the local handoff boundary.
            let physical = artifact_fs.resolve_cell_source_root_for_consumption(cell)?;
            let logical = artifact_fs.resolve_cell_path_for_execution(
                CellPath::new(cell, CellRelativePathBuf::unchecked_new(String::new())).as_ref(),
            )?;
            let entries = requirements.top_level_entries().cloned().collect();
            let empty_directories = requirements
                .empty_directories()
                .filter(|path| !path.is_empty())
                .cloned()
                .collect();
            let bundled = matches!(
                artifact_fs.cell_resolver().get(cell)?.external(),
                Some(ExternalCellOrigin::Bundled(_))
            );
            cells.push(PreparedCell {
                cell,
                physical,
                logical,
                entries,
                empty_directories,
                bundled,
                project_root: artifact_fs.fs().root().as_path().to_path_buf(),
            });
        }
        Ok(Self { cells })
    }

    fn preflight(&self) -> buck2_error::Result<()> {
        for cell in &self.cells {
            cell.preflight()?;
        }
        Ok(())
    }
}

struct PreparedCell {
    cell: CellName,
    physical: ProjectRelativePathBuf,
    logical: ProjectRelativePathBuf,
    entries: Vec<CellRelativePathBuf>,
    empty_directories: Vec<CellRelativePathBuf>,
    bundled: bool,
    project_root: PathBuf,
}

impl PreparedCell {
    fn physical_abs(&self) -> PathBuf {
        self.project_root.join(self.physical.as_str())
    }

    fn logical_abs(&self) -> PathBuf {
        self.project_root.join(self.logical.as_str())
    }

    fn preflight(&self) -> buck2_error::Result<()> {
        let physical = self.physical_abs();
        let metadata = fs::metadata(&physical).buck_error_context(format!(
            "Physical source root `{}` for cell `{}` is not available",
            self.physical, self.cell
        ))?;
        if !metadata.is_dir() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Physical source root `{}` for cell `{}` is not a directory",
                self.physical,
                self.cell,
            ));
        }
        for directory in &self.empty_directories {
            let source = physical.join(directory.as_str());
            match fs::symlink_metadata(&source) {
                Ok(metadata) if is_plain_directory(&metadata) => {}
                Ok(_) => return Err(unexpected_object(&source, "empty source directory")),
                Err(e) if e.kind() == io::ErrorKind::NotFound && self.bundled => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(e).buck_error_context(format!(
                        "Empty source directory `{}` is missing from non-bundled cell `{}`",
                        directory, self.cell
                    ));
                }
                Err(e) => {
                    return Err(e).buck_error_context(format!(
                        "Failed to inspect empty source directory `{}` in cell `{}`",
                        directory, self.cell
                    ));
                }
            }
        }
        for entry in &self.entries {
            if entry.iter().count() != 1 {
                return Err(buck2_error::buck2_error!(
                    buck2_error::ErrorTag::Tier0,
                    "Cell execution view requirement `{}` is not a top-level entry",
                    CellPath::new(self.cell, entry.clone()),
                ));
            }
            let source = physical.join(entry.as_str());
            match fs::symlink_metadata(&source) {
                Ok(_) => {}
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound
                        && self.bundled
                        && self
                            .empty_directories
                            .iter()
                            .any(|directory| directory.starts_with(entry)) => {}
                Err(e) => {
                    return Err(e).buck_error_context(format!(
                        "Canonical source view entry `{}` is missing from physical cell `{}`",
                        entry, self.cell
                    ));
                }
            }
        }
        Ok(())
    }

    fn prepare(&self) -> buck2_error::Result<()> {
        if self.bundled {
            for directory in &self.empty_directories {
                ensure_bundled_directory(&self.physical_abs(), directory)?;
            }
        }

        let logical = self.logical_abs();
        let namespace = Namespace::from_logical_root(&logical)?;
        namespace.ensure(self.cell, &self.physical)?;
        for entry in &self.entries {
            let physical_entry = self.physical_abs().join(entry.as_str());
            let logical_entry = logical.join(entry.as_str());
            prepare_plant(&physical_entry, &logical_entry).map_err(|e| {
                e.context(format!(
                    "Failed to prepare canonical source view entry `{}` for cell `{}`",
                    entry, self.cell
                ))
            })?;
        }
        Ok(())
    }
}

/// A bundled directory value with no leaves has nothing for the materializer to realize. Each
/// component is walked without following links so bundle data cannot redirect this mutation
/// outside its physical expansion root.
fn ensure_bundled_directory(
    physical_root: &Path,
    relative: &CellRelativePathBuf,
) -> buck2_error::Result<()> {
    let mut current = physical_root.to_path_buf();
    for component in relative.iter() {
        current.push(component.as_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_plain_directory(&metadata) => {}
            Ok(_) => return Err(unexpected_object(&current, "bundled source directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&current) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&current).buck_error_context(
                        "Failed to inspect concurrently-created bundled source directory",
                    )?;
                    if !is_plain_directory(&metadata) {
                        return Err(unexpected_object(&current, "bundled source directory"));
                    }
                }
                Err(e) => {
                    return Err(e).buck_error_context(format!(
                        "Failed to recreate bundled empty source directory `{}`",
                        current.display()
                    ));
                }
            },
            Err(e) => {
                return Err(e).buck_error_context(format!(
                    "Failed to inspect bundled source directory `{}`",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

struct Namespace<'a> {
    v1: &'a Path,
    cell_sources: &'a Path,
    logical: &'a Path,
    owners: PathBuf,
}

impl<'a> Namespace<'a> {
    fn from_logical_root(logical: &'a Path) -> buck2_error::Result<Self> {
        let v1 = logical.parent().ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "Canonical cell root has no parent"
            )
        })?;
        let cell_sources = v1.parent().ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "Canonical cell namespace has no parent"
            )
        })?;
        Ok(Self {
            v1,
            cell_sources,
            logical,
            owners: v1.join(".owners"),
        })
    }

    fn ensure(&self, cell: CellName, physical: &ProjectRelativePathBuf) -> buck2_error::Result<()> {
        let buck_out = self.cell_sources.parent().ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "Canonical cell namespace is outside Buck-out"
            )
        })?;
        fs::create_dir_all(buck_out).buck_error_context("Failed to create Buck-out root")?;
        ensure_plain_directory(self.cell_sources)?;
        ensure_plain_directory(self.v1)?;
        ensure_plain_directory(&self.owners)?;

        let name = self.logical.file_name().ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "Canonical cell root has no name"
            )
        })?;
        let owner = self.owners.join(name);
        let expected = owner_contents(cell, physical);
        let expected_prefix = owner_prefix(cell);
        match fs::symlink_metadata(&owner) {
            Ok(metadata) => {
                if !is_plain_file(&metadata) {
                    return Err(unexpected_object(&owner, "owner record"));
                }
                let actual = fs::read(&owner)
                    .buck_error_context("Failed to read canonical cell owner record")?;
                if !actual.starts_with(&expected_prefix)
                    || actual.len() <= expected_prefix.len()
                    || actual.last() != Some(&0)
                {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Environment,
                        "Canonical cell owner record `{}` is not owned by cell `{}`; run `buck2 clean`",
                        owner.display(),
                        cell
                    ));
                }
                if actual != expected {
                    // Live rebinding on a topology change (replacing the owner record and
                    // republishing the forest) is not implemented yet; fail closed rather
                    // than serve a view of the previous physical root.
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Environment,
                        "Canonical cell owner record `{}` was created for a different cell topology; run `buck2 clean`",
                        owner.display()
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(self.logical).is_ok() {
                    return Err(buck2_error::buck2_error!(
                        buck2_error::ErrorTag::Environment,
                        "Refusing to adopt unowned canonical cell directory `{}`; run `buck2 clean`",
                        self.logical.display()
                    ));
                }
                create_owner_record(&owner, &expected)?;
            }
            Err(e) => {
                return Err(e).buck_error_context("Failed to inspect canonical cell owner record");
            }
        }

        match fs::symlink_metadata(self.logical) {
            Ok(metadata) if is_plain_directory(&metadata) => Ok(()),
            Ok(_) => Err(unexpected_object(self.logical, "cell directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(self.logical)
                .buck_error_context("Failed to create canonical cell directory"),
            Err(e) => Err(e).buck_error_context("Failed to inspect canonical cell directory"),
        }
    }
}

fn owner_prefix(cell: CellName) -> Vec<u8> {
    let mut contents = Vec::with_capacity(OWNER_MAGIC.len() + cell.as_str().len() + 1);
    contents.extend_from_slice(OWNER_MAGIC);
    contents.extend_from_slice(cell.as_str().as_bytes());
    contents.push(0);
    contents
}

fn owner_contents(cell: CellName, physical: &ProjectRelativePathBuf) -> Vec<u8> {
    let mut contents = owner_prefix(cell);
    contents.extend_from_slice(physical.as_str().as_bytes());
    contents.push(0);
    contents
}

fn ensure_plain_directory(path: &Path) -> buck2_error::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_plain_directory(&metadata) => Ok(()),
        Ok(_) => Err(unexpected_object(path, "namespace directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(path)
                    .buck_error_context("Failed to inspect concurrently-created namespace")?;
                if is_plain_directory(&metadata) {
                    Ok(())
                } else {
                    Err(unexpected_object(path, "namespace directory"))
                }
            }
            Err(e) => Err(e).buck_error_context("Failed to create canonical cell namespace"),
        },
        Err(e) => Err(e).buck_error_context("Failed to inspect canonical cell namespace"),
    }
}

#[cfg(unix)]
fn is_plain_directory(metadata: &fs::Metadata) -> bool {
    metadata.is_dir() && !metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn is_plain_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink()
}

fn unexpected_object(path: &Path, role: &str) -> buck2_error::Error {
    buck2_error::buck2_error!(
        buck2_error::ErrorTag::Environment,
        "Canonical cell {role} `{}` has an unexpected type; run `buck2 clean`",
        path.display()
    )
}

fn create_owner_record(path: &Path, contents: &[u8]) -> buck2_error::Result<()> {
    // Owner records are only ever created fresh: a torn or lost record fails
    // validation on the next read and fails closed into clean-required.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .buck_error_context("Failed to create canonical cell owner record")?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .buck_error_context("Failed to write canonical cell owner record")
}

#[cfg(unix)]
fn prepare_plant(physical: &Path, logical: &Path) -> buck2_error::Result<()> {
    let metadata = fs::symlink_metadata(physical)
        .buck_error_context("Failed to inspect physical source entry")?;
    let target = if metadata.file_type().is_symlink() {
        let target =
            fs::read_link(physical).buck_error_context("Failed to read physical source symlink")?;
        if target.is_absolute() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Absolute source symlink `{}` is not supported by canonical cell execution paths",
                physical.display()
            ));
        }
        target
    } else if metadata.is_file() || metadata.is_dir() {
        physical.to_path_buf()
    } else {
        return Err(unexpected_object(physical, "source entry"));
    };

    match fs::symlink_metadata(logical) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::read_link(logical)? == target {
                Ok(())
            } else {
                create_posix_symlink_atomic(&target, logical)
            }
        }
        Ok(_) => Err(unexpected_object(logical, "view entry")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            create_posix_symlink_atomic(&target, logical)
        }
        Err(e) => Err(e).buck_error_context("Failed to inspect canonical view entry"),
    }
}

#[cfg(unix)]
fn create_posix_symlink_atomic(target: &Path, link: &Path) -> buck2_error::Result<()> {
    let parent = link.parent().ok_or_else(|| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Tier0,
            "Canonical source view link has no parent"
        )
    })?;
    for _ in 0..32 {
        let temp = unique_sibling(parent, "tmp");
        match std::os::unix::fs::symlink(target, &temp) {
            Ok(()) => {
                return match fs::rename(&temp, link) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ignored = fs::remove_file(&temp);
                        Err(e).buck_error_context("Failed to publish canonical source view link")
                    }
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).buck_error_context("Failed to create canonical source view link");
            }
        }
    }
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Environment,
        "Could not allocate a temporary canonical source view link"
    ))
}

// Canonical cell execution paths are rejected at daemon startup on Windows, so these
// entry points are unreachable there; they exist only to keep this module compiling
// until Windows support (junctions and file symlinks) lands.
#[cfg(windows)]
fn windows_unsupported() -> buck2_error::Error {
    buck2_error::buck2_error!(
        buck2_error::ErrorTag::Environment,
        "canonical cell execution paths are not yet supported on Windows"
    )
}

#[cfg(windows)]
fn prepare_plant(_physical: &Path, _logical: &Path) -> buck2_error::Result<()> {
    Err(windows_unsupported())
}

#[cfg(windows)]
fn is_plain_directory(metadata: &fs::Metadata) -> bool {
    metadata.is_dir() && !metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_plain_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn unique_sibling(parent: &Path, role: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(
        ".{role}_{timestamp:032x}_{:016x}",
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use buck2_core::cells::CellAliasResolver;
    use buck2_core::cells::CellResolver;
    use buck2_core::cells::cell_root_path::CellRootPathBuf;
    use buck2_core::cells::instance::CellInstance;
    use buck2_core::cells::nested::NestedCells;
    use buck2_core::fs::artifact_path_resolver::CellSourcePathMode;
    use buck2_core::fs::buck_out_path::BuckOutPathResolver;
    use buck2_core::fs::project::ProjectRoot;
    use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
    use tempfile::TempDir;

    use super::*;

    const LOGICAL: &str = "buck-out/v2/cell_sources/v1/c_73616d706c65";

    fn artifact_fs_with_origin(
        project: &TempDir,
        sample_root: &str,
        origin: Option<ExternalCellOrigin>,
    ) -> buck2_error::Result<ArtifactFs> {
        let root_name = CellName::testing_new("root");
        let sample_name = CellName::testing_new("sample");
        let root_path = CellRootPathBuf::testing_new("");
        let sample_path = CellRootPathBuf::testing_new(sample_root);
        let roots = [
            (root_name, root_path.as_path()),
            (sample_name, sample_path.as_path()),
        ];
        let root = CellInstance::new(
            root_name,
            root_path.clone(),
            None,
            NestedCells::from_cell_roots(&roots, &root_path),
        )?;
        let sample = CellInstance::new(
            sample_name,
            sample_path.clone(),
            origin,
            NestedCells::from_cell_roots(&roots, &sample_path),
        )?;
        let cells = CellResolver::new(
            vec![root, sample],
            CellAliasResolver::new(root_name, Default::default())?,
        )?;
        Ok(ArtifactFs::new_with_cell_source_path_mode(
            cells,
            BuckOutPathResolver::new(ProjectRelativePathBuf::unchecked_new("buck-out/v2".into())),
            ProjectRoot::new_unchecked(
                AbsNormPathBuf::new(project.path().to_owned()).expect("absolute temp directory"),
            ),
            CellSourcePathMode::CanonicalV1,
        ))
    }

    fn artifact_fs(project: &TempDir, sample_root: &str) -> buck2_error::Result<ArtifactFs> {
        artifact_fs_with_origin(project, sample_root, None)
    }

    fn bundled_artifact_fs(project: &TempDir) -> buck2_error::Result<ArtifactFs> {
        let sample = CellName::testing_new("sample");
        artifact_fs_with_origin(
            project,
            "declared/sample",
            Some(ExternalCellOrigin::Bundled(sample)),
        )
    }

    fn requirements(entries: &[&str]) -> CellExecutionViewRequirements {
        requirements_with_empty(entries, &[])
    }

    fn requirements_with_empty(
        entries: &[&str],
        empty_directories: &[&str],
    ) -> CellExecutionViewRequirements {
        let cell = CellName::testing_new("sample");
        let mut requirements = CellExecutionViewRequirements::default();
        requirements.add_cell(cell);
        for entry in entries {
            requirements.add_top_level_entry(CellPath::new(
                cell,
                CellRelativePathBuf::unchecked_new((*entry).to_owned()),
            ));
        }
        for entry in empty_directories {
            requirements.add_empty_directory(CellPath::new(
                cell,
                CellRelativePathBuf::unchecked_new((*entry).to_owned()),
            ));
        }
        requirements
    }

    #[test]
    fn creates_real_sparse_cell_directory() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a/src"))?;
        fs::write(project.path().join("sample-a/LICENSE"), "license")?;
        let view = CanonicalCellExecutionView::new();
        view.prepare(
            &artifact_fs(&project, "sample-a")?,
            &requirements(&["src", "LICENSE"]),
        )?;

        let metadata = fs::symlink_metadata(project.path().join(LOGICAL))?;
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
        assert!(
            fs::symlink_metadata(project.path().join(LOGICAL).join("src"))?
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(project.path().join(LOGICAL).join("LICENSE"))?,
            "license"
        );
        assert!(
            project
                .path()
                .join("buck-out/v2/cell_sources/v1/.owners/c_73616d706c65")
                .is_file()
        );
        Ok(())
    }

    #[test]
    fn preserves_empty_cell_presence() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a"))?;
        CanonicalCellExecutionView::new()
            .prepare(&artifact_fs(&project, "sample-a")?, &requirements(&[]))?;
        assert!(project.path().join(LOGICAL).is_dir());
        assert_eq!(fs::read_dir(project.path().join(LOGICAL))?.count(), 0);
        Ok(())
    }

    #[test]
    fn recreates_relative_source_symlink() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a/dir"))?;
        fs::write(project.path().join("sample-a/dir/file"), "bytes")?;
        std::os::unix::fs::symlink("dir", project.path().join("sample-a/link"))?;
        CanonicalCellExecutionView::new().prepare(
            &artifact_fs(&project, "sample-a")?,
            &requirements(&["dir", "link"]),
        )?;
        assert_eq!(
            fs::read_link(project.path().join(LOGICAL).join("link"))?,
            PathBuf::from("dir")
        );
        assert_eq!(
            fs::read_to_string(project.path().join(LOGICAL).join("link/file"))?,
            "bytes"
        );
        Ok(())
    }

    #[test]
    fn refuses_unowned_existing_cell_directory() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a"))?;
        fs::create_dir_all(project.path().join(LOGICAL))?;
        let error = CanonicalCellExecutionView::new()
            .prepare(&artifact_fs(&project, "sample-a")?, &requirements(&[]))
            .expect_err("must not adopt an unowned directory");
        assert!(error.to_string().contains("unowned"));
        Ok(())
    }

    #[test]
    fn owner_mismatch_requires_clean() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a"))?;
        fs::create_dir_all(project.path().join("sample-b"))?;
        fs::write(project.path().join("sample-a/LICENSE"), "a")?;
        let view = CanonicalCellExecutionView::new();
        view.prepare(
            &artifact_fs(&project, "sample-a")?,
            &requirements(&["LICENSE"]),
        )?;
        let error = view
            .prepare(&artifact_fs(&project, "sample-b")?, &requirements(&[]))
            .expect_err("a topology change must require a clean");
        assert!(error.to_string().contains("different cell topology"));
        assert!(error.to_string().contains("buck2 clean"));
        // The published view keeps serving the recorded topology.
        assert_eq!(
            fs::read_to_string(project.path().join(LOGICAL).join("LICENSE"))?,
            "a"
        );
        Ok(())
    }

    #[test]
    fn recreates_bundled_empty_directories_only() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        let physical = project
            .path()
            .join("buck-out/v2/external_cells/bundled/sample");
        fs::create_dir_all(&physical)?;
        CanonicalCellExecutionView::new().prepare(
            &bundled_artifact_fs(&project)?,
            &requirements_with_empty(&["assets"], &["assets/empty/nested"]),
        )?;
        assert!(physical.join("assets/empty/nested").is_dir());
        assert!(
            project
                .path()
                .join(LOGICAL)
                .join("assets/empty/nested")
                .is_dir()
        );

        let local = TempDir::new()?;
        fs::create_dir_all(local.path().join("sample-a"))?;
        CanonicalCellExecutionView::new()
            .prepare(
                &artifact_fs(&local, "sample-a")?,
                &requirements_with_empty(&["assets"], &["assets/empty"]),
            )
            .expect_err("local source directories must already exist");
        assert!(!local.path().join("sample-a/assets").exists());
        Ok(())
    }

    #[test]
    fn temporary_names_do_not_include_long_final_name() {
        let parent = Path::new("parent");
        let name = unique_sibling(parent, "tmp");
        assert!(name.file_name().unwrap().len() < 80);
    }
}
