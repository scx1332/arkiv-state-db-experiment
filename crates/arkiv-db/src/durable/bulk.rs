//! Bounded external-sort construction of one genesis namespace.
use super::*;
use crate::layout::{Blob, Member, NamespaceState, Posting, StoredRecord, TermKey};
use crate::{
    Cell, CellName, Limits, NamespaceDescriptor, PriceSchedule, RecordId, RecordKey, STATE_FORMAT,
    StateHeader,
};
use arkiv_db_storage::versioned::{ObjectId, SpaceId};
use arkiv_db_trie::{MerkleMap, SortedTrieBuilder, TrieKey, TrieValue, TryObjectSink};
use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

fn io(e: impl fmt::Display) -> DurableError {
    incompatible(format!("bulk scratch: {e}"))
}
type Entry = (Vec<u8>, Vec<u8>);
type DiskCheck = std::rc::Rc<dyn Fn() -> Result<(), String>>;
type ProgressCallback = std::rc::Rc<dyn Fn(&str, u64)>;
struct Progress {
    callback: Option<ProgressCallback>,
    phase: &'static str,
    count: u64,
    last: std::time::Instant,
}
impl Progress {
    fn new(callback: Option<ProgressCallback>, phase: &'static str, announce: bool) -> Self {
        if announce && let Some(f) = &callback {
            f(phase, 0)
        }
        Self {
            callback,
            phase,
            count: 0,
            last: std::time::Instant::now(),
        }
    }
    fn tick(&mut self) {
        self.count += 1;
        if self.count.is_multiple_of(1024)
            && self.last.elapsed() >= std::time::Duration::from_secs(10)
        {
            self.report();
        }
    }
    fn report(&mut self) {
        if let Some(f) = &self.callback {
            f(self.phase, self.count)
        }
        self.last = std::time::Instant::now();
    }
}
const FAN_IN: usize = 16;
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_SORT_KEY: usize = TermKey::MAX_LEN + 8;
fn write_entry(w: &mut impl Write, e: &Entry) -> Result<(), DurableError> {
    for bytes in [&e.0, &e.1] {
        w.write_all(&(bytes.len() as u32).to_be_bytes())
            .map_err(io)?;
        w.write_all(bytes).map_err(io)?;
    }
    Ok(())
}
fn read_entry(r: &mut impl Read) -> Result<Option<Entry>, DurableError> {
    let mut n = [0; 4];
    let first = r.read(&mut n[..1]).map_err(io)?;
    if first == 0 {
        return Ok(None);
    }
    r.read_exact(&mut n[1..]).map_err(io)?;
    let a = u32::from_be_bytes(n) as usize;
    if a > MAX_SORT_KEY {
        return Err(io("oversized sort key"));
    }
    let mut key = vec![0; a];
    r.read_exact(&mut key).map_err(io)?;
    r.read_exact(&mut n).map_err(io)?;
    let b = u32::from_be_bytes(n) as usize;
    if b > MAX_FRAME {
        return Err(io("oversized sort value"));
    }
    let mut value = vec![0; b];
    r.read_exact(&mut value).map_err(io)?;
    Ok(Some((key, value)))
}
#[derive(Clone)]
struct Run {
    path: PathBuf,
    bytes: u64,
    hash: B256,
}
struct RunWriter {
    inner: BufWriter<File>,
    run: Run,
    hash: alloy_primitives::Keccak256,
}
impl RunWriter {
    fn new(path: PathBuf) -> Result<Self, DurableError> {
        Ok(Self {
            inner: BufWriter::new(File::create(&path).map_err(io)?),
            run: Run {
                path,
                bytes: 0,
                hash: B256::ZERO,
            },
            hash: alloy_primitives::Keccak256::new(),
        })
    }
    fn finish(mut self) -> Result<Run, DurableError> {
        self.flush().map_err(io)?;
        self.run.hash = self.hash.finalize();
        Ok(self.run)
    }
}
impl Write for RunWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(b)?;
        self.hash.update(&b[..n]);
        self.run.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
