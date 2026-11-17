//! Typst [`World`] implementation backed by the real filesystem.
//!
//! [`SystemWorld`] is the bridge between typst-diff and the Typst compiler.
//! It resolves virtual file paths to real disk paths, caches loaded sources and
//! binary blobs, and downloads Typst packages on demand via `typst-kit`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_kit::downloader::SystemDownloader;
use typst_kit::files::{FsRoot, SystemFiles};
use typst_kit::fonts::FontStore;
use typst_kit::packages::SystemPackages;

/// Return Typst's current-date value for this process.
///
/// Typst expects `None` to mean that the date is unavailable and reports that as
/// an evaluation error. Cache the UTC instant so the old and new documents see
/// the same "today" value even if a diff run crosses midnight.
///
/// Since Typst 0.15 the offset is a [`Duration`]; it is truncated to whole
/// seconds, as in `typst-kit`'s own implementation.
pub(crate) fn current_date(offset: Option<Duration>) -> Option<Datetime> {
    static NOW_UTC: OnceLock<time::OffsetDateTime> = OnceLock::new();

    let now = *NOW_UTC.get_or_init(time::OffsetDateTime::now_utc);
    let date = match offset {
        Some(offset) => {
            let seconds = offset.seconds().trunc();
            if !seconds.is_finite()
                || seconds < f64::from(i32::MIN)
                || seconds > f64::from(i32::MAX)
            {
                return None;
            }
            let offset = time::UtcOffset::from_whole_seconds(seconds as i32).ok()?;
            now.to_offset(offset).date()
        }
        None => {
            let offset = time::UtcOffset::current_local_offset().ok()?;
            now.to_offset(offset).date()
        }
    };

    Datetime::from_ymd(date.year(), date.month() as u8, date.day())
}

/// Collect the fonts available to Typst: system fonts first, then the fonts
/// embedded in the binary (lowest priority), matching the Typst CLI.
pub(crate) fn search_fonts() -> FontStore {
    let mut fonts = FontStore::new();
    fonts.extend(typst_kit::fonts::system());
    fonts.extend(typst_kit::fonts::embedded());
    fonts
}

/// The [`FileId`] of a project file at the virtual path `path`.
pub(crate) fn project_file_id(path: &str) -> Result<FileId> {
    let vpath = VirtualPath::new(path).with_context(|| format!("invalid virtual path {path:?}"))?;
    Ok(RootedPath::new(VirtualRoot::Project, vpath).intern())
}

/// The virtual path of `entry` below the project `root` (both canonical),
/// e.g. `/certification/doc.typ`.
fn virtual_entry_path(entry: &Path, root: &Path) -> Result<String> {
    let relative = entry
        .strip_prefix(root)
        .map_err(|_| anyhow::anyhow!("{entry:?} is not inside the root directory {root:?}"))?;
    let mut path = String::new();
    for component in relative.components() {
        let component = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {relative:?}"))?;
        path.push('/');
        path.push_str(component);
    }
    Ok(path)
}

/// A Typst [`World`] that reads source and binary files from the local filesystem.
///
/// All virtual paths are rooted at the project root: by default the directory
/// containing the entry file (see [`SystemWorld::with_root`]).
/// Files are cached after the first read so repeated accesses are free.
pub struct SystemWorld {
    /// Resolves project files (below the project root) and package files
    /// (downloaded on demand) to paths on disk.
    files: SystemFiles,
    /// The virtual [`FileId`] of the entry file, relative to the project root.
    main: FileId,
    library: LazyHash<Library>,
    fonts: FontStore,
    /// Cache of already-parsed [`Source`] objects, keyed by their virtual [`FileId`].
    source_cache: Arc<Mutex<HashMap<FileId, Source>>>,
    /// Cache of raw binary file bytes (images, fonts, etc.).
    file_cache: Arc<Mutex<HashMap<FileId, FileResult<Bytes>>>>,
}

impl SystemWorld {
    /// Create a world rooted at `entry`'s parent directory.
    ///
    /// The entry file is pre-loaded into the source cache; if it cannot be read
    /// (e.g. the path doesn't exist) an error is returned immediately.
    pub fn new(entry: impl AsRef<Path>) -> Result<Self> {
        Self::with_root(entry, None)
    }

    /// Create a world whose project root is `root`, or `entry`'s parent
    /// directory when `root` is `None`, like `typst compile --root`.
    ///
    /// Absolute paths in the document (`/templates/template.typ`) resolve
    /// against the root, and no file outside it can be read. `entry` must lie
    /// inside the root.
    pub fn with_root(entry: impl AsRef<Path>, root: Option<&Path>) -> Result<Self> {
        let entry = entry
            .as_ref()
            .canonicalize()
            .with_context(|| format!("cannot find {:?}", entry.as_ref()))?;
        let root = match root {
            Some(root) => root
                .canonicalize()
                .with_context(|| format!("cannot find root directory {root:?}"))?,
            None => entry
                .parent()
                .ok_or_else(|| anyhow::anyhow!("entry path has no parent: {:?}", entry))?
                .to_owned(),
        };
        let main = project_file_id(&virtual_entry_path(&entry, &root)?)?;

        let world = Self {
            files: SystemFiles::new(
                FsRoot::new(root),
                SystemPackages::new(SystemDownloader::new("typst-diff")),
            ),
            main,
            library: LazyHash::new(Library::default()),
            fonts: search_fonts(),
            source_cache: Default::default(),
            file_cache: Default::default(),
        };

        // Pre-load the main file so it's in the cache.
        world.source(main).map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(world)
    }

