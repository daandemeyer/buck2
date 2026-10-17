/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use allocative::Allocative;
use buck2_error::BuckErrorOptionContext;
use buck2_error::ErrorTag;
use buck2_error::conversion::from_any_with_tag;
use buck2_fs::error::IoResultExt;
use buck2_fs::fs_util;
use buck2_fs::paths::abs_path::AbsPath;
use dupe::Dupe;
use pagable::Pagable;
use regex::Regex;

#[derive(Debug, buck2_error::Error)]
#[buck2(tag = Input)]
enum ExtractArchiveError {
    #[error(
        "Unknown archive type `{0}`, expected one of: {}",
        ArchiveType::names()
    )]
    UnknownType(String),
    #[error(
        "Cannot tell the archive type of `{0}` from its name, expected one of these extensions: {}",
        ArchiveType::names()
    )]
    UnknownExtension(String),
    #[error(
        "Invalid strip_prefix `{0}`: it must not contain `..`, and on Windows it must not contain `\\` or `:`"
    )]
    InvalidStripPrefix(String),
    #[error("Invalid exclude pattern `{0}`")]
    InvalidExclude(String),
    #[error("Archive entry `{0}` has a name that is not valid UTF-8")]
    NonUtf8Name(String),
    #[error("Archive entry `{0}` has an invalid name")]
    InvalidName(String),
    #[error("Archive entry `{0}...` has a path longer than {MAX_PATH_LEN} bytes")]
    PathTooLong(String),
    #[error("Archive entry `{0}` would be extracted below `{1}`, which is not a directory")]
    NotADirectory(String, String),
    #[error("Archive entry `{0}` would replace the directory `{1}`")]
    ReplacesDirectory(String, String),
    #[error(
        "Archive entry `{0}` is a hard link to `{1}`, which is not a file or symlink extracted before it"
    )]
    InvalidHardLink(String, String),
    #[error("Archive entry `{0}` is a symlink with an empty target")]
    EmptySymlinkTarget(String),
    #[error("Archive entry `{0}` is a symlink with an invalid target `{1}`")]
    InvalidSymlinkTarget(String, String),
    #[error("Archive entry `{0}` has an unsupported type ({1})")]
    UnsupportedEntryType(String, String),
    #[error("The archive has no entries below strip_prefix `{0}`")]
    StripPrefixNotFound(String),
    #[error("The zip archive has entries whose data overlaps")]
    OverlappingZipEntries,
}

#[derive(Debug, Clone, Copy, Dupe, PartialEq, Eq, Allocative, Pagable)]
pub(crate) enum ArchiveType {
    Tar,
    TarBz2,
    TarGz,
    TarXz,
    TarZst,
    Zip,
}

impl ArchiveType {
    const ALL: [ArchiveType; 6] = [
        ArchiveType::Tar,
        ArchiveType::TarBz2,
        ArchiveType::TarGz,
        ArchiveType::TarXz,
        ArchiveType::TarZst,
        ArchiveType::Zip,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            ArchiveType::Tar => "tar",
            ArchiveType::TarBz2 => "tar.bz2",
            ArchiveType::TarGz => "tar.gz",
            ArchiveType::TarXz => "tar.xz",
            ArchiveType::TarZst => "tar.zst",
            ArchiveType::Zip => "zip",
        }
    }

    fn names() -> String {
        Self::ALL.map(|t| t.name()).join(", ")
    }

    pub(crate) fn parse(name: &str) -> buck2_error::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| ExtractArchiveError::UnknownType(name.to_owned()).into())
    }

    pub(crate) fn from_file_name(file_name: &str) -> buck2_error::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|t| {
                file_name
                    .strip_suffix(t.name())
                    .is_some_and(|stem| stem.ends_with('.'))
            })
            .ok_or_else(|| ExtractArchiveError::UnknownExtension(file_name.to_owned()).into())
    }
}

/// tar drops leading slashes and `.` components from entry names, and `split_name` drops them too.
fn split_name(name: &str) -> Option<Vec<&str>> {
    let mut components = Vec::new();
    for component in name.split('/') {
        match component {
            "" | "." => {}
            ".." => return None,
            // Windows treats `\` as a path separator and `:` as a drive or stream separator. A
            // component that contains `\` or `:` could name a path outside the output directory.
            c if cfg!(windows) && (c.contains('\\') || c.contains(':')) => return None,
            c => components.push(c),
        }
    }
    Some(components)
}

pub(crate) fn parse_strip_prefix(prefix: &str) -> buck2_error::Result<Box<[String]>> {
    Ok(split_name(prefix)
        .ok_or_else(|| ExtractArchiveError::InvalidStripPrefix(prefix.to_owned()))?
        .into_iter()
        .map(str::to_owned)
        .collect())
}

pub(crate) fn parse_excludes(patterns: &[&str]) -> buck2_error::Result<Box<[Regex]>> {
    patterns
        .iter()
        .map(|p| {
            Regex::new(p).map_err(|e| {
                buck2_error::Error::from(ExtractArchiveError::InvalidExclude((*p).to_owned()))
                    .context(e.to_string())
            })
        })
        .collect()
}

pub(crate) struct ExtractOptions<'a> {
    pub(crate) archive_type: ArchiveType,
    pub(crate) strip_prefix: &'a [String],
    /// The regexes match the entry name as the archive stores it, except that a zip entry name
    /// has `/` in place of `\`.
    pub(crate) excludes: &'a [Regex],
}

