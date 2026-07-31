/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
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
use buck2_fs::fs_util::is_plain_directory;
use buck2_fs::fs_util::is_plain_file;
#[cfg(windows)]
use buck2_fs::fs_util::is_reparse_point;
use parking_lot::Mutex;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const OWNER_MAGIC: &[u8] = b"buck2-cell-execution-view-v1\0";

/// Each action-visible `c_*` is a real directory of links to the represented top-level source
/// entries: that works for the project root without linking Buck-out back into itself, and gives
/// root and non-root cells one local shape.
pub struct CanonicalCellExecutionView {
    /// Lets a fresh daemon tell first publication from republication, so it only clears a forest
    /// it has not yet handed to a consumer. The mutex also orders topology replacement.
    ///
    /// Keyed on execution name, not cell name, because that is what selects the `c_*` directory.
    /// Keying it on the cell name would make a rename under a pinned execution name look like a
    /// first use and clear a forest this daemon has already published to a live worker.
    prepared_cells: Mutex<BTreeSet<String>>,
}

impl CanonicalCellExecutionView {
    pub fn new() -> Self {
        Self {
            prepared_cells: Mutex::new(BTreeSet::new()),
        }
    }
}

impl CellExecutionView for CanonicalCellExecutionView {
    fn prepare(
        &self,
        artifact_fs: &ArtifactFs,
        requirements: &CellExecutionViewRequirements,
    ) -> buck2_error::Result<()> {
        let prepared = PreparedCell::plan(artifact_fs, requirements)?;
        let mut prepared_cells = self.prepared_cells.lock();

        // Preflight every source path before creating any view entry.
        for cell in &prepared {
            cell.preflight()?;
        }
        for cell in &prepared {
            let first_use_in_daemon = !prepared_cells.contains(&cell.execution_name);
            cell.prepare(first_use_in_daemon)?;
            prepared_cells.insert(cell.execution_name.clone());
        }
        Ok(())
    }
}

struct PreparedCell {
    cell: CellName,
    execution_name: String,
    physical: ProjectRelativePathBuf,
    logical: ProjectRelativePathBuf,
    entries: Vec<CellRelativePathBuf>,
    empty_directories: Vec<CellRelativePathBuf>,
    bundled: bool,
    project_root: PathBuf,
}

