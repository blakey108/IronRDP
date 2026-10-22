//! File-backed persistent bitmap cache for the graphics pipeline ([MS-RDPEGFX] 3.3.1.5).
//!
//! The file holds the bitmap cache's tiles, most recently used first:
//!
//! ```text
//! magic    8 bytes   "IRGFXPC1"
//! count    u32 LE
//! count x  cache_key u64 LE, width u16 LE, height u16 LE, width * height * 4 bytes of RGBA8888
//! ```
//!
//! Saving writes a sibling temporary file and renames it over the cache, so a crash mid-save
//! leaves the previous cache intact and concurrent sessions never read a half-written file.
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0

use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use ironrdp_egfx::cache::{
    CACHE_BYTES, CachedBitmap, MAX_CACHE_IMPORT_ENTRIES, PersistentCacheEntry, PersistentCacheError,
    PersistentCacheStore,
};
use ironrdp_egfx::pdu::CacheEntryMetadata;

const MAGIC: &[u8; 8] = b"IRGFXPC1";
const FILE_HEADER_SIZE: u64 = 8 /* magic */ + 4 /* count */;
const ENTRY_HEADER_SIZE: u64 = 8 /* cache_key */ + 2 /* width */ + 2 /* height */;

/// Distinguishes the temporary files of saves running at once in one process.
static SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Where an entry's pixels sit in the cache file.
#[derive(Debug, Clone, Copy)]
struct EntryLocation {
    offset: u64,
    width: u16,
    height: u16,
}

/// A [`PersistentCacheStore`] kept in one file.
#[derive(Debug)]
pub struct FilePersistentCache {
    path: PathBuf,
    /// Entries found by the last [`PersistentCacheStore::entries`] call, until the import finishes.
    index: HashMap<u64, EntryLocation>,
    /// The file `index` describes, kept open with it so loads read what was listed.
    reader: Option<File>,
}