/// The xz format lets an archive declare a dictionary of up to 4 GiB, and the decoder allocates
/// the whole dictionary inside the daemon. The largest `xz` preset uses a 64 MiB dictionary, and
/// the limit is four times that.
const XZ_MEMORY_LIMIT_KIB: u32 = 256 * 1024;

/// Linux refuses to create a symlink whose target is 4096 bytes or longer. A zip archive can store
/// a symlink target of any size, and reading the whole target would allocate that much memory.
const MAX_SYMLINK_TARGET_LEN: u64 = 4095;

/// Linux refuses paths of 4096 bytes or longer. `Unpacker::create_parents` checks every new parent
/// directory with its own `symlink_metadata` call. Each call resolves the whole path again, so a
/// new deep path costs time quadratic in its length.
const MAX_PATH_LEN: usize = 4095;

fn read_error(e: io::Error) -> buck2_error::Error {
    from_any_with_tag(e, ErrorTag::Input)
}

/// The tar crate reads and decompresses the data of every entry that `extract` skips, and the zip
/// crate reads the local header of every entry. Checking for cancellation on every read of the
/// archive also stops those reads.
struct Cancellable<'a, R> {
    inner: R,
    is_cancelled: &'a (dyn Fn() -> bool + Sync),
}

impl<R: Read> Read for Cancellable<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.is_cancelled)() {
            return Err(io::Error::other("The extraction was cancelled"));
        }
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for Cancellable<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if (self.is_cancelled)() {
            return Err(io::Error::other("The extraction was cancelled"));
        }
        self.inner.seek(pos)
    }
}

pub(crate) fn extract(
    archive: &Path,
    dest: &Path,
    options: &ExtractOptions,
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> buck2_error::Result<()> {
    fs::create_dir_all(dest)?;
    let mut unpacker = Unpacker {
        dest,
        options,
        is_cancelled,
        known_dirs: KnownDirs::new(),
        found_prefix: false,
    };

    // The decoders below can read a lot of input without producing output, so the input is
    // checked for cancellation as well as the decoded tar stream.
    let file = BufReader::new(Cancellable {
        inner: File::open(archive)?,
        is_cancelled,
    });
    let decoded: Box<dyn Read + '_> = match options.archive_type {
        ArchiveType::Tar => Box::new(file),
        ArchiveType::TarBz2 => Box::new(bzip2::bufread::MultiBzDecoder::new(file)),
        ArchiveType::TarGz => Box::new(flate2::bufread::MultiGzDecoder::new(file)),
        ArchiveType::TarXz => Box::new(XzStreams {
            reader: Some(lzma_rust2::XzReader::new_mem_limit(
                file,
                false,
                XZ_MEMORY_LIMIT_KIB,
            )),
        }),
        ArchiveType::TarZst => Box::new(zstd::Decoder::with_buffer(file).map_err(read_error)?),
        ArchiveType::Zip => {
            extract_zip(file, &mut unpacker)?;
            return unpacker.finish();
        }
    };
    extract_tar(
        Cancellable {
            inner: decoded,
            is_cancelled,
        },
        &mut unpacker,
    )?;
    unpacker.finish()
}

/// lzma-rust2 can decode concatenated xz streams itself, but it recurses once for every stream
/// without blocks. A small archive of thousands of empty streams then overflows the stack of the
/// daemon thread. `XzStreams` decodes one stream at a time in a loop instead.
struct XzStreams<R: Read> {
    /// `None` once the input has no more streams, or after a read failed.
    reader: Option<lzma_rust2::XzReader<R>>,
}

impl<R: BufRead> Read for XzStreams<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(mut reader) = self.reader.take() else {
                return Ok(0);
            };
            let n = reader.read(buf)?;
            if n > 0 || buf.is_empty() {
                self.reader = Some(reader);
                return Ok(n);
            }
            let mut input = reader.into_inner();

            // The xz format allows stream padding after a stream: zero bytes in a multiple of
            // four.
            let mut padding = 0;
            let at_end = loop {
                let data = input.fill_buf()?;
                if data.is_empty() {
                    break true;
                }
                let zeros = data.iter().take_while(|byte| **byte == 0).count();
                let more = zeros < data.len();
                input.consume(zeros);
                padding += zeros;
                if more {
                    break false;
                }
            };
            if padding % 4 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "xz stream padding is not a multiple of four bytes",
                ));
            }
            if !at_end {
                self.reader = Some(lzma_rust2::XzReader::new_mem_limit(
                    input,
                    false,
                    XZ_MEMORY_LIMIT_KIB,
                ));
            }
        }
    }
}

fn utf8_name(bytes: &[u8]) -> buck2_error::Result<&str> {
    str::from_utf8(bytes).map_err(|_| {
        ExtractArchiveError::NonUtf8Name(String::from_utf8_lossy(bytes).into_owned()).into()
    })
}

