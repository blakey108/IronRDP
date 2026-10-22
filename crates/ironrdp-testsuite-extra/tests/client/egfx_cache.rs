use std::fs;
use std::path::PathBuf;

use ironrdp_client::egfx_cache::FilePersistentCache;
use ironrdp_egfx::cache::{CachedBitmap, PersistentCacheEntry, PersistentCacheStore as _};
use ironrdp_egfx::pdu::CacheEntryMetadata;
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("ironrdp-egfx-cache-{}", Uuid::new_v4()));
        Self(path)
    }

    fn file(&self) -> PathBuf {
        self.0.join("nested").join("egfx-cache.bin")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pixels(width: u16, height: u16, seed: u8) -> Vec<u8> {
    (0..usize::from(width) * usize::from(height) * 4)
        .map(|i| u8::try_from(i % 251).unwrap() ^ seed)
        .collect()
}

#[test]
fn a_missing_file_is_an_empty_cache() {
    let dir = TempDir::new();
    let mut cache = FilePersistentCache::new(dir.file());

    assert!(cache.entries().unwrap().is_empty());
    assert_eq!(cache.load(1).unwrap(), None);
}

#[test]
fn saved_entries_are_listed_in_order_and_load_back() {
    let dir = TempDir::new();
    let first = pixels(3, 2, 0x11);
    let second = pixels(64, 64, 0x22);

    FilePersistentCache::new(dir.file())
        .save(&[
            CachedBitmap {
                cache_key: 0xDEAD_BEEF,
                width: 3,
                height: 2,
                data: &first,
            },
            CachedBitmap {
                cache_key: 7,
                width: 64,
                height: 64,
                data: &second,
            },
        ])
        .unwrap();

    let mut cache = FilePersistentCache::new(dir.file());
    assert_eq!(
        cache.entries().unwrap(),
        [
            CacheEntryMetadata {
                cache_key: 0xDEAD_BEEF,
                bitmap_len: 3 * 2 * 4,
            },
            CacheEntryMetadata {
                cache_key: 7,
                bitmap_len: 64 * 64 * 4,
            },
        ]
    );
    assert_eq!(
        cache.load(7).unwrap(),
        Some(PersistentCacheEntry {
            cache_key: 7,
            width: 64,
            height: 64,
            data: second,
        })
    );
    assert_eq!(
        cache.load(0xDEAD_BEEF).unwrap(),
        Some(PersistentCacheEntry {
            cache_key: 0xDEAD_BEEF,
            width: 3,
            height: 2,
            data: first,
        })
    );
    assert_eq!(cache.load(8).unwrap(), None);
}

#[test]
fn saving_replaces_the_file_even_after_a_listing() {
    let dir = TempDir::new();
    let old = pixels(2, 2, 1);
    let new = pixels(4, 1, 2);
    let mut cache = FilePersistentCache::new(dir.file());
    cache
        .save(&[CachedBitmap {
            cache_key: 1,
            width: 2,
            height: 2,
            data: &old,
        }])
        .unwrap();

    // A session lists and imports, then saves its own cache over the file it read.
    assert_eq!(cache.entries().unwrap().len(), 1);
    cache
        .save(&[CachedBitmap {
            cache_key: 2,
            width: 4,
            height: 1,
            data: &new,
        }])
        .unwrap();

    let mut reread = FilePersistentCache::new(dir.file());
    let keys: Vec<u64> = reread.entries().unwrap().iter().map(|entry| entry.cache_key).collect();
    assert_eq!(keys, [2]);
}

#[test]
fn a_truncated_file_keeps_the_entries_before_the_cut() {
    let dir = TempDir::new();
    let data = pixels(8, 8, 3);
    FilePersistentCache::new(dir.file())
        .save(&[
            CachedBitmap {
                cache_key: 1,
                width: 8,
                height: 8,
                data: &data,
            },
            CachedBitmap {
                cache_key: 2,
                width: 8,
                height: 8,
                data: &data,
            },
        ])
        .unwrap();
    let bytes = fs::read(dir.file()).unwrap();
    fs::write(dir.file(), &bytes[..bytes.len() - 10]).unwrap();

    let mut cache = FilePersistentCache::new(dir.file());
    let keys: Vec<u64> = cache.entries().unwrap().iter().map(|entry| entry.cache_key).collect();

    assert_eq!(keys, [1]);
}

#[test]
fn a_foreign_file_is_an_error_not_a_cache() {
    let dir = TempDir::new();
    fs::create_dir_all(dir.file().parent().unwrap()).unwrap();
    fs::write(dir.file(), b"not a cache file").unwrap();

    assert!(FilePersistentCache::new(dir.file()).entries().is_err());
}

#[test]
fn malformed_entries_are_not_written() {
    let dir = TempDir::new();
    let short = vec![0; 5];
    let good = pixels(1, 1, 4);
    FilePersistentCache::new(dir.file())
        .save(&[
            CachedBitmap {
                cache_key: 1,
                width: 2,
                height: 2,
                data: &short,
            },
            CachedBitmap {
                cache_key: 2,
                width: 1,
                height: 1,
                data: &good,
            },
        ])
        .unwrap();

    let keys: Vec<u64> = FilePersistentCache::new(dir.file())
        .entries()
        .unwrap()
        .iter()
        .map(|entry| entry.cache_key)
        .collect();
    assert_eq!(keys, [2]);
}