struct RunReader {
    inner: BufReader<File>,
    expected: Run,
    bytes: u64,
    hash: alloy_primitives::Keccak256,
}
impl RunReader {
    fn open(run: &Run) -> Result<Self, DurableError> {
        let file = File::open(&run.path).map_err(io)?;
        if file.metadata().map_err(io)?.len() != run.bytes {
            return Err(io("sort run length mismatch"));
        }
        Ok(Self {
            inner: BufReader::new(file),
            expected: run.clone(),
            bytes: 0,
            hash: alloy_primitives::Keccak256::new(),
        })
    }
}
impl Read for RunReader {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(b)?;
        self.hash.update(&b[..n]);
        self.bytes += n as u64;
        if n == 0
            && !b.is_empty()
            && (self.bytes != self.expected.bytes
                || self.hash.clone().finalize() != self.expected.hash)
        {
            return Err(std::io::Error::other("sort run integrity mismatch"));
        }
        Ok(n)
    }
}
struct Sorter {
    dir: PathBuf,
    cap: usize,
    used: usize,
    entries: Vec<Entry>,
    levels: Vec<Vec<Run>>,
    serial: u64,
    progress: Option<ProgressCallback>,
    disk_check: Option<DiskCheck>,
}
impl Sorter {
    fn new(dir: PathBuf, cap: usize) -> Result<Self, DurableError> {
        fs::create_dir(&dir).map_err(io)?;
        Ok(Self {
            dir,
            cap,
            used: 0,
            entries: Vec::new(),
            levels: Vec::new(),
            serial: 0,
            progress: None,
            disk_check: None,
        })
    }
    fn path(&mut self) -> PathBuf {
        let p = self.dir.join(format!("{:016x}.run", self.serial));
        self.serial += 1;
        p
    }
    fn add(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), DurableError> {
        let size = key.len() + value.len() + std::mem::size_of::<Entry>() * 2;
        if key.len() > MAX_SORT_KEY || value.len() > MAX_FRAME {
            return Err(io("oversized sort entry"));
        }
        if self.used + size > self.cap && !self.entries.is_empty() {
            self.flush()?
        }
        self.used += size;
        self.entries.push((key, value));
        Ok(())
    }
    fn flush(&mut self) -> Result<(), DurableError> {
        if self.entries.is_empty() {
            return Ok(());
        }
        if let Some(check) = &self.disk_check {
            check().map_err(io)?;
        }
        self.entries.sort_unstable();
        let path = self.path();
        let mut w = RunWriter::new(path)?;
        let mut checked = 0;
        for e in &self.entries {
            write_entry(&mut w, e)?;
            if w.run.bytes - checked >= 64 * 1024 * 1024 {
                if let Some(check) = &self.disk_check {
                    check().map_err(io)?;
                }
                checked = w.run.bytes;
            }
        }
        let path = w.finish()?;
        self.entries = Vec::new();
        self.used = 0;
        self.insert_run(0, path)
    }
    fn insert_run(&mut self, mut level: usize, mut path: Run) -> Result<(), DurableError> {
        loop {
            if self.levels.len() == level {
                self.levels.push(Vec::new())
            }
            self.levels[level].push(path);
            if self.levels[level].len() < FAN_IN {
                return Ok(());
            }
            let runs = std::mem::take(&mut self.levels[level]);
            path = self.merge(runs)?;
            level += 1;
        }
    }
    fn merge(&mut self, paths: Vec<Run>) -> Result<Run, DurableError> {
        if let Some(check) = &self.disk_check {
            check().map_err(io)?;
        }
        let mut progress = Progress::new(self.progress.clone(), "sort_merge", false);
        let out = self.path();
        let mut readers = paths
            .iter()
            .map(RunReader::open)
            .collect::<Result<Vec<_>, _>>()?;
        let mut heads = readers
            .iter_mut()
            .map(read_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let mut w = RunWriter::new(out)?;
        let mut checked = 0;
        loop {
            let Some(i) = (0..heads.len())
                .filter(|&i| heads[i].is_some())
                .min_by(|&a, &b| heads[a].cmp(&heads[b]))
            else {
                break;
            };
            write_entry(&mut w, &heads[i].take().unwrap())?;
            heads[i] = read_entry(&mut readers[i])?;
            progress.tick();
            if w.run.bytes - checked >= 64 * 1024 * 1024 {
                if let Some(check) = &self.disk_check {
                    check().map_err(io)?;
                }
                checked = w.run.bytes;
            }
        }
        let out = w.finish()?;
        drop(readers);
        for p in paths {
            fs::remove_file(p.path).map_err(io)?
        }
        Ok(out)
    }
    fn finish(mut self) -> Result<RunReader, DurableError> {
        self.flush()?;
        let mut paths = self.levels.drain(..).flatten().collect::<Vec<_>>();
        while paths.len() > 1 {
            let n = paths.len().min(FAN_IN);
            let group = paths.drain(..n).collect();
            paths.push(self.merge(group)?);
        }
        let path = if let Some(p) = paths.pop() {
            p
        } else {
            let p = self.path();
            RunWriter::new(p)?.finish()?
        };
        RunReader::open(&path)
    }
}

struct Sink {
    store: Arc<dyn VersionedStore>,
    space: SpaceId,
    options: Options,
    pending: Vec<(SpaceId, ObjectId, Vec<u8>)>,
    bytes: usize,
    max_object: usize,
    disk_check: Option<DiskCheck>,
}
impl From<arkiv_db_trie::SortedTrieError<DurableError>> for DurableError {
    fn from(e: arkiv_db_trie::SortedTrieError<DurableError>) -> Self {
        match e {
            arkiv_db_trie::SortedTrieError::Sink(e) => e,
            other => io(other),
        }
    }
}
impl Sink {
    fn flush(&mut self) -> Result<(), DurableError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if let Some(check) = &self.disk_check {
            check().map_err(io)?;
        }
        match self.store.stage_objects(&self.pending) {
            CommitOutcome::Committed => {
                self.pending.clear();
                self.bytes = 0;
                Ok(())
            }
            CommitOutcome::NotCommitted(e) | CommitOutcome::Indeterminate(e) => Err(e.into()),
        }
    }
}
impl TryObjectSink for Sink {
    type Error = DurableError;
    fn record(&mut self, id: B256, bytes: &[u8]) -> Result<(), DurableError> {
        self.max_object = self.max_object.max(bytes.len());
        let size = bytes.len() + 64;
        if size > self.options.prepare_bytes || self.options.prepare_objects == 0 {
            return Err(DurableError::ResourceLimit(
                "bulk object exceeds staging cap".into(),
            ));
        }
        if self.bytes + size > self.options.prepare_bytes
            || self.pending.len() >= self.options.prepare_objects
        {
            self.flush()?
        }
        self.pending.push((self.space, id.0, bytes.to_vec()));
        self.bytes += size;
        Ok(())
    }
}