impl PreparedCell {
    fn plan(
        artifact_fs: &ArtifactFs,
        requirements: &CellExecutionViewRequirements,
    ) -> buck2_error::Result<Vec<PreparedCell>> {
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
                execution_name: artifact_fs.cell_execution_name(cell).to_owned(),
                physical,
                logical,
                entries,
                empty_directories,
                bundled,
                project_root: artifact_fs.fs().root().as_path().to_path_buf(),
            });
        }
        Ok(cells)
    }

    fn physical_abs(&self) -> PathBuf {
        self.project_root.join(self.physical.as_str())
    }

    fn logical_abs(&self) -> PathBuf {
        self.project_root.join(self.logical.as_str())
    }

    fn preflight(&self) -> buck2_error::Result<()> {
        let physical = self.physical_abs();
        let logical = self.logical_abs();
        let namespace = Namespace::from_logical_root(&logical)?;
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
        preflight_platform_path(&physical)?;
        preflight_platform_path(&logical)?;
        preflight_publication_siblings(&logical)?;
        preflight_platform_path(namespace.cell_sources)?;
        preflight_platform_path(namespace.v1)?;
        preflight_platform_path(&namespace.owners)?;
        let owner =
            namespace.owners.join(logical.file_name().ok_or_else(|| {
                buck2_error::internal_error!("Canonical cell root has no file name")
            })?);
        preflight_platform_path(&owner)?;
        preflight_publication_siblings(&owner)?;
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
            preflight_platform_path(&source)?;
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
            let source_metadata = match fs::symlink_metadata(&source) {
                Ok(metadata) => Some(metadata),
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound
                        && self.bundled
                        && self
                            .empty_directories
                            .iter()
                            .any(|directory| directory.starts_with(entry)) =>
                {
                    None
                }
                Err(e) => {
                    return Err(e).buck_error_context(format!(
                        "Canonical source view entry `{}` is missing from physical cell `{}`",
                        entry, self.cell
                    ));
                }
            };
            preflight_platform_path(&source)?;
            if source_metadata.is_none() || source_metadata.as_ref().is_some_and(is_plain_directory)
            {
                preflight_junction_target(&source)?;
            }
            let logical_entry = logical.join(entry.as_str());
            preflight_platform_path(&logical_entry)?;
            preflight_publication_siblings(&logical_entry)?;
        }
        Ok(())
    }

    fn prepare(&self, first_use_in_daemon: bool) -> buck2_error::Result<()> {
        if self.bundled {
            for directory in &self.empty_directories {
                ensure_bundled_directory(&self.physical_abs(), directory)?;
            }
        }

        let logical = self.logical_abs();
        let namespace = Namespace::from_logical_root(&logical)?;
        namespace.ensure(
            self.cell,
            &self.execution_name,
            &self.physical,
            first_use_in_daemon,
        )?;
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
        ensure_plain_directory(&current, "bundled source directory")?;
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

    /// The record identifies the forest by execution name rather than cell name, because the
    /// execution name is what selects this `c_*` directory. Keying it on the cell name instead
    /// would make renaming a cell while pinning its execution name a clean-required mismatch, and
    /// that rename is exactly what pinning exists to make free.
    fn ensure(
        &self,
        cell: CellName,
        execution_name: &str,
        physical: &ProjectRelativePathBuf,
        first_use_in_daemon: bool,
    ) -> buck2_error::Result<()> {
        let buck_out = self.cell_sources.parent().ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "Canonical cell namespace is outside Buck-out"
            )
        })?;
        fs::create_dir_all(buck_out).buck_error_context("Failed to create Buck-out root")?;
        ensure_plain_directory(self.cell_sources, "namespace directory")?;
        ensure_plain_directory(self.v1, "namespace directory")?;
        ensure_plain_directory(&self.owners, "namespace directory")?;

        let name = self.logical.file_name().ok_or_else(|| {
            buck2_error::buck2_error!(
                buck2_error::ErrorTag::Tier0,
                "Canonical cell root has no name"
            )
        })?;
        let owner = self.owners.join(name);
        let expected = owner_contents(execution_name, physical);
        let expected_prefix = owner_prefix(execution_name);
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
                        "Canonical cell owner record `{}` is not owned by execution name `{}` (cell `{}`); run `buck2 clean`",
                        owner.display(),
                        execution_name,
                        cell
                    ));
                }
                if actual != expected {
                    // Topology change. The order below — children, forest directory, owner
                    // record, then owner-before-mkdir republication — leaves every crash point
                    // in a recoverable state, never a directory without a matching owner.
                    match fs::symlink_metadata(self.logical) {
                        Ok(metadata) if is_plain_directory(&metadata) => {
                            clear_view_entries(self.logical)?;
                            fs::remove_dir(self.logical).buck_error_context(
                                "Failed to remove stale canonical cell directory",
                            )?;
                            // Without this, power-loss metadata reordering could persist the
                            // owner rewrite while the old `c_*` directory survives: the
                            // unowned-directory state the ordering above rules out.
                            #[cfg(unix)]
                            if let Some(parent) = self.logical.parent() {
                                if let Ok(dir) = fs::File::open(parent) {
                                    let _ignored = dir.sync_all();
                                }
                            }
                        }
                        Ok(_) => return Err(unexpected_object(self.logical, "cell directory")),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(e).buck_error_context(
                                "Failed to inspect stale canonical cell directory",
                            );
                        }
                    }
                    fs::remove_file(&owner)
                        .buck_error_context("Failed to remove stale canonical cell owner record")?;
                    create_file_atomic(&owner, &expected)?;
                } else if first_use_in_daemon {
                    // A fresh daemon has no admitted consumers yet, so clearing the owned forest
                    // is safe: it lets Windows recover plants the previous daemon could not
                    // atomically refresh.
                    clear_view_entries(self.logical)?;
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
                create_file_atomic(&owner, &expected)?;
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

fn owner_prefix(execution_name: &str) -> Vec<u8> {
    let mut contents = Vec::with_capacity(OWNER_MAGIC.len() + execution_name.len() + 1);
    contents.extend_from_slice(OWNER_MAGIC);
    contents.extend_from_slice(execution_name.as_bytes());
    contents.push(0);
    contents
}

fn owner_contents(execution_name: &str, physical: &ProjectRelativePathBuf) -> Vec<u8> {
    let mut contents = owner_prefix(execution_name);
    contents.extend_from_slice(physical.as_str().as_bytes());
    contents.push(0);
    contents
}

/// Tolerates a concurrent creator, but never adopts a link.
fn ensure_plain_directory(path: &Path, role: &str) -> buck2_error::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_plain_directory(&metadata) => Ok(()),
        Ok(_) => Err(unexpected_object(path, role)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(path).buck_error_context(
                    "Failed to inspect concurrently-created canonical cell directory",
                )?;
                if is_plain_directory(&metadata) {
                    Ok(())
                } else {
                    Err(unexpected_object(path, role))
                }
            }
            Err(e) => Err(e).buck_error_context("Failed to create canonical cell directory"),
        },
        Err(e) => Err(e).buck_error_context("Failed to inspect canonical cell directory"),
    }
}

