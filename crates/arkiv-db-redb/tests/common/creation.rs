//! Crash images of store creation, shared by `initialization.rs` and the
//! experiment example.

use std::{collections::BTreeSet, path::Path};

use arkiv_db_redb::{
    RedbConfig, RedbStore, initialization_path,
    testing::{CrashDisk, CrashMode, Granularity},
};
use arkiv_db_storage::versioned::{StorageErrorKind, VersionedStore};

pub fn config() -> RedbConfig {
    RedbConfig::with_cache_size(64 << 10)
}

fn modes() -> Vec<CrashMode> {
    let mut modes = vec![CrashMode::DurableOnly, CrashMode::AllUnsynced];
    for seed in 0..8 {
        modes.push(CrashMode::Torn {
            seed,
            granularity: Granularity::Sector,
        });
        modes.push(CrashMode::Torn {
            seed: seed + 500,
            granularity: Granularity::Byte,
        });
    }
    modes
}

/// The crash images of an in-place store creation.
pub struct CreationImages {
    /// Mutating backend calls of a complete creation.
    pub calls: u64,
    /// Every distinct crash image, over every crash point and mode.
    pub images: BTreeSet<Vec<u8>>,
    /// The synced image of a completed creation.
    pub complete: Vec<u8>,
}

/// Cuts an in-place creation (`RedbStore::with_backend` on an empty
/// `CrashDisk`) off at every mutating call and collects the crash images.
pub fn creation_images() -> CreationImages {
    let disk = CrashDisk::new();
    let complete = {
        let open = disk.clone();
        let store = RedbStore::with_backend(move || Ok(open.backend()), config()).unwrap();
        disk.crash(store, CrashMode::DurableOnly)
    };
    let calls = disk.counters().mutating_calls;
    assert!(calls > 3, "creation writes and syncs");

    let mut images = BTreeSet::new();
    for k in 0..=calls {
        let disk = CrashDisk::new();
        disk.crash_after(k);
        let open = disk.clone();
        let created = RedbStore::with_backend(move || Ok(open.backend()), config());
        assert_eq!(created.is_ok(), k == calls, "crash point {k} of {calls}");
        disk.cut_off();
        drop(created);
        for mode in modes() {
            images.insert(disk.crash_image(mode));
        }
    }
    CreationImages {
        calls,
        images,
        complete,
    }
}

/// How in-place reopens treat crash images.
#[derive(Clone, Copy, Debug, Default)]
pub struct InPlace {
    /// Opened and initialized (empty, or a redb file without store metadata).
    pub initialized: u64,
    /// Opened a complete store.
    pub opened: u64,
    /// Refused as `Incompatible`.
    pub refused: u64,
}

/// Reopens every image in place with `RedbStore::with_backend`.
pub fn reopen_in_place(images: &BTreeSet<Vec<u8>>) -> InPlace {
    let mut counts = InPlace::default();
    for image in images {
        let disk = CrashDisk::from_image(image.clone());
        match RedbStore::with_backend(move || Ok(disk.backend()), config()) {
            Ok(store) if store.last_open().initialized => counts.initialized += 1,
            Ok(_) => counts.opened += 1,
            Err(error) => {
                assert_eq!(error.kind, StorageErrorKind::Incompatible, "{error}");
                counts.refused += 1;
            }
        }
    }
    counts
}

/// Leaves each image as the temporary file of a `create` in `dir` (with no
/// target, or an empty one) and checks that `create` discards it, builds a
/// fresh store and leaves no temporary file. Returns the number checked.
pub fn create_over_leftovers(dir: &Path, images: &BTreeSet<Vec<u8>>) -> usize {
    for (index, image) in images.iter().enumerate() {
        let path = dir.join(format!("store-{index}.redb"));
        // The documented temporary name, written out independently.
        let temp = dir.join(format!("store-{index}.redb.init"));
        assert_eq!(initialization_path(&path), temp);
        std::fs::write(&temp, image).unwrap();
        if index % 2 == 1 {
            // An empty target file is replaced as well.
            std::fs::write(&path, b"").unwrap();
        }

        let store = RedbStore::create(&path, config())
            .unwrap_or_else(|error| panic!("image {index} ({} bytes): {error}", image.len()));
        assert!(store.last_open().initialized);
        assert_eq!(store.head().unwrap(), None);
        assert!(!temp.exists(), "image {index}: temporary file left behind");
        let id = store.database_id();
        drop(store);

        let reopened = RedbStore::open(&path, config()).unwrap();
        assert_eq!(reopened.database_id(), id);
        assert!(!reopened.last_open().initialized);
        drop(reopened);
        std::fs::remove_file(&path).unwrap();
    }
    images.len()
}