    /// Resolve a virtual [`FileId`] to an absolute path on disk.
    ///
    /// For package files, the package is downloaded (once) into the package
    /// cache and the returned root is the unpacked package directory. For
    /// project files, the root is the project root.
    fn disk_path(&self, id: FileId) -> FileResult<PathBuf> {
        self.files.resolve(id)
    }
}

impl World for SystemWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        let mut cache = self.source_cache.lock().unwrap();
        if let Some(src) = cache.get(&id) {
            return Ok(src.clone());
        }
        let path = self.disk_path(id)?;
        let text = std::fs::read_to_string(&path).map_err(|err| FileError::from_io(err, &path))?;
        let src = Source::new(id, text);
        cache.insert(id, src.clone());
        Ok(src)
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        let mut cache = self.file_cache.lock().unwrap();
        if let Some(result) = cache.get(&id) {
            return result.clone();
        }
        let path = self.disk_path(id)?;
        let result = std::fs::read(&path)
            .map(Bytes::new)
            .map_err(|err| FileError::from_io(err, &path));
        cache.insert(id, result.clone());
        result
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        current_date(offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn source_reads_file_by_virtual_path() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.typ"), "Hello world").unwrap();
        let world = SystemWorld::new(dir.path().join("main.typ")).unwrap();
        let src = world.source(world.main()).unwrap();
        assert_eq!(src.text(), "Hello world");
    }

    #[test]
    fn source_resolves_include_path() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.typ"), "#include \"ch.typ\"").unwrap();
        fs::write(dir.path().join("ch.typ"), "Chapter text").unwrap();
        let world = SystemWorld::new(dir.path().join("main.typ")).unwrap();
        let ch_id = project_file_id("/ch.typ").unwrap();
        let src = world.source(ch_id).unwrap();
        assert_eq!(src.text(), "Chapter text");
    }

    #[test]
    fn root_resolves_absolute_paths_and_nested_entry() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("docs/sub")).unwrap();
        fs::create_dir_all(dir.path().join("templates")).unwrap();
        fs::write(
            dir.path().join("docs/sub/main.typ"),
            "#import \"/templates/t.typ\": x\n#x",
        )
        .unwrap();
        fs::write(dir.path().join("templates/t.typ"), "#let x = [Template]").unwrap();
        let world =
            SystemWorld::with_root(dir.path().join("docs/sub/main.typ"), Some(dir.path())).unwrap();

        assert_eq!(world.main(), project_file_id("/docs/sub/main.typ").unwrap());
        let content = crate::eval::eval_to_content(&world).unwrap();
        assert_eq!(content.plain_text(), "Template");
    }

    #[test]
    fn root_must_contain_entry() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("a")).unwrap();
        fs::create_dir_all(dir.path().join("b")).unwrap();
        fs::write(dir.path().join("a/main.typ"), "").unwrap();
        let err = match SystemWorld::with_root(
            dir.path().join("a/main.typ"),
            Some(&dir.path().join("b")),
        ) {
            Ok(_) => panic!("entry outside the root should fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("not inside the root"), "{err}");
    }

    #[test]
    fn new_rejects_missing_entry_file() {
        let dir = TempDir::new().unwrap();
        let err = match SystemWorld::new(dir.path().join("missing.typ")) {
            Ok(_) => panic!("missing entry should fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("cannot find"), "{err}");
    }

    #[test]
    fn source_cache_returns_original_contents_after_disk_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.typ");
        fs::write(&path, "Original").unwrap();
        let world = SystemWorld::new(&path).unwrap();

        fs::write(&path, "Changed").unwrap();
        let src = world.source(world.main()).unwrap();

        assert_eq!(src.text(), "Original");
    }

    #[test]
    fn file_reads_and_caches_binary_assets() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.typ"), "").unwrap();
        let asset = dir.path().join("asset.bin");
        fs::write(&asset, [1_u8, 2, 3]).unwrap();
        let world = SystemWorld::new(dir.path().join("main.typ")).unwrap();
        let id = project_file_id("/asset.bin").unwrap();

        let first = world.file(id).unwrap();
        fs::write(&asset, [9_u8, 9, 9]).unwrap();
        let second = world.file(id).unwrap();

        assert_eq!(first.as_slice(), &[1, 2, 3]);
        assert_eq!(second.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn missing_source_returns_file_error() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.typ"), "").unwrap();
        let world = SystemWorld::new(dir.path().join("main.typ")).unwrap();
        let id = project_file_id("/missing.typ").unwrap();

        assert!(world.source(id).is_err());
    }

    #[test]
    fn today_returns_current_date() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.typ"), "").unwrap();
        let world = SystemWorld::new(dir.path().join("main.typ")).unwrap();

        assert!(world.today(None).is_some());
    }
}