fn clear_view_entries(path: &Path) -> buck2_error::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_plain_directory(&metadata) => {}
        Ok(_) => return Err(unexpected_object(path, "cell directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).buck_error_context("Failed to inspect canonical cell directory");
        }
    }
    for entry in fs::read_dir(path).buck_error_context("Failed to read canonical cell directory")? {
        let entry = entry.buck_error_context("Failed to read canonical cell directory entry")?;
        remove_view_entry(&entry.path())?;
    }
    Ok(())
}

#[cfg(unix)]
fn remove_view_entry(path: &Path) -> buck2_error::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .buck_error_context("Failed to inspect stale canonical source view entry")?;
    if !metadata.file_type().is_symlink() {
        return Err(unexpected_object(path, "view entry"));
    }
    fs::remove_file(path).buck_error_context("Failed to remove stale canonical source view entry")
}

#[cfg(windows)]
fn remove_view_entry(path: &Path) -> buck2_error::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .buck_error_context("Failed to inspect stale canonical source view entry")?;
    if !is_reparse_point(&metadata) {
        return Err(unexpected_object(path, "view entry"));
    }
    remove_windows_link(path)
}

fn unexpected_object(path: &Path, role: &str) -> buck2_error::Error {
    buck2_error::buck2_error!(
        buck2_error::ErrorTag::Environment,
        "Canonical cell {role} `{}` has an unexpected type; run `buck2 clean`",
        path.display()
    )
}

fn create_file_atomic(path: &Path, contents: &[u8]) -> buck2_error::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        buck2_error::buck2_error!(buck2_error::ErrorTag::Tier0, "Owner record has no parent")
    })?;
    for _ in 0..32 {
        let temp = unique_sibling(parent);
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(contents).and_then(|()| file.sync_all()) {
                    let _ignored = fs::remove_file(&temp);
                    return Err(e)
                        .buck_error_context("Failed to write canonical cell owner record");
                }
                drop(file);
                return match fs::rename(&temp, path) {
                    Ok(()) => {
                        // After power loss, a persisted `c_*` mkdir with a lost owner-record
                        // rename would be an unowned, clean-required forest.
                        #[cfg(unix)]
                        if let Ok(dir) = fs::File::open(parent) {
                            let _ignored = dir.sync_all();
                        }
                        Ok(())
                    }
                    Err(e) => {
                        let _ignored = fs::remove_file(&temp);
                        Err(e).buck_error_context("Failed to publish canonical cell owner record")
                    }
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).buck_error_context("Failed to create canonical cell owner record");
            }
        }
    }
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Environment,
        "Could not allocate a temporary canonical cell owner record"
    ))
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
        let temp = unique_sibling(parent);
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

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum WindowsPlant {
    Junction(PathBuf),
    FileSymlink(PathBuf),
    DirectorySymlink(PathBuf),
}

#[cfg(windows)]
fn prepare_plant(physical: &Path, logical: &Path) -> buck2_error::Result<()> {
    let desired = windows_plant(physical)?;
    match fs::symlink_metadata(logical) {
        Ok(metadata) if is_reparse_point(&metadata) => {
            if windows_plant_matches(logical, &desired)? {
                return Ok(());
            }
            Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Environment,
                "Canonical source view entry `{}` changed while its cell topology is still active; restart the Buck2 daemon or run `buck2 clean`",
                logical.display()
            ))
        }
        Ok(_) => Err(unexpected_object(logical, "view entry")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_windows_plant(&desired, logical),
        Err(e) => Err(e).buck_error_context("Failed to inspect canonical view entry"),
    }
}