impl FilePersistentCache {
    /// A cache stored at `path`. Nothing is read or written until the graphics channel asks.
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            index: HashMap::new(),
            reader: None,
        }
    }

    fn read_index(&mut self) -> io::Result<Vec<CacheEntryMetadata>> {
        self.index.clear();
        self.reader = None;

        let file = match open_shared(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);

        let mut magic = [0; 8];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an IronRDP graphics cache file",
            ));
        }
        let count = read_u32(&mut reader)?;

        let mut offset = FILE_HEADER_SIZE;
        let mut entries = Vec::new();
        for _ in 0..count.min(u32::try_from(MAX_CACHE_IMPORT_ENTRIES).expect("small constant")) {
            if offset + ENTRY_HEADER_SIZE > file_len {
                break;
            }
            let cache_key = read_u64(&mut reader)?;
            let width = read_u16(&mut reader)?;
            let height = read_u16(&mut reader)?;
            let len = u64::from(width) * u64::from(height) * 4;
            let data_offset = offset + ENTRY_HEADER_SIZE;

            // A length past the cache's capacity or past the end of the file means corruption;
            // keep the entries read so far.
            if len == 0 || len > u64::try_from(CACHE_BYTES).expect("fits") || data_offset + len > file_len {
                break;
            }

            reader.seek_relative(i64::try_from(len).expect("bounded by CACHE_BYTES"))?;
            offset = data_offset + len;

            if self.index.contains_key(&cache_key) {
                continue;
            }
            self.index.insert(
                cache_key,
                EntryLocation {
                    offset: data_offset,
                    width,
                    height,
                },
            );
            entries.push(CacheEntryMetadata {
                cache_key,
                bitmap_len: u32::try_from(len).expect("bounded by CACHE_BYTES"),
            });
        }

        self.reader = Some(reader.into_inner());
        Ok(entries)
    }

    fn read_entry(&mut self, cache_key: u64) -> io::Result<Option<PersistentCacheEntry>> {
        let Some(location) = self.index.get(&cache_key).copied() else {
            return Ok(None);
        };
        // INVARIANT: `index` is filled together with `reader` and cleared with it, so an indexed
        // entry always has the file it was measured in.
        let Some(file) = self.reader.as_mut() else {
            return Ok(None);
        };

        let mut data = vec![0; usize::from(location.width) * usize::from(location.height) * 4];
        file.seek(SeekFrom::Start(location.offset))?;
        file.read_exact(&mut data)?;

        Ok(Some(PersistentCacheEntry {
            cache_key,
            width: location.width,
            height: location.height,
            data,
        }))
    }

    fn write(&mut self, entries: &[CachedBitmap<'_>]) -> io::Result<()> {
        // The rename below fails on Windows while this process still holds the file open.
        self.reader = None;
        self.index.clear();

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        // UNTESTED: sessions sharing one cache file never observe a half-written file, because
        // each writes a temporary of its own (unique per process and per save) and renames it into
        // place; the last save wins. This matters since the default path is per user, not per
        // connection. When changing how the file is written, keep the unique temporary and the
        // rename, and the reader drop above, without which Windows refuses the rename.
        let mut temp_path = self.path.clone().into_os_string();
        temp_path.push(format!(
            ".{}-{}.tmp",
            std::process::id(),
            SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let temp_path = PathBuf::from(temp_path);

        // Created before the cleanup below takes over, so a failed exclusive create never removes
        // a file this save did not make.
        let temp_file = create_private(&temp_path)?;
        let result = (|| {
            let mut writer = BufWriter::new(temp_file);
            writer.write_all(MAGIC)?;
            let count = u32::try_from(entries.len()).map_err(io::Error::other)?;
            writer.write_all(&count.to_le_bytes())?;
            for entry in entries {
                writer.write_all(&entry.cache_key.to_le_bytes())?;
                writer.write_all(&entry.width.to_le_bytes())?;
                writer.write_all(&entry.height.to_le_bytes())?;
                writer.write_all(entry.data)?;
            }
            writer
                .into_inner()
                .map_err(io::IntoInnerError::into_error)?
                .sync_all()?;
            fs::rename(&temp_path, &self.path)
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }
}

impl PersistentCacheStore for FilePersistentCache {
    fn entries(&mut self) -> Result<Vec<CacheEntryMetadata>, PersistentCacheError> {
        Ok(self.read_index()?)
    }

    fn load(&mut self, cache_key: u64) -> Result<Option<PersistentCacheEntry>, PersistentCacheError> {
        Ok(self.read_entry(cache_key)?)
    }

    fn import_finished(&mut self) {
        self.index.clear();
        self.reader = None;
    }

    fn save(&mut self, entries: &[CachedBitmap<'_>]) -> Result<(), PersistentCacheError> {
        // Only well-formed entries are written, so the reader's corruption checks never trip on
        // something this writer produced.
        let entries: Vec<CachedBitmap<'_>> = entries
            .iter()
            .filter(|entry| usize::from(entry.width) * usize::from(entry.height) * 4 == entry.data.len())
            .filter(|entry| !entry.data.is_empty())
            .copied()
            .collect();
        Ok(self.write(&entries)?)
    }
}

/// Open the cache file for reading without stopping another session from replacing it.
///
/// On Windows a plain open denies deletion, so another session's save could not rename its file
/// over this one while an import is reading it.
fn open_shared(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    options.open(path)
}

/// Create a new file only its owner can read, failing if it exists.
///
/// The cache holds desktop pixels, so on Unix it must not inherit a umask that lets other local
/// users read it; the rename keeps the mode set here. On Windows the file inherits the ACL of its
/// directory, which for the default path is the user's local application data.
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options.open(path)
}

/// The default location of the graphics cache file for the current user: `egfx-cache.bin` under
/// `%LOCALAPPDATA%\IronRDP` on Windows, and under `$XDG_CACHE_HOME/ironrdp` (or `~/.cache/ironrdp`
/// when `XDG_CACHE_HOME` is unset or empty) elsewhere.
pub fn default_cache_path() -> Option<PathBuf> {
    const FILE_NAME: &str = "egfx-cache.bin";

    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("IronRDP").join(FILE_NAME))
    }

    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .map(|dir| dir.join("ironrdp").join(FILE_NAME))
    }
}

fn read_u16(reader: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