/// GNU tar can store a sparse file in a pax archive as a regular file with `GNU.sparse.*` keys. The
/// tar crate does not decode these keys and would extract the stored data as the file content,
/// which leaves out the holes of the file.
fn is_pax_sparse<R: Read>(entry: &mut tar::Entry<R>) -> buck2_error::Result<bool> {
    let Some(extensions) = entry.pax_extensions().map_err(read_error)? else {
        return Ok(false);
    };
    for extension in extensions {
        if extension
            .map_err(read_error)?
            .key_bytes()
            .starts_with(b"GNU.sparse.")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn extract_tar(reader: impl Read, unpacker: &mut Unpacker) -> buck2_error::Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(read_error)? {
        let mut entry = entry.map_err(read_error)?;
        let entry_type = entry.header().entry_type();
        // A pax global header holds metadata, such as the commit that `git archive` adds.
        if entry_type == tar::EntryType::XGlobalHeader {
            continue;
        }

        let name_bytes = entry.path_bytes().into_owned();
        let name = String::from_utf8_lossy(&name_bytes).into_owned();
        let Some(rel) = unpacker.place(&name)? else {
            continue;
        };
        utf8_name(&name_bytes)?;
        if is_pax_sparse(&mut entry)? {
            return Err(ExtractArchiveError::UnsupportedEntryType(
                name,
                "sparse file in pax format".to_owned(),
            )
            .into());
        }

        match entry_type {
            tar::EntryType::Directory => unpacker.dir(&name, &rel)?,
            // Archives from before POSIX mark directories as regular files whose name ends in a
            // slash.
            tar::EntryType::Regular if name.ends_with('/') => unpacker.dir(&name, &rel)?,
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                let executable = entry.header().mode().map_err(read_error)? & 0o111 != 0;
                let sparse = entry_type == tar::EntryType::GNUSparse;
                unpacker.file(&name, &rel, &mut entry, executable, sparse)?;
            }
            tar::EntryType::Symlink | tar::EntryType::Link => {
                let target = entry
                    .link_name_bytes()
                    .map(|target| utf8_name(&target).map(str::to_owned))
                    .transpose()?
                    .unwrap_or_default();
                if entry_type == tar::EntryType::Symlink {
                    unpacker.symlink(&name, &rel, &target)?;
                } else {
                    unpacker.hard_link(&name, &rel, &target)?;
                }
            }
            other => {
                return Err(
                    ExtractArchiveError::UnsupportedEntryType(name, format!("{other:?}")).into(),
                );
            }
        }
    }
    Ok(())
}

fn zip_error(e: zip::result::ZipError) -> buck2_error::Error {
    from_any_with_tag(e, ErrorTag::Input)
}

/// Several zip entries can point at the same compressed data. A small archive could then extract
/// to many full copies of that data. `ZipArchive::has_overlapping_files` compares every entry with
/// every earlier entry, which takes minutes for an archive with a million entries.
fn has_overlapping_entries<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> buck2_error::Result<bool> {
    let mut ranges = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).map_err(zip_error)?;
        let size = entry.compressed_size();
        if size == 0 {
            continue;
        }
        let start = entry
            .data_start()
            .internal_error("Opening a zip entry did not find the start of its data")?;
        ranges.push((start, start.saturating_add(size)));
    }
    ranges.sort_unstable();
    Ok(ranges.windows(2).any(|pair| pair[0].1 > pair[1].0))
}

fn extract_zip<R: Read + Seek>(file: R, unpacker: &mut Unpacker) -> buck2_error::Result<()> {
    let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;
    if has_overlapping_entries(&mut archive)? {
        return Err(ExtractArchiveError::OverlappingZipEntries.into());
    }
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        // Zip files written on Windows can separate path components with backslashes.
        let name = entry.name().replace('\\', "/");
        let Some(rel) = unpacker.place(&name)? else {
            continue;
        };
        if entry.is_dir() {
            unpacker.dir(&name, &rel)?;
        } else if entry.is_symlink() {
            let mut target = String::new();
            (&mut entry)
                .take(MAX_SYMLINK_TARGET_LEN + 1)
                .read_to_string(&mut target)
                .map_err(read_error)?;
            if target.len() as u64 > MAX_SYMLINK_TARGET_LEN {
                return Err(ExtractArchiveError::InvalidSymlinkTarget(
                    name,
                    format!("{}...", target.chars().take(64).collect::<String>()),
                )
                .into());
            }
            unpacker.symlink(&name, &rel, &target)?;
        } else {
            let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
            unpacker.file(&name, &rel, &mut entry, executable, false)?;
        }
    }
    Ok(())
}

enum Extracted {
    File,
    Symlink,
}

struct Unpacker<'a> {
    dest: &'a Path,
    options: &'a ExtractOptions<'a>,
    is_cancelled: &'a (dyn Fn() -> bool + Sync),
    known_dirs: KnownDirs,
    found_prefix: bool,
}

/// The directories below `dest` that `Unpacker::create_parents` or `Unpacker::dir` checked or
/// created, as a tree of path components. No entry can replace a directory, so a recorded
/// directory stays a directory. Later entries skip the `symlink_metadata` check for recorded
/// parents. A tree uses memory for each component once, while a set of full paths would use memory
/// quadratic in the depth of a directory.
struct KnownDirs {
    /// The recorded children of every recorded directory. Index 0 is `dest` itself.
    children: Vec<HashMap<OsString, usize>>,
}

impl KnownDirs {
    fn new() -> Self {
        Self {
            children: vec![HashMap::new()],
        }
    }

    fn child(&self, dir: usize, name: &OsStr) -> Option<usize> {
        self.children[dir].get(name).copied()
    }

    fn get(&self, rel: &Path) -> Option<usize> {
        rel.components()
            .try_fold(0, |dir, component| self.child(dir, component.as_os_str()))
    }

    fn insert(&mut self, dir: usize, name: &OsStr) -> usize {
        let child = self.children.len();
        self.children.push(HashMap::new());
        self.children[dir].insert(name.to_owned(), child);
        child
    }
}