#[cfg(windows)]
fn windows_plant(physical: &Path) -> buck2_error::Result<WindowsPlant> {
    let metadata = fs::symlink_metadata(physical)
        .buck_error_context("Failed to inspect physical source entry")?;
    if metadata.file_type().is_symlink() {
        if junction::exists(physical)
            .buck_error_context("Failed to classify physical source reparse point")?
        {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Source junction `{}` is not supported by canonical cell execution paths",
                physical.display()
            ));
        }
        let target = fs::read_link(physical)
            .buck_error_context("Unsupported or unreadable physical source reparse point")?;
        if target.is_absolute() {
            return Err(buck2_error::buck2_error!(
                buck2_error::ErrorTag::Input,
                "Absolute source symlink `{}` is not supported by canonical cell execution paths",
                physical.display()
            ));
        }
        let target_metadata = fs::metadata(physical).buck_error_context(
            "Dangling or racing Windows source symlink is not supported by canonical cell execution paths",
        )?;
        return if target_metadata.is_dir() {
            Ok(WindowsPlant::DirectorySymlink(target))
        } else if target_metadata.is_file() {
            Ok(WindowsPlant::FileSymlink(target))
        } else {
            Err(unexpected_object(physical, "source symlink target"))
        };
    }
    if metadata.is_dir() {
        Ok(WindowsPlant::Junction(physical.to_path_buf()))
    } else if metadata.is_file() {
        Ok(WindowsPlant::FileSymlink(physical.to_path_buf()))
    } else {
        Err(unexpected_object(physical, "source entry"))
    }
}

#[cfg(windows)]
fn windows_plant_matches(path: &Path, desired: &WindowsPlant) -> buck2_error::Result<bool> {
    match desired {
        WindowsPlant::Junction(expected) => {
            if !junction::exists(path)
                .buck_error_context("Failed to classify canonical source view junction")?
            {
                return Ok(false);
            }
            let actual = junction::get_target(path)
                .buck_error_context("Failed to read canonical source view junction")?;
            Ok(same_target(&actual, expected, path))
        }
        WindowsPlant::FileSymlink(expected) | WindowsPlant::DirectorySymlink(expected) => {
            if junction::exists(path)
                .buck_error_context("Failed to classify canonical source view symlink")?
            {
                return Ok(false);
            }
            // Windows refuses to open a file through a directory-flavored symlink and vice
            // versa, so the flavor must match even when the target text has not changed.
            {
                use std::os::windows::fs::FileTypeExt;
                let file_type = fs::symlink_metadata(path)
                    .buck_error_context("Failed to inspect canonical source view symlink")?
                    .file_type();
                let flavor_matches = match desired {
                    WindowsPlant::FileSymlink(_) => file_type.is_symlink_file(),
                    WindowsPlant::DirectorySymlink(_) => file_type.is_symlink_dir(),
                    WindowsPlant::Junction(_) => false,
                };
                if !flavor_matches {
                    return Ok(false);
                }
            }
            let actual = fs::read_link(path)
                .buck_error_context("Failed to read canonical source view symlink")?;
            Ok(same_target(&actual, expected, path))
        }
    }
}