/// Successfully staged state. Its private fields prevent publishing an arbitrary header.
pub struct BulkBuild {
    store: Arc<dyn VersionedStore>,
    header: StateHeader,
    limits: Limits,
    max_object: usize,
}
impl BulkBuild {
    pub fn state_header(&self) -> &StateHeader {
        &self.header
    }
}
/// Writes only scratch runs until finish; input order assigns stable ordinal record IDs.
pub struct BulkGenesisBuilder {
    descriptor: NamespaceDescriptor,
    limits: Limits,
    next: u64,
    keys: Sorter,
    rows: Sorter,
    terms: Sorter,
    host: Sorter,
    failed: bool,
}
impl BulkGenesisBuilder {
    pub fn new(
        scratch: &Path,
        descriptor: NamespaceDescriptor,
        limits: Limits,
        sort_buffer_bytes: usize,
    ) -> Result<Self, DurableError> {
        limits.validate().map_err(io)?;
        if descriptor.id.ordinal() != 1 || !(4096..=1024 * 1024 * 1024).contains(&sort_buffer_bytes)
        {
            return Err(io("bulk requires namespace1 and sort buffer4096..=1GiB"));
        }
        fs::create_dir_all(scratch).map_err(io)?;
        let cap = sort_buffer_bytes / 4;
        Ok(Self {
            descriptor,
            limits,
            next: 1,
            keys: Sorter::new(scratch.join("keys"), cap)?,
            rows: Sorter::new(scratch.join("rows"), cap)?,
            terms: Sorter::new(scratch.join("terms"), cap)?,
            host: Sorter::new(scratch.join("host"), cap)?,
            failed: false,
        })
    }
    /// Check caller disk reserve before every scratch flush, merge and object batch.
    pub fn set_disk_check(&mut self, check: Box<dyn Fn() -> Result<(), String>>) {
        let check: DiskCheck = check.into();
        self.keys.disk_check = Some(check.clone());
        self.rows.disk_check = Some(check.clone());
        self.terms.disk_check = Some(check.clone());
        self.host.disk_check = Some(check);
    }
    /// Optional operational progress; phase counters do not affect canonical state.
    pub fn set_progress(&mut self, callback: impl Fn(&str, u64) + 'static) {
        let callback: ProgressCallback = std::rc::Rc::new(callback);
        self.keys.progress = Some(callback.clone());
        self.rows.progress = Some(callback.clone());
        self.terms.progress = Some(callback.clone());
        self.host.progress = Some(callback);
    }
    pub fn add_record(
        &mut self,
        key: RecordKey,
        mut cells: Vec<(CellName, Cell)>,
    ) -> Result<RecordId, DurableError> {
        if self.failed {
            return Err(io("builder previously failed"));
        }
        self.failed = true;
        crate::typed::validate_create(&self.limits, &cells)
            .map_err(|e| io(format!("invalid record: {e:?}")))?;
        let id =
            RecordId::from_ordinal(self.next).ok_or_else(|| io("record allocator exhausted"))?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| io("record allocator exhausted"))?;
        cells.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        self.keys
            .add(TrieKey::encode(&key), TrieValue::encode(&id))?;
        for (name, cell) in &cells {
            if cell.is_attribute() {
                for term in [
                    TermKey::existence(name, cell.value.tag()),
                    TermKey::value(name, &cell.value),
                ] {
                    let mut k = TrieKey::encode(&term);
                    k.extend(TrieKey::encode(&id));
                    self.terms.add(k, Vec::new())?;
                }
            }
        }
        self.rows.add(
            TrieKey::encode(&id),
            TrieValue::encode(&StoredRecord {
                key,
                cells: cells.into(),
            }),
        )?;
        self.failed = false;
        Ok(id)
    }
    pub fn add_host(&mut self, key: [u8; 32], bytes: Vec<u8>) -> Result<(), DurableError> {
        if self.failed {
            return Err(io("builder previously failed"));
        }
        self.failed = true;
        if bytes.len() > crate::HOST_VALUE_MAX {
            return Err(io("host value exceeds limit"));
        }
        let mut v = vec![0];
        v.extend(bytes);
        self.host.add(key.to_vec(), v)?;
        self.failed = false;
        Ok(())
    }
    pub fn add_host_count(
        &mut self,
        key: [u8; 32],
        count: u64,
        cap: u64,
    ) -> Result<(), DurableError> {
        if self.failed {
            return Err(io("builder previously failed"));
        }
        self.failed = true;
        let mut v = vec![1];
        v.extend(count.to_be_bytes());
        v.extend(cap.to_be_bytes());
        self.host.add(key.to_vec(), v)?;
        self.failed = false;
        Ok(())
    }
    pub fn finish(
        self,
        target: Arc<dyn VersionedStore>,
        pricing: PriceSchedule,
        options: Options,
    ) -> Result<BulkBuild, DurableError> {
        if self.failed {
            return Err(io("builder previously failed"));
        }
        options.validate()?;
        if target.head()?.is_some() {
            return Err(io("bulk target must be headless"));
        }
        let mut sink = Sink {
            store: target.clone(),
            space: SpaceId::Namespace(1),
            options,
            pending: Vec::new(),
            bytes: 0,
            max_object: 0,
            disk_check: self.keys.disk_check.clone(),
        };
        let callback = self.keys.progress.clone();
        let keys = build_map::<RecordKey>(self.keys, &mut sink, "keys")?;
        let rows = build_map::<RecordId>(self.rows, &mut sink, "rows")?;
        let mut progress = Progress::new(callback.clone(), "postings", true);
        let mut terms = SortedTrieBuilder::<TermKey>::new();
        let mut input = self.terms.finish()?;
        let mut current: Option<Vec<u8>> = None;
        let mut posting = SortedTrieBuilder::<RecordId>::new();
        while let Some((key, _)) = read_entry(&mut input)? {
            progress.tick();
            if key.len() < 8 {
                return Err(io("invalid posting entry"));
            }
            let split = key.len() - 8;
            let term = &key[..split];
            if current.as_deref() != Some(term) {
                if let Some(old) = current.take() {
                    let (root, len) = posting.finish(&mut sink)?;
                    terms.push(
                        &old,
                        &Posting(MerkleMap::from_root(root, len).map_err(io)?).encode(),
                        &mut sink,
                    )?;
                }
                current = Some(term.to_vec());
                posting = SortedTrieBuilder::new();
            }
            posting.push(&key[split..], &Member.encode(), &mut sink)?;
        }
        if let Some(old) = current {
            let (root, len) = posting.finish(&mut sink)?;
            terms.push(
                &old,
                &Posting(MerkleMap::from_root(root, len).map_err(io)?).encode(),
                &mut sink,
            )?;
        }
        let terms = terms.finish(&mut sink)?;
        progress.report();
        let state = NamespaceState {
            descriptor: self.descriptor,
            records: MerkleMap::new(),
            typed: crate::typed::TypedState {
                next_record: self.next,
                keys: MerkleMap::from_root(keys.0, keys.1).map_err(io)?,
                rows: MerkleMap::from_root(rows.0, rows.1).map_err(io)?,
                terms: MerkleMap::from_root(terms.0, terms.1).map_err(io)?,
            },
        };
        sink.flush()?;
        sink.space = SpaceId::Catalog;
        let mut catalog = SortedTrieBuilder::<crate::NamespaceId>::new();
        catalog.push(
            &TrieKey::encode(&self.descriptor.id),
            &state.encode(),
            &mut sink,
        )?;
        let catalog = catalog.finish(&mut sink)?;
        sink.flush()?;
        sink.space = SpaceId::Host;
        let mut progress = Progress::new(callback, "host", true);
        let mut host = SortedTrieBuilder::<[u8; 32]>::new();
        let mut input = self.host.finish()?;
        let mut pending = read_entry(&mut input)?;
        while let Some((key, value)) = pending.take() {
            progress.tick();
            let mut next = read_entry(&mut input)?;
            let bytes = match value.first() {
                Some(0) => {
                    if next.as_ref().is_some_and(|v| v.0 == key) {
                        return Err(io("duplicate host key"));
                    }
                    value[1..].to_vec()
                }
                Some(1) if value.len() == 17 => {
                    let mut sum = u64::from_be_bytes(value[1..9].try_into().unwrap());
                    let cap = u64::from_be_bytes(value[9..17].try_into().unwrap());
                    while next.as_ref().is_some_and(|v| v.0 == key) {
                        let (_, v) = next.take().unwrap();
                        if v.len() != 17 || v[0] != 1 || v[9..17] != value[9..17] {
                            return Err(io("conflicting host count"));
                        }
                        sum = sum
                            .checked_add(u64::from_be_bytes(v[1..9].try_into().unwrap()))
                            .ok_or_else(|| io("host count overflow"))?;
                        next = read_entry(&mut input)?;
                        progress.tick();
                    }
                    if sum > cap {
                        return Err(io("host count cap exceeded"));
                    }
                    sum.to_be_bytes().to_vec()
                }
                _ => return Err(io("invalid host run")),
            };
            host.push(&key, &Blob(bytes.into()).encode(), &mut sink)?;
            pending = next;
        }
        let host = host.finish(&mut sink)?;
        sink.flush()?;
        progress.report();
        Ok(BulkBuild {
            store: target,
            limits: self.limits,
            max_object: sink.max_object,
            header: StateHeader {
                format: STATE_FORMAT,
                catalog_root: catalog.0,
                catalog_len: catalog.1,
                next_namespace: 2,
                pricing,
                host_root: host.0,
                host_len: host.1,
            },
        })
    }
}
fn build_map<K: TrieKey>(
    sorter: Sorter,
    sink: &mut Sink,
    phase: &'static str,
) -> Result<(B256, u64), DurableError> {
    let mut progress = Progress::new(sorter.progress.clone(), phase, true);
    let mut input = sorter.finish()?;
    let mut map = SortedTrieBuilder::<K>::new();
    while let Some((k, v)) = read_entry(&mut input)? {
        map.push(&k, &v, sink)?;
        progress.tick();
    }
    let result = map.finish(sink)?;
    progress.report();
    Ok(result)
}
impl DurableEngine {
    /// Atomically publishes only a successfully completed, store-bound bulk build.
    pub fn initialize_bulk(
        build: BulkBuild,
        genesis: EngineGenesis,
        options: Options,
    ) -> Result<Initialization, DurableError> {
        genesis.validate()?;
        options.validate()?;
        if genesis.initial_state.as_ref() != Some(&build.header)
            || genesis.limits != build.limits
            || genesis.pricing != build.header.pricing
        {
            return Err(io("bulk genesis association"));
        }
        if build.max_object as u64 > genesis.max_object_bytes {
            return Err(io("bulk object exceeds genesis object cap"));
        }
        let target = build.store;
        if target.head()?.is_some() {
            return Err(io("bulk target already published"));
        }
        for key in [PROFILE_KEY, GENESIS_KEY, simulator::IDENTITY_KEY] {
            if target
                .read_record_bounded(
                    Table::Meta,
                    key,
                    GENESIS_MAX_BYTES + simulator::IDENTITY_MAX_BYTES + 32,
                )?
                .is_some()
            {
                return Err(io("bulk target has metadata"));
            }
        }
        let manifest = EngineManifest {
            genesis: genesis.id(),
            sequence: 0,
            parent: None,
            state_root: build.header.root(),
            state: build.header,
        };
        let mut batch = manifest_batch(None, &manifest);
        batch
            .put_record(Table::Meta, PROFILE_KEY, profile_bytes())
            .put_record(Table::Meta, GENESIS_KEY, genesis.envelope());
        if batch.logical_bytes() > options.prepare_bytes as u64 {
            return Err(DurableError::ResourceLimit("bulk metadata cap".into()));
        }
        let engine = Self::from_manifest(target, genesis, options, manifest, None)?;
        InitializationPending {
            engine,
            batch,
            retry_allowed: true,
        }
        .commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Budget, Cell, NamespaceId, NamespaceName, OwnerId, Revision, Value, WriteOp};
    use arkiv_db_memory::MemoryVersionedStore;
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "arkiv-bulk-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn descriptor() -> NamespaceDescriptor {
        NamespaceDescriptor {
            id: NamespaceId::from_ordinal(1).unwrap(),
            name: NamespaceName::new("entities").unwrap(),
            owner: OwnerId::new([1; 20]),
            revision: Revision::INITIAL,
        }
    }
    fn engine(init: Initialization) -> DurableEngine {
        match init {
            Initialization::Committed(e) => e,
            _ => panic!("commit failed"),
        }
    }
    #[test]
    fn authenticated_runs_reject_boundary_truncation_and_value_flip() {
        for truncate in [false, true] {
            let temp = Temp::new();
            fs::create_dir_all(&temp.0).unwrap();
            let mut writer = RunWriter::new(temp.0.join("run")).unwrap();
            write_entry(&mut writer, &(vec![1], vec![2])).unwrap();
            write_entry(&mut writer, &(vec![3], vec![4])).unwrap();
            let run = writer.finish().unwrap();
            let mut bytes = fs::read(&run.path).unwrap();
            if truncate {
                bytes.truncate(10);
            } else {
                bytes[9] ^= 1;
            }
            fs::write(&run.path, bytes).unwrap();
            let result = RunReader::open(&run).and_then(|mut reader| {
                while read_entry(&mut reader)?.is_some() {}
                Ok(())
            });
            assert!(result.is_err());
        }
    }
    #[test]
    fn bulk_matches_incremental_with_multiple_merge_levels_and_reopen() {
        let temp = Temp::new();
        let mut builder =
            BulkGenesisBuilder::new(&temp.0, descriptor(), Limits::DEFAULT, 4096).unwrap();
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let captured = events.clone();
        builder.set_progress(move |phase, count| {
            captured.borrow_mut().push((phase.to_owned(), count))
        });
        let initial = EngineGenesis::new([1; 32], PriceSchedule::FIXTURE);
        let memory = Arc::new(MemoryVersionedStore::new());
        let oracle =
            engine(DurableEngine::initialize(memory, initial.clone(), Options::default()).unwrap());
        let mut branch = oracle.begin(oracle.head(), None).unwrap();
        branch
            .try_execute(
                descriptor().owner,
                WriteOp::CreateNamespace {
                    name: descriptor().name,
                },
                Budget::unlimited(),
            )
            .unwrap()
            .unwrap();
        for n in (0..300u64).rev() {
            let key = RecordKey::new(&n.to_be_bytes()).unwrap();
            let cells = vec![
                (
                    CellName::new("shared").unwrap(),
                    Cell::attribute(Value::str("same")),
                ),
                (
                    CellName::new("number").unwrap(),
                    Cell::attribute(Value::U64(n % 7)),
                ),
                (
                    CellName::new("body").unwrap(),
                    Cell::field(Value::bytes(&[3; 64])),
                ),
            ];
            builder.add_record(key.clone(), cells.clone()).unwrap();
            branch
                .try_execute(
                    descriptor().owner,
                    WriteOp::CreateRecord {
                        namespace: descriptor().id,
                        key,
                        cells,
                    },
                    Budget::unlimited(),
                )
                .unwrap()
                .unwrap();
        }
        builder.add_host([4; 32], vec![1, 2, 3]).unwrap();
        branch.try_host_put([4; 32], &[1, 2, 3]).unwrap().unwrap();
        for _ in 0..100 {
            builder.add_host_count([5; 32], 1, 100).unwrap();
        }
        branch
            .try_host_put([5; 32], &100u64.to_be_bytes())
            .unwrap()
            .unwrap();
        let expected = branch.seal().state_header();
        let target = Arc::new(MemoryVersionedStore::new());
        let options = Options {
            prepare_objects: 3,
            prepare_bytes: 4096,
            ..Options::default()
        };
        let build = builder
            .finish(target.clone(), PriceSchedule::FIXTURE, options.clone())
            .unwrap();
        assert_eq!(build.state_header(), &expected);
        assert!(events.borrow().contains(&("rows".to_owned(), 300)));
        assert!(events.borrow().contains(&("postings".to_owned(), 1200)));
        assert!(target.head().unwrap().is_none());
        let mut genesis = initial;
        genesis.initial_state = Some(expected.clone());
        let published = engine(
            DurableEngine::initialize_bulk(build, genesis.clone(), options.clone()).unwrap(),
        );
        assert_eq!(published.head_snapshot().state_header(), expected);
        drop(published);
        let reopened = DurableEngine::open(target, genesis, options).unwrap();
        assert_eq!(reopened.head_snapshot().state_header(), expected);
    }
    #[test]
    fn rejects_duplicates_caps_and_failed_builders_without_publication() {
        for mode in 0..5 {
            let temp = Temp::new();
            let mut b =
                BulkGenesisBuilder::new(&temp.0, descriptor(), Limits::DEFAULT, 4096).unwrap();
            let target = Arc::new(MemoryVersionedStore::new());
            match mode {
                0 => {
                    for _ in 0..2 {
                        b.add_record(RecordKey::new(b"same").unwrap(), vec![])
                            .unwrap();
                    }
                }
                1 => {
                    b.add_host([1; 32], vec![1]).unwrap();
                    b.add_host([1; 32], vec![2]).unwrap();
                }
                2 => {
                    b.add_host_count([1; 32], 1, 1).unwrap();
                    b.add_host_count([1; 32], 1, 1).unwrap();
                }
                3 => {
                    b.add_host_count([1; 32], u64::MAX, u64::MAX).unwrap();
                    b.add_host_count([1; 32], 1, u64::MAX).unwrap();
                }
                _ => {
                    assert!(
                        b.add_host([1; 32], vec![0; crate::HOST_VALUE_MAX + 1])
                            .is_err()
                    );
                    assert!(b.add_host([2; 32], vec![1]).is_err());
                }
            }
            assert!(
                b.finish(target.clone(), PriceSchedule::FIXTURE, Options::default())
                    .is_err()
            );
            assert!(target.head().unwrap().is_none());
        }
    }
    #[test]
    fn disk_failure_and_genesis_mismatch_never_publish() {
        let temp = Temp::new();
        let mut b = BulkGenesisBuilder::new(&temp.0, descriptor(), Limits::DEFAULT, 4096).unwrap();
        b.set_disk_check(Box::new(|| Err("reserve".into())));
        b.add_host([1; 32], vec![2]).unwrap();
        let target = Arc::new(MemoryVersionedStore::new());
        assert!(
            b.finish(target.clone(), PriceSchedule::FIXTURE, Options::default())
                .is_err()
        );
        assert!(target.head().unwrap().is_none());
        let temp = Temp::new();
        let b = BulkGenesisBuilder::new(&temp.0, descriptor(), Limits::DEFAULT, 4096).unwrap();
        let build = b
            .finish(target.clone(), PriceSchedule::FIXTURE, Options::default())
            .unwrap();
        assert!(
            DurableEngine::initialize_bulk(
                build,
                EngineGenesis::new([1; 32], PriceSchedule::FIXTURE),
                Options::default()
            )
            .is_err()
        );
        assert!(target.head().unwrap().is_none());
    }
}