impl Unpacker<'_> {
    fn finish(&self) -> buck2_error::Result<()> {
        if !self.options.strip_prefix.is_empty() && !self.found_prefix {
            return Err(ExtractArchiveError::StripPrefixNotFound(
                self.options.strip_prefix.join("/"),
            )
            .into());
        }
        Ok(())
    }

    fn strip<'n>(&self, components: &'n [&'n str]) -> Option<&'n [&'n str]> {
        let prefix = &self.options.strip_prefix;
        if components.len() < prefix.len()
            || !components.iter().zip(prefix.iter()).all(|(c, p)| c == p)
        {
            return None;
        }
        Some(&components[prefix.len()..])
    }

    fn place(&mut self, name: &str) -> buck2_error::Result<Option<PathBuf>> {
        let components =
            split_name(name).ok_or_else(|| ExtractArchiveError::InvalidName(name.to_owned()))?;
        let Some(rest) = self.strip(&components) else {
            return Ok(None);
        };
        self.found_prefix = true;
        if rest.is_empty() || self.options.excludes.iter().any(|r| r.is_match(name)) {
            return Ok(None);
        }
        let rel: PathBuf = rest.iter().collect();
        if rel.as_os_str().len() > MAX_PATH_LEN {
            return Err(ExtractArchiveError::PathTooLong(name.chars().take(100).collect()).into());
        }
        Ok(Some(rel))
    }

    /// An earlier entry can create a symlink at a parent path of `rel`. Writing through that
    /// symlink could create files outside the output directory, so a parent path that exists as
    /// anything but a directory fails the extraction.
    fn create_parents(&mut self, name: &str, rel: &Path) -> buck2_error::Result<(PathBuf, usize)> {
        let mut dir = 0;
        let mut path = self.dest.to_path_buf();
        for component in rel.parent().into_iter().flat_map(Path::components) {
            let component = component.as_os_str();
            path.push(component);
            if let Some(child) = self.known_dirs.child(dir, component) {
                dir = child;
                continue;
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(ExtractArchiveError::NotADirectory(
                        name.to_owned(),
                        path.strip_prefix(self.dest)
                            .unwrap_or(&path)
                            .display()
                            .to_string(),
                    )
                    .into());
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&path)?,
                Err(e) => return Err(e.into()),
            }
            dir = self.known_dirs.insert(dir, component);
        }
        Ok((self.dest.join(rel), dir))
    }

    /// An archive can contain two entries with the same name. A later file, symlink or hard link
    /// replaces an earlier file or symlink. Creating a file, symlink or hard link fails when the
    /// path exists, so the earlier file or symlink is removed first.
    fn remove_existing(&self, name: &str, rel: &Path) -> buck2_error::Result<()> {
        let path = self.dest.join(rel);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => Err(ExtractArchiveError::ReplacesDirectory(
                name.to_owned(),
                rel.display().to_string(),
            )
            .into()),
            Ok(_) => Ok(fs_util::remove_file(AbsPath::new(&path)?).categorize_input()?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn dir(&mut self, name: &str, rel: &Path) -> buck2_error::Result<()> {
        let (path, parent) = self.create_parents(name, rel)?;
        let file_name = rel
            .file_name()
            .internal_error("A placed entry has a file name")?;
        if self.known_dirs.child(parent, file_name).is_some() {
            return Ok(());
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                fs_util::remove_file(AbsPath::new(&path)?).categorize_input()?;
                fs::create_dir(&path)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(e) => return Err(e.into()),
        }
        self.known_dirs.insert(parent, file_name);
        Ok(())
    }

    fn file(
        &mut self,
        name: &str,
        rel: &Path,
        contents: &mut dyn Read,
        executable: bool,
        sparse: bool,
    ) -> buck2_error::Result<()> {
        let (path, _) = self.create_parents(name, rel)?;
        self.remove_existing(name, rel)?;

        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(if executable { 0o755 } else { 0o644 });
        }
        #[cfg(not(unix))]
        let _unused = executable;
        let mut file = options.open(&path)?;

        // A sparse tar entry can declare a size of up to 2^64 - 1 bytes in a few hundred bytes of
        // archive. The tar crate returns the holes of the entry as zeros without reading the
        // archive. The `Cancellable` around the archive never sees those reads, so the entry is
        // wrapped in `Cancellable` too. Writing all the zeros could fill the disk. The loop seeks
        // over all-zero chunks of a sparse entry instead, which leaves holes in the file the way
        // GNU tar does.
        let mut contents = Cancellable {
            inner: contents,
            is_cancelled: self.is_cancelled,
        };
        let mut buf = vec![0; 64 * 1024];
        let mut len = 0;
        loop {
            let n = contents.read(&mut buf).map_err(read_error)?;
            if n == 0 {
                break;
            }
            if sparse && buf[..n].iter().all(|byte| *byte == 0) {
                file.seek(SeekFrom::Current(n as i64))?;
            } else {
                file.write_all(&buf[..n])?;
            }
            len += n as u64;
        }
        if sparse {
            file.set_len(len)?;
        }
        Ok(())
    }

    fn symlink(&mut self, name: &str, rel: &Path, target: &str) -> buck2_error::Result<()> {
        if target.is_empty() {
            return Err(ExtractArchiveError::EmptySymlinkTarget(name.to_owned()).into());
        }
        // `build_entry_from_disk` panics on a relative symlink target that contains `\`, because
        // Buck2 records a relative target as a path with `/` separators.
        if cfg!(not(windows)) && Path::new(target).is_relative() && target.contains('\\') {
            return Err(ExtractArchiveError::InvalidSymlinkTarget(
                name.to_owned(),
                target.to_owned(),
            )
            .into());
        }
        let (path, _) = self.create_parents(name, rel)?;
        self.remove_existing(name, rel)?;
        fs_util::symlink(target, AbsPath::new(&path)?).categorize_input()?;
        Ok(())
    }

    fn hard_link(&mut self, name: &str, rel: &Path, target: &str) -> buck2_error::Result<()> {
        let invalid = || ExtractArchiveError::InvalidHardLink(name.to_owned(), target.to_owned());
        let components = split_name(target).ok_or_else(invalid)?;
        let target_rel: PathBuf = self
            .strip(&components)
            .ok_or_else(invalid)?
            .iter()
            .collect();
        let kind = self.extracted(&target_rel).ok_or_else(invalid)?;
        // GNU tar writes a hard link to itself when a file is archived twice.
        if target_rel == rel {
            return Ok(());
        }

        let (path, _) = self.create_parents(name, rel)?;
        self.remove_existing(name, rel)?;
        let target_path = self.dest.join(&target_rel);
        match kind {
            Extracted::File => fs::hard_link(&target_path, &path)?,
            Extracted::Symlink => {
                let link_target = fs::read_link(&target_path)?;
                fs_util::symlink(link_target, AbsPath::new(&path)?).categorize_input()?;
            }
        }
        Ok(())
    }

    /// `symlink_metadata` on the full path would follow a symlink in a parent directory of `rel`.
    /// A hard link through such a symlink could link a file from outside the output directory, so
    /// the parent of `rel` must be a recorded directory in `known_dirs`.
    fn extracted(&self, rel: &Path) -> Option<Extracted> {
        self.known_dirs.get(rel.parent()?)?;
        let metadata = fs::symlink_metadata(self.dest.join(rel)).ok()?;
        if metadata.is_file() {
            Some(Extracted::File)
        } else if metadata.is_symlink() {
            Some(Extracted::Symlink)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    enum Entry {
        Dir(&'static str),
        File(&'static str, &'static str, u32),
        Symlink(&'static str, &'static str),
        HardLink(&'static str, &'static str),
        Raw(tar::EntryType, &'static [u8], &'static [u8]),
    }

    fn header(entry_type: tar::EntryType, name: &[u8], mode: u32, size: u64) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_mode(mode);
        header.set_size(size);
        // `set_path` refuses names with `..`, and some tests need such names. Every name in the
        // tests fits into the 100-byte name field of the header.
        let field = &mut header.as_old_mut().name;
        field.fill(0);
        field[..name.len()].copy_from_slice(name);
        header.as_gnu_mut().unwrap().set_real_size(size);
        header
    }

    fn tar_bytes(entries: &[Entry]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for entry in entries {
            let (mut h, data): (tar::Header, &[u8]) = match entry {
                Entry::Dir(name) => (
                    header(tar::EntryType::Directory, name.as_bytes(), 0o755, 0),
                    b"",
                ),
                Entry::File(name, contents, mode) => (
                    header(
                        tar::EntryType::Regular,
                        name.as_bytes(),
                        *mode,
                        contents.len() as u64,
                    ),
                    contents.as_bytes(),
                ),
                Entry::Symlink(name, target) | Entry::HardLink(name, target) => {
                    let entry_type = match entry {
                        Entry::Symlink(..) => tar::EntryType::Symlink,
                        _ => tar::EntryType::Link,
                    };
                    let mut h = header(entry_type, name.as_bytes(), 0o777, 0);
                    h.set_link_name(target).unwrap();
                    (h, b"")
                }
                Entry::Raw(entry_type, name, data) => {
                    (header(*entry_type, name, 0o644, data.len() as u64), *data)
                }
            };
            h.set_cksum();
            builder.append(&h, data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn compress(archive_type: ArchiveType, tar: Vec<u8>) -> Vec<u8> {
        match archive_type {
            ArchiveType::Tar => tar,
            ArchiveType::TarBz2 => {
                let mut encoder =
                    bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
                encoder.write_all(&tar).unwrap();
                encoder.finish().unwrap()
            }
            ArchiveType::TarGz => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                encoder.write_all(&tar).unwrap();
                encoder.finish().unwrap()
            }
            ArchiveType::TarXz => {
                let mut encoder =
                    lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default())
                        .unwrap();
                encoder.write_all(&tar).unwrap();
                encoder.finish().unwrap()
            }
            ArchiveType::TarZst => zstd::encode_all(tar.as_slice(), 0).unwrap(),
            ArchiveType::Zip => unreachable!("the zip tests write archives with zip::ZipWriter"),
        }
    }

    struct ExtractedDir {
        _dir: tempfile::TempDir,
        out: PathBuf,
    }

    fn extract_bytes(
        archive_type: ArchiveType,
        bytes: &[u8],
        strip_prefix: &str,
        excludes: &[&str],
    ) -> buck2_error::Result<ExtractedDir> {
        extract_bytes_with(archive_type, bytes, strip_prefix, excludes, &|| false)
    }

    fn extract_bytes_with(
        archive_type: ArchiveType,
        bytes: &[u8],
        strip_prefix: &str,
        excludes: &[&str],
        is_cancelled: &(dyn Fn() -> bool + Sync),
    ) -> buck2_error::Result<ExtractedDir> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        fs::write(&archive, bytes).unwrap();
        let out = dir.path().join("out");
        extract(
            &archive,
            &out,
            &ExtractOptions {
                archive_type,
                strip_prefix: &parse_strip_prefix(strip_prefix)?,
                excludes: &parse_excludes(excludes)?,
            },
            is_cancelled,
        )?;
        Ok(ExtractedDir { _dir: dir, out })
    }

    fn extract_tar_entries(
        entries: &[Entry],
        strip_prefix: &str,
        excludes: &[&str],
    ) -> buck2_error::Result<ExtractedDir> {
        extract_bytes(
            ArchiveType::Tar,
            &tar_bytes(entries),
            strip_prefix,
            excludes,
        )
    }

    fn error_of(result: buck2_error::Result<ExtractedDir>) -> String {
        format!("{:#}", result.err().unwrap())
    }

    fn listing(root: &Path) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let mut children: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let path = child.path();
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                let metadata = fs::symlink_metadata(&path).unwrap();
                if metadata.is_symlink() {
                    out.push(format!(
                        "{rel} -> {}",
                        fs::read_link(&path).unwrap().display()
                    ));
                } else if metadata.is_dir() {
                    out.push(format!("{rel}/"));
                    walk(root, &path, out);
                } else {
                    let contents = fs::read_to_string(&path).unwrap();
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        metadata.permissions().mode() & 0o111 != 0
                    };
                    #[cfg(not(unix))]
                    let executable = false;
                    let x = if executable { " (x)" } else { "" };
                    out.push(format!("{rel}: {contents}{x}"));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out
    }

    fn sample() -> Vec<Entry> {
        vec![
            Entry::Dir("pkg-1.0/"),
            Entry::Dir("pkg-1.0/bin/"),
            Entry::File("pkg-1.0/bin/tool", "#!/bin/sh", 0o755),
            Entry::File("pkg-1.0/README", "hello", 0o644),
            Entry::Symlink("pkg-1.0/README.md", "README"),
        ]
    }

    #[test]
    fn test_archive_type_from_file_name() {
        for (name, expected) in [
            ("foo.tar", ArchiveType::Tar),
            ("foo-1.0.tar.gz", ArchiveType::TarGz),
            ("foo.tar.bz2", ArchiveType::TarBz2),
            ("foo.tar.xz", ArchiveType::TarXz),
            ("foo.tar.zst", ArchiveType::TarZst),
            ("dir/foo.zip", ArchiveType::Zip),
        ] {
            assert_eq!(ArchiveType::from_file_name(name).unwrap(), expected);
        }
        assert!(ArchiveType::from_file_name("foo.tgz").is_err());
        assert!(ArchiveType::from_file_name("tar").is_err());
        assert!(ArchiveType::parse("tar.lz").is_err());
    }

    #[test]
    fn test_compressed_tar() {
        for archive_type in [
            ArchiveType::Tar,
            ArchiveType::TarBz2,
            ArchiveType::TarGz,
            ArchiveType::TarXz,
            ArchiveType::TarZst,
        ] {
            let bytes = compress(archive_type, tar_bytes(&sample()));
            let extracted = extract_bytes(archive_type, &bytes, "", &[]).unwrap();
            let mut expected = vec![
                "pkg-1.0/",
                "pkg-1.0/README: hello",
                "pkg-1.0/README.md -> README",
                "pkg-1.0/bin/",
            ];
            expected.push(if cfg!(unix) {
                "pkg-1.0/bin/tool: #!/bin/sh (x)"
            } else {
                "pkg-1.0/bin/tool: #!/bin/sh"
            });
            assert_eq!(listing(&extracted.out), expected, "{archive_type:?}");
        }
    }

    #[test]
    fn test_strip_prefix() {
        let extracted = extract_tar_entries(
            &[
                Entry::File("pkg-1.0/a", "a", 0o644),
                Entry::File("pkg-1.0/sub/b", "b", 0o644),
                Entry::File("other/c", "c", 0o644),
                Entry::File("pkg-1.0x", "x", 0o644),
            ],
            "./pkg-1.0/",
            &[],
        )
        .unwrap();
        assert_eq!(listing(&extracted.out), vec!["a: a", "sub/", "sub/b: b"]);
    }

    #[test]
    fn test_strip_prefix_not_found() {
        let err = error_of(extract_tar_entries(&sample(), "pkg-2.0", &[]));
        assert!(
            err.contains("no entries below strip_prefix `pkg-2.0`"),
            "{err}"
        );
        assert!(parse_strip_prefix("a/../b").is_err());
    }

    #[test]
    fn test_excludes() {
        let extracted = extract_tar_entries(
            &[
                Entry::Dir("pkg/"),
                Entry::File("pkg/docs/index.html", "docs", 0o644),
                Entry::Dir("pkg/docs/"),
                Entry::File("pkg/docs/keep.txt", "keep", 0o644),
                Entry::File("pkg/src/lib.rs", "lib", 0o644),
                Entry::File("pkg/src/lib_test.rs", "test", 0o644),
            ],
            "pkg",
            &["^pkg/docs/$", "\\.html$", "_test\\.rs$"],
        )
        .unwrap();
        // Excluding the directory entry doesn't exclude the entries below it.
        assert_eq!(
            listing(&extracted.out),
            vec!["docs/", "docs/keep.txt: keep", "src/", "src/lib.rs: lib"]
        );
        assert!(parse_excludes(&["("]).is_err());
    }

    #[test]
    fn test_skipped_entries_are_not_checked() {
        let extracted = extract_tar_entries(
            &[
                Entry::File("pkg/a", "a", 0o644),
                Entry::Raw(tar::EntryType::Fifo, b"other/fifo", b""),
                Entry::Raw(tar::EntryType::Fifo, b"pkg/excluded_fifo", b""),
                Entry::Raw(tar::EntryType::Regular, b"other/\xff", b"x"),
            ],
            "pkg",
            &["excluded"],
        )
        .unwrap();
        assert_eq!(listing(&extracted.out), vec!["a: a"]);

        let err = error_of(extract_tar_entries(
            &[Entry::Raw(tar::EntryType::Fifo, b"fifo", b"")],
            "",
            &[],
        ));
        assert!(err.contains("unsupported type"), "{err}");
    }

    #[test]
    fn test_long_path_is_rejected() {
        let name: &'static str = Box::leak("a/".repeat(3000).into_boxed_str());
        let mut builder = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        builder.append_data(&mut h, name, io::empty()).unwrap();
        let bytes = builder.into_inner().unwrap();

        let err = error_of(extract_bytes(ArchiveType::Tar, &bytes, "", &[]));
        assert!(err.contains("longer than 4095 bytes"), "{err}");
        // An entry that is skipped is not checked.
        extract_bytes(ArchiveType::Tar, &bytes, "", &["^a/"]).unwrap();
    }

    #[test]
    fn test_absolute_and_dot_names() {
        let extracted = extract_tar_entries(
            &[
                Entry::File("/abs", "abs", 0o644),
                Entry::File("./dot//file", "dot", 0o644),
            ],
            "",
            &[],
        )
        .unwrap();
        assert_eq!(
            listing(&extracted.out),
            vec!["abs: abs", "dot/", "dot/file: dot"]
        );
    }

    #[test]
    fn test_parent_dir_name_is_rejected() {
        let err = error_of(extract_tar_entries(
            &[Entry::File("a/../../evil", "x", 0o644)],
            "",
            &[],
        ));
        assert!(err.contains("invalid name"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn test_no_write_through_symlink() {
        let outside = tempfile::tempdir().unwrap();
        let target: &'static str =
            Box::leak(outside.path().to_str().unwrap().to_owned().into_boxed_str());
        for entry in [
            Entry::File("link/evil", "x", 0o644),
            Entry::Dir("link/evil/"),
            Entry::Symlink("link/evil", "x"),
            Entry::Dir("link/sub/"),
        ] {
            let err = error_of(extract_tar_entries(
                &[Entry::Symlink("link", target), entry],
                "",
                &[],
            ));
            assert!(
                err.contains("below `link`, which is not a directory"),
                "{err}"
            );
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        }

        // A file entry replaces the symlink instead of writing to its target.
        let extracted = extract_tar_entries(
            &[
                Entry::Symlink("link", target),
                Entry::File("link", "replaced", 0o644),
            ],
            "",
            &[],
        )
        .unwrap();
        assert_eq!(listing(&extracted.out), vec!["link: replaced"]);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    #[cfg(unix)]
    fn test_symlink_target_with_backslash_is_rejected() {
        let err = error_of(extract_tar_entries(
            &[Entry::Symlink("link", "sub\\a")],
            "",
            &[],
        ));
        assert!(err.contains("invalid target"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn test_absolute_symlink_target_with_backslash_is_accepted() {
        let extracted =
            extract_tar_entries(&[Entry::Symlink("link", "/opt/a\\b")], "", &[]).unwrap();
        assert_eq!(listing(&extracted.out), vec!["link -> /opt/a\\b"]);
    }

    #[test]
    fn test_cancellation() {
        let err = error_of(extract_bytes_with(
            ArchiveType::Tar,
            &tar_bytes(&sample()),
            "",
            &[],
            &|| true,
        ));
        assert!(err.contains("cancelled"), "{err}");
    }

    #[test]
    fn test_concatenated_xz_streams() {
        let tar = tar_bytes(&sample());
        let (first, second) = tar.split_at(tar.len() / 2);
        let mut bytes = Vec::new();
        // Thousands of empty streams overflow the stack if each stream adds a stack frame.
        for _ in 0..5000 {
            bytes.extend(compress(ArchiveType::TarXz, Vec::new()));
        }
        bytes.extend(compress(ArchiveType::TarXz, first.to_vec()));
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend(compress(ArchiveType::TarXz, second.to_vec()));
        let extracted = extract_bytes(ArchiveType::TarXz, &bytes, "", &[]).unwrap();
        assert_eq!(listing(&extracted.out).len(), 5);

        let mut bytes = compress(ArchiveType::TarXz, first.to_vec());
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend(compress(ArchiveType::TarXz, second.to_vec()));
        let err = error_of(extract_bytes(ArchiveType::TarXz, &bytes, "", &[]));
        assert!(err.contains("multiple of four"), "{err}");
    }

    #[test]
    fn test_cancellation_while_decoder_reads_input() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        // Every empty gzip member is input that produces no output. The decoder reads all of the
        // members inside one read of the decoded stream.
        let mut bytes = Vec::new();
        for _ in 0..10000 {
            bytes.extend(compress(ArchiveType::TarGz, Vec::new()));
        }
        bytes.extend(compress(ArchiveType::TarGz, tar_bytes(&sample())));
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        fs::write(&archive, bytes).unwrap();
        let out = dir.path().join("out");
        let checks = AtomicUsize::new(0);
        let err = extract(
            &archive,
            &out,
            &ExtractOptions {
                archive_type: ArchiveType::TarGz,
                strip_prefix: &[],
                excludes: &[],
            },
            &|| checks.fetch_add(1, Ordering::Relaxed) >= 2,
        )
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
        assert_eq!(listing(&out), Vec::<String>::new());
    }

    #[test]
    fn test_hard_link() {
        let extracted = extract_tar_entries(
            &[
                Entry::File("pkg/a", "a", 0o644),
                Entry::HardLink("pkg/b", "pkg/a"),
                Entry::HardLink("pkg/a", "pkg/a"),
                Entry::Symlink("pkg/s", "a"),
                Entry::HardLink("pkg/t", "pkg/s"),
                Entry::File("pkg/sub/c", "c", 0o644),
                Entry::HardLink("pkg/d", "pkg/sub/c"),
            ],
            "pkg",
            &[],
        )
        .unwrap();
        assert_eq!(
            listing(&extracted.out),
            vec![
                "a: a", "b: a", "d: c", "s -> a", "sub/", "sub/c: c", "t -> a"
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = fs::metadata(extracted.out.join("a")).unwrap();
            let b = fs::metadata(extracted.out.join("b")).unwrap();
            assert_eq!(a.ino(), b.ino());
        }

        let err = error_of(extract_tar_entries(
            &[Entry::HardLink("b", "missing")],
            "",
            &[],
        ));
        assert!(err.contains("hard link"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn test_hard_link_through_symlink_is_rejected() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        let target: &'static str =
            Box::leak(outside.path().to_str().unwrap().to_owned().into_boxed_str());
        let err = error_of(extract_tar_entries(
            &[
                Entry::Symlink("link", target),
                Entry::HardLink("copy", "link/secret"),
            ],
            "",
            &[],
        ));
        assert!(err.contains("hard link"), "{err}");
    }

    /// Returns a tar archive with one old GNU sparse entry of `size` bytes that is a hole except
    /// for its last 4 bytes.
    fn sparse_tar(size: u64) -> Vec<u8> {
        let mut h = header(tar::EntryType::GNUSparse, b"sparse", 0o644, 4);
        let gnu = h.as_gnu_mut().unwrap();
        gnu.set_real_size(size);
        gnu.sparse[0].set_offset(size - 4);
        gnu.sparse[0].set_length(4);
        h.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&h, &b"data"[..]).unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn test_cancellation_while_reading_holes() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        let checks = AtomicUsize::new(0);
        let err = error_of(extract_bytes_with(
            ArchiveType::Tar,
            &sparse_tar(1 << 30),
            "",
            &[],
            &|| checks.fetch_add(1, Ordering::Relaxed) >= 100,
        ));
        assert!(err.contains("cancelled"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn test_sparse_files() {
        // The tar crate decodes the old GNU sparse format into the full file.
        let size = 1 << 30;
        let bytes = sparse_tar(size);

        let extracted = extract_bytes(ArchiveType::Tar, &bytes, "", &[]).unwrap();
        let metadata = fs::metadata(extracted.out.join("sparse")).unwrap();
        assert_eq!(metadata.len(), size);
        use std::os::unix::fs::MetadataExt;
        assert!(metadata.blocks() * 512 < size / 16, "{}", metadata.blocks());
    }

    #[test]
    fn test_pax_sparse_files_are_rejected() {
        let err = error_of(extract_tar_entries(
            &[
                Entry::Raw(
                    tar::EntryType::XHeader,
                    b"PaxHeaders/sparse",
                    b"28 GNU.sparse.name=d/sparse\n",
                ),
                Entry::File("d/GNUSparseFile.1/sparse", "0\n", 0o644),
            ],
            "",
            &[],
        ));
        assert!(err.contains("sparse file"), "{err}");
    }

    #[test]
    fn test_later_entry_replaces_file() {
        let extracted = extract_tar_entries(
            &[
                Entry::File("a", "old", 0o644),
                Entry::File("a", "new", 0o644),
            ],
            "",
            &[],
        )
        .unwrap();
        assert_eq!(listing(&extracted.out), vec!["a: new"]);

        let err = error_of(extract_tar_entries(
            &[Entry::Dir("a/"), Entry::File("a", "x", 0o644)],
            "",
            &[],
        ));
        assert!(err.contains("replace the directory"), "{err}");
    }

    #[test]
    fn test_zip() {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer.add_directory("pkg/", options).unwrap();
        writer
            .start_file("pkg/tool", options.unix_permissions(0o755))
            .unwrap();
        writer.write_all(b"#!/bin/sh").unwrap();
        writer
            .start_file("pkg\\data.txt", options.unix_permissions(0o644))
            .unwrap();
        writer.write_all(b"data").unwrap();
        writer.start_file("pkg/docs/index.html", options).unwrap();
        writer.write_all(b"docs").unwrap();
        writer.add_symlink("pkg/link", "data.txt", options).unwrap();
        writer.start_file("other", options).unwrap();
        writer.write_all(b"other").unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let extracted = extract_bytes(ArchiveType::Zip, &bytes, "pkg", &["/docs/"]).unwrap();
        let tool = if cfg!(unix) {
            "tool: #!/bin/sh (x)"
        } else {
            "tool: #!/bin/sh"
        };
        assert_eq!(
            listing(&extracted.out),
            vec!["data.txt: data", "link -> data.txt", tool]
        );
    }

    #[test]
    fn test_zip_overlapping_entries_are_rejected() {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("a", options).unwrap();
        writer.write_all(b"data").unwrap();
        writer.shallow_copy_file("a", "b").unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let err = error_of(extract_bytes(ArchiveType::Zip, &bytes, "", &[]));
        assert!(err.contains("overlaps"), "{err}");
    }

    #[test]
    fn test_zip_long_symlink_target_is_rejected() {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        writer.add_symlink("ok", "a".repeat(4095), options).unwrap();
        writer
            .add_symlink("link", "a".repeat(4096), options)
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let err = error_of(extract_bytes(ArchiveType::Zip, &bytes, "", &[]));
        assert!(err.contains("invalid target"), "{err}");
    }
}