#[cfg(windows)]
fn create_windows_plant(desired: &WindowsPlant, link: &Path) -> buck2_error::Result<()> {
    let parent = link.parent().ok_or_else(|| {
        buck2_error::buck2_error!(
            buck2_error::ErrorTag::Tier0,
            "Canonical source view link has no parent"
        )
    })?;
    for _ in 0..32 {
        let temp = unique_sibling(parent);
        let created = match desired {
            WindowsPlant::Junction(target) => junction::create(target, &temp),
            WindowsPlant::FileSymlink(target) => std::os::windows::fs::symlink_file(target, &temp),
            WindowsPlant::DirectorySymlink(target) => {
                std::os::windows::fs::symlink_dir(target, &temp)
            }
        };
        match created {
            Ok(()) => {
                return match fs::rename(&temp, link) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ignored = remove_windows_link(&temp);
                        Err(e).buck_error_context("Failed to publish canonical source view link")
                    }
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                // `junction::create` is `create_dir` plus setting the reparse point; a failing
                // reparse step leaves a plain `.tmp_*` directory, which the next daemon's forest
                // clear would escalate to a clean-required error.
                if remove_windows_link(&temp).is_err() {
                    let _ignored = fs::remove_dir(&temp);
                }
                return Err(e).buck_error_context(
                    "Failed to create canonical source view link; Windows file links require symlink capability",
                );
            }
        }
    }
    Err(buck2_error::buck2_error!(
        buck2_error::ErrorTag::Environment,
        "Could not allocate a temporary canonical source view link"
    ))
}

#[cfg(windows)]
fn remove_windows_link(path: &Path) -> buck2_error::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    if junction::exists(path).buck_error_context("Failed to classify stale view link")? {
        junction::delete(path).buck_error_context("Failed to detach stale view junction")?;
        return fs::remove_dir(path).buck_error_context("Failed to remove stale view junction");
    }
    let file_type = fs::symlink_metadata(path)
        .buck_error_context("Failed to inspect stale view symlink")?
        .file_type();
    if file_type.is_symlink_dir() {
        fs::remove_dir(path).buck_error_context("Failed to remove stale directory symlink")
    } else if file_type.is_symlink_file() {
        fs::remove_file(path).buck_error_context("Failed to remove stale file symlink")
    } else {
        Err(unexpected_object(path, "stale view link"))
    }
}

#[cfg(windows)]
fn same_target(actual: &Path, expected: &Path, link: &Path) -> bool {
    if actual.is_relative() && expected.is_relative() {
        // Plants preserve the raw target text: resolving one against the logical forest and the
        // other against physical storage would make two identical links appear different.
        return actual == expected;
    }
    let actual = if actual.is_absolute() {
        actual.to_owned()
    } else {
        match link.parent() {
            Some(parent) => parent.join(actual),
            None => return false,
        }
    };
    let expected = if expected.is_absolute() {
        expected.to_owned()
    } else {
        match link.parent() {
            Some(parent) => parent.join(expected),
            None => return false,
        }
    };
    if actual == expected {
        return true;
    }
    match (actual.canonicalize(), expected.canonicalize()) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => false,
    }
}

#[cfg(not(windows))]
fn preflight_platform_path(_path: &Path) -> buck2_error::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn preflight_junction_target(_path: &Path) -> buck2_error::Result<()> {
    Ok(())
}

fn preflight_publication_siblings(path: &Path) -> buck2_error::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        buck2_error::internal_error!("Canonical view publication path has no parent")
    })?;
    preflight_platform_path(&parent.join(".tmp_00000000000000000000000000000000_0000000000000000"))
}

#[cfg(windows)]
fn preflight_platform_path(path: &Path) -> buck2_error::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let length = path.as_os_str().encode_wide().count();
    if length >= 32_767 {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Environment,
            "Canonical source view path `{}` exceeds the Windows absolute path limit",
            path.display()
        ));
    }
    if path
        .components()
        .any(|component| component.as_os_str().encode_wide().count() > 255)
    {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Environment,
            "Canonical source view path `{}` contains a component longer than 255 UTF-16 code units",
            path.display()
        ));
    }
    if length >= 260 {
        // Buck2's own I/O uses `\\?\` paths, but spawned tools get raw ones, and tools without a
        // long-path manifest (notably cl.exe) fail on them whatever LongPathsEnabled says.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                "Canonical source view path `{}` exceeds 260 UTF-16 code units; \
                 tools that are not long-path aware may fail to open it",
                path.display()
            );
        });
    }
    Ok(())
}

#[cfg(windows)]
fn preflight_junction_target(path: &Path) -> buck2_error::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    // junction 2.0 stores an NT substitute name (`\\??\\` + target) and a printable target in the
    // 16 KiB reparse buffer, which with both terminators leaves 4,089 UTF-16 code units.
    if path.as_os_str().encode_wide().count() > 4_089 {
        return Err(buck2_error::buck2_error!(
            buck2_error::ErrorTag::Environment,
            "Canonical source directory target `{}` exceeds the Windows junction reparse-buffer limit",
            path.display()
        ));
    }
    Ok(())
}

fn unique_sibling(parent: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(
        ".tmp_{timestamp:032x}_{:016x}",
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn same_topology_mismatch_requires_restart_without_replacing_junction()
    -> buck2_error::Result<()> {
        let project = tempfile::tempdir()?;
        let physical_a = project.path().join("physical-a");
        let physical_b = project.path().join("physical-b");
        let logical = project.path().join("logical");
        fs::create_dir(&physical_a)?;
        fs::create_dir(&physical_b)?;
        junction::create(&physical_a, &logical)?;

        let error = prepare_plant(&physical_b, &logical)
            .expect_err("a published Windows plant must not be replaced without quiescence");
        assert!(error.to_string().contains("restart the Buck2 daemon"));
        assert_eq!(
            junction::get_target(&logical)?.canonicalize()?,
            physical_a.canonicalize()?
        );
        Ok(())
    }

    #[test]
    fn fresh_daemon_owner_check_recovers_stale_junction() -> buck2_error::Result<()> {
        let project = tempfile::tempdir()?;
        let physical_a = project.path().join("physical-a");
        let physical_b = project.path().join("physical-b");
        let logical_root = project
            .path()
            .join("buck-out/v2/cell_sources/v1/c_73616d706c65");
        fs::create_dir(&physical_a)?;
        fs::create_dir(&physical_b)?;
        let namespace = Namespace::from_logical_root(&logical_root)?;
        let cell = CellName::testing_new("sample");
        let physical_owner = ProjectRelativePathBuf::unchecked_new("physical".to_owned());
        namespace.ensure(cell, &physical_owner, true)?;
        let logical = logical_root.join("src");
        prepare_plant(&physical_a, &logical)?;

        namespace.ensure(cell, &physical_owner, false)?;
        assert!(prepare_plant(&physical_b, &logical).is_err());
        namespace.ensure(cell, &physical_owner, true)?;
        prepare_plant(&physical_b, &logical)?;
        assert_eq!(
            junction::get_target(&logical)?.canonicalize()?,
            physical_b.canonicalize()?
        );
        Ok(())
    }

    #[test]
    fn relative_source_symlink_plants_are_idempotent() -> buck2_error::Result<()> {
        use std::os::windows::fs::symlink_dir;
        use std::os::windows::fs::symlink_file;

        let project = tempfile::tempdir()?;
        let physical = project.path().join("physical");
        let logical = project.path().join("logical");
        fs::create_dir(&physical)?;
        fs::create_dir(&logical)?;
        fs::write(physical.join("file-target"), b"file")?;
        fs::create_dir(physical.join("dir-target"))?;
        if let Err(error) = symlink_file("file-target", physical.join("file-link")) {
            if error.kind() == io::ErrorKind::PermissionDenied {
                return Ok(());
            }
            return Err(error.into());
        }
        symlink_dir("dir-target", physical.join("dir-link"))?;

        for name in ["file-link", "dir-link"] {
            let source = physical.join(name);
            let plant = logical.join(name);
            prepare_plant(&source, &plant)?;
            prepare_plant(&source, &plant)?;
            assert_eq!(fs::read_link(plant)?, fs::read_link(source)?);
        }
        Ok(())
    }

    #[test]
    fn namespace_validation_rejects_junctions() -> buck2_error::Result<()> {
        let project = tempfile::tempdir()?;
        let target = project.path().join("target");
        let namespace = project.path().join("cell_sources");
        fs::create_dir(&target)?;
        junction::create(&target, &namespace)?;

        assert!(ensure_plain_directory(&namespace, "namespace directory").is_err());
        assert!(target.exists());
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use buck2_core::fs::artifact_path_resolver::CellSourcePathMode;
    use buck2_core::fs::project::ProjectRoot;
    use buck2_fs::paths::abs_norm_path::AbsNormPathBuf;
    use tempfile::TempDir;

    use super::*;

    const LOGICAL: &str = "buck-out/v2/cell_sources/v1/c_73616d706c65";

    fn artifact_fs_in(project: &TempDir, sample_root: &str, bundled: &[&str]) -> ArtifactFs {
        ArtifactFs::testing_new_full(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), ("sample", sample_root)],
            &[],
            bundled,
            Some(ProjectRoot::new_unchecked(
                AbsNormPathBuf::new(project.path().to_owned()).expect("absolute temp directory"),
            )),
        )
    }

    fn artifact_fs(project: &TempDir, sample_root: &str) -> ArtifactFs {
        artifact_fs_in(project, sample_root, &[])
    }

    fn bundled_artifact_fs(project: &TempDir) -> ArtifactFs {
        artifact_fs_in(project, "declared/sample", &["sample"])
    }

    fn renamed_artifact_fs(project: &TempDir, sample_root: &str) -> ArtifactFs {
        ArtifactFs::testing_new_full(
            CellSourcePathMode::CanonicalV1,
            &[("root", ""), ("sample_v2", sample_root)],
            &[],
            &[],
            Some(ProjectRoot::new_unchecked(
                AbsNormPathBuf::new(project.path().to_owned()).expect("absolute temp directory"),
            )),
        )
        .testing_with_execution_names(&[("sample_v2", "sample")])
        .expect("execution names")
    }

    fn requirements(entries: &[&str]) -> CellExecutionViewRequirements {
        requirements_for(CellName::testing_new("sample"), entries, &[])
    }

    fn requirements_with_empty(
        entries: &[&str],
        empty_directories: &[&str],
    ) -> CellExecutionViewRequirements {
        requirements_for(CellName::testing_new("sample"), entries, empty_directories)
    }

    fn requirements_for(
        cell: CellName,
        entries: &[&str],
        empty_directories: &[&str],
    ) -> CellExecutionViewRequirements {
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
            &artifact_fs(&project, "sample-a"),
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
            .prepare(&artifact_fs(&project, "sample-a"), &requirements(&[]))?;
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
            &artifact_fs(&project, "sample-a"),
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
            .prepare(&artifact_fs(&project, "sample-a"), &requirements(&[]))
            .expect_err("must not adopt an unowned directory");
        assert!(error.to_string().contains("unowned"));
        Ok(())
    }

    #[test]
    fn replaces_physical_root_for_new_topology() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a"))?;
        fs::create_dir_all(project.path().join("sample-b"))?;
        fs::create_dir_all(project.path().join("sample-a/stale"))?;
        fs::write(project.path().join("sample-a/LICENSE"), "a")?;
        fs::write(project.path().join("sample-b/LICENSE"), "b")?;
        let view = CanonicalCellExecutionView::new();
        view.prepare(
            &artifact_fs(&project, "sample-a"),
            &requirements(&["LICENSE", "stale"]),
        )?;
        assert_eq!(
            fs::read_to_string(project.path().join(LOGICAL).join("LICENSE"))?,
            "a"
        );
        view.prepare(
            &artifact_fs(&project, "sample-b"),
            &requirements(&["LICENSE"]),
        )?;
        assert_eq!(
            fs::read_to_string(project.path().join(LOGICAL).join("LICENSE"))?,
            "b"
        );
        assert!(!project.path().join(LOGICAL).join("stale").exists());
        assert_eq!(
            fs::read(
                project
                    .path()
                    .join("buck-out/v2/cell_sources/v1/.owners/c_73616d706c65")
            )?,
            owner_contents(
                "sample",
                &ProjectRelativePathBuf::unchecked_new("sample-b".to_owned())
            )
        );
        Ok(())
    }

    /// Renaming a cell while pinning its execution name must not disturb the forest: same
    /// directory, same owner record, no clearing, no `buck2 clean`.
    #[test]
    fn pinned_execution_name_survives_a_cell_rename() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a"))?;
        fs::write(project.path().join("sample-a/LICENSE"), "a")?;

        CanonicalCellExecutionView::new().prepare(
            &artifact_fs(&project, "sample-a"),
            &requirements(&["LICENSE"]),
        )?;
        let owner = project
            .path()
            .join("buck-out/v2/cell_sources/v1/.owners/c_73616d706c65");
        let before = fs::read(&owner)?;
        let plant_before = fs::symlink_metadata(project.path().join(LOGICAL).join("LICENSE"))?;

        CanonicalCellExecutionView::new().prepare(
            &renamed_artifact_fs(&project, "sample-a"),
            &requirements_for(CellName::testing_new("sample_v2"), &["LICENSE"], &[]),
        )?;

        assert_eq!(
            fs::read(&owner)?,
            before,
            "rename must not rewrite the owner record"
        );
        assert_eq!(
            fs::read_to_string(project.path().join(LOGICAL).join("LICENSE"))?,
            "a"
        );
        assert!(
            plant_before.file_type().is_symlink()
                && fs::symlink_metadata(project.path().join(LOGICAL).join("LICENSE"))?
                    .file_type()
                    .is_symlink()
        );
        Ok(())
    }

    /// A rename under a pinned execution name selects the same `c_*` directory, so it must not look
    /// like a first use to the daemon that already published it: clearing there would pull the
    /// forest out from under a live worker holding those paths.
    #[test]
    fn rename_does_not_reclear_a_published_forest() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a"))?;
        fs::write(project.path().join("sample-a/LICENSE"), "a")?;
        fs::write(project.path().join("sample-a/OTHER"), "o")?;

        let view = CanonicalCellExecutionView::new();
        view.prepare(
            &artifact_fs(&project, "sample-a"),
            &requirements(&["LICENSE", "OTHER"]),
        )?;
        view.prepare(
            &renamed_artifact_fs(&project, "sample-a"),
            &requirements_for(CellName::testing_new("sample_v2"), &["LICENSE"], &[]),
        )?;

        assert!(
            project.path().join(LOGICAL).join("OTHER").exists(),
            "rename must not clear plants the same daemon already published"
        );
        Ok(())
    }

    /// The physical root still rebinds under a pinned execution name; only the label is stable.
    #[test]
    fn pinned_execution_name_still_rebinds_on_a_moved_checkout() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a"))?;
        fs::create_dir_all(project.path().join("sample-b"))?;
        fs::write(project.path().join("sample-a/LICENSE"), "a")?;
        fs::write(project.path().join("sample-b/LICENSE"), "b")?;

        CanonicalCellExecutionView::new().prepare(
            &artifact_fs(&project, "sample-a"),
            &requirements(&["LICENSE"]),
        )?;
        CanonicalCellExecutionView::new().prepare(
            &renamed_artifact_fs(&project, "sample-b"),
            &requirements_for(CellName::testing_new("sample_v2"), &["LICENSE"], &[]),
        )?;

        assert_eq!(
            fs::read_to_string(project.path().join(LOGICAL).join("LICENSE"))?,
            "b"
        );
        assert_eq!(
            fs::read(
                project
                    .path()
                    .join("buck-out/v2/cell_sources/v1/.owners/c_73616d706c65")
            )?,
            owner_contents(
                "sample",
                &ProjectRelativePathBuf::unchecked_new("sample-b".to_owned())
            )
        );
        Ok(())
    }

    #[test]
    fn fresh_daemon_replants_verified_same_owner_entries() -> buck2_error::Result<()> {
        let project = TempDir::new()?;
        fs::create_dir_all(project.path().join("sample-a"))?;
        fs::create_dir_all(project.path().join("wrong"))?;
        fs::write(project.path().join("sample-a/LICENSE"), "right")?;
        fs::write(project.path().join("wrong/LICENSE"), "wrong")?;
        let artifact_fs = artifact_fs(&project, "sample-a");

        CanonicalCellExecutionView::new().prepare(&artifact_fs, &requirements(&["LICENSE"]))?;
        let logical = project.path().join(LOGICAL).join("LICENSE");
        fs::remove_file(&logical)?;
        std::os::unix::fs::symlink(project.path().join("wrong/LICENSE"), &logical)?;

        // A new daemon with a matching owner record may clear the stale plant.
        CanonicalCellExecutionView::new().prepare(&artifact_fs, &requirements(&["LICENSE"]))?;
        assert_eq!(fs::read_to_string(logical)?, "right");
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
            &bundled_artifact_fs(&project),
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
                &artifact_fs(&local, "sample-a"),
                &requirements_with_empty(&["assets"], &["assets/empty"]),
            )
            .expect_err("local source directories must already exist");
        assert!(!local.path().join("sample-a/assets").exists());
        Ok(())
    }
}
