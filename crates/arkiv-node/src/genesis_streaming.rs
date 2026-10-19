//! Version-two file-backed prefill with bounded external-sort genesis construction.
//! Live execution is not used to build or update intermediate import states.
use crate::{
    genesis_bundle::{PrefillEntity, PrefillLaunch, PrefillLimits, recipe_hash},
    signed::protocol::SignedGenesis,
};
use alloy_primitives::{Keccak256, U256};
use arkiv_db::{
    B256, Decoder, decode_exact, digest,
    durable::{DurableEngine, Options},
    encode,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Seek, Write},
    path::Path,
    time::{Duration, Instant},
};

pub use crate::genesis_import_resources::{check_disk, import_redb_config};

type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Debug) -> String {
    format!("streaming prefill: {e:?}")
}
pub const STREAM_DOMAIN: &str = "arkiv/prefill-stream/v2";
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_USER_ATTRIBUTES: u32 = 55;
pub const MAX_ENTITIES: u64 = 100_915_200;
pub const MAX_CANONICAL_BYTES: u64 = 1 << 40;
pub const MAX_STORAGE_UNITS: u64 = 1 << 50;
pub const DEFAULT_BATCH_BYTES: usize = 1024 * 1024;

/// Operational telemetry only: no paths, identities, or canonical fields change.
pub(crate) struct Progress {
    stage: &'static str,
    started: Instant,
    last: Option<Instant>,
    total_rows: u64,
    total_bytes: u64,
}
impl Progress {
    pub(crate) fn new(stage: &'static str, total_rows: u64, total_bytes: u64) -> Self {
        Self {
            stage,
            started: Instant::now(),
            last: None,
            total_rows,
            total_bytes,
        }
    }
    pub(crate) fn report(
        &mut self,
        rows: u64,
        bytes: u64,
        checkpoint: Option<u64>,
        complete: bool,
    ) {
        let now = Instant::now();
        if !complete
            && self
                .last
                .is_some_and(|last| now.duration_since(last) < Duration::from_secs(10))
        {
            return;
        }
        self.last = Some(now);
        let record = serde_json::json!({
            "event": "prefill_progress", "stage": self.stage,
            "rows": rows, "totalRows": self.total_rows,
            "bytes": bytes, "totalBytes": self.total_bytes,
            "elapsedSeconds": now.duration_since(self.started).as_secs_f64(),
            "checkpointSequence": checkpoint, "complete": complete,
        });
        // Telemetry errors must never change import or publication behavior.
        let _ = writeln!(std::io::stderr().lock(), "{record}");
    }
}

pub fn validate_limits(l: PrefillLimits) -> Result<()> {
    if l.max_entities > MAX_ENTITIES
        || !(20..=MAX_CANONICAL_BYTES).contains(&l.max_canonical_bytes)
        || l.max_storage_units > MAX_STORAGE_UNITS
    {
        return Err("streaming prefill exceeds version-two admission limits".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSummary {
    pub rows: u64,
    pub bytes: u64,
    pub hash: B256,
}

/// One bounded frame and one preceding creator/nonce, independent of row count.
pub struct StreamReader<R> {
    reader: R,
    hasher: Keccak256,
    pub rows: u64,
    pub consumed: u64,
    pub bytes: u64,
    previous: Option<([u8; 20], u64)>,
    max_bytes: u64,
}
impl<R: Read> StreamReader<R> {
    pub fn new(mut reader: R, limits: PrefillLimits) -> Result<Self> {
        validate_limits(limits)?;
        let mut header = [0u8; 20];
        reader.read_exact(&mut header).map_err(err)?;
        if &header[..8] != b"ARKPFL02" || header[8..12] != 2u32.to_be_bytes() {
            return Err("invalid streaming prefill header".into());
        }
        let rows = u64::from_be_bytes(header[12..20].try_into().unwrap());
        if rows > limits.max_entities {
            return Err("streaming prefill row limit".into());
        }
        let mut hasher = Keccak256::new();
        hasher.update([STREAM_DOMAIN.len() as u8]);
        hasher.update(STREAM_DOMAIN.as_bytes());
        hasher.update(header);
        Ok(Self {
            reader,
            hasher,
            rows,
            consumed: 0,
            bytes: 20,
            previous: None,
            max_bytes: limits.max_canonical_bytes,
        })
    }
    pub fn prefix_hash(&self) -> B256 {
        self.hasher.clone().finalize()
    }
    pub fn next_row(&mut self) -> Result<Option<PrefillEntity>> {
        if self.consumed == self.rows {
            return Ok(None);
        }
        let mut length = [0u8; 4];
        self.reader.read_exact(&mut length).map_err(err)?;
        let length_usize = u32::from_be_bytes(length) as usize;
        if !(65..=MAX_FRAME_BYTES).contains(&length_usize) {
            return Err("invalid streaming prefill frame length".into());
        }
        let next_bytes = self
            .bytes
            .checked_add(4 + length_usize as u64)
            .ok_or("stream size overflow")?;
        if next_bytes > self.max_bytes {
            return Err("streaming prefill byte limit".into());
        }
        let mut frame = vec![0u8; length_usize];
        self.reader.read_exact(&mut frame).map_err(err)?;
        let mut decoder = Decoder::new(&frame);
        let row = PrefillEntity::decode_with_attribute_limit(&mut decoder, MAX_USER_ATTRIBUTES)
            .map_err(err)?;
        if decoder.remaining() != 0 || encode(&row) != frame {
            return Err("noncanonical prefill frame".into());
        }
        validate_row(&row)?;
        match self.previous {
            None if row.create_nonce != 0 => return Err("creator nonce must begin at zero".into()),
            Some((creator, nonce))
                if creator == row.creator && nonce.checked_add(1) != Some(row.create_nonce) =>
            {
                return Err("creator nonce gap or duplicate".into());
            }
            Some((creator, _))
                if creator != row.creator && (row.creator < creator || row.create_nonce != 0) =>
            {
                return Err("noncanonical creator order or initial nonce".into());
            }
            _ => {}
        }
        self.previous = Some((row.creator, row.create_nonce));
        self.hasher.update(length);
        self.hasher.update(&frame);
        self.bytes = next_bytes;
        self.consumed += 1;
        Ok(Some(row))
    }
    pub fn finish(mut self) -> Result<StreamSummary> {
        if self.consumed != self.rows {
            return Err("unconsumed prefill rows".into());
        }
        let mut trailing = [0u8; 1];
        if self.reader.read(&mut trailing).map_err(err)? != 0 {
            return Err("trailing prefill bytes".into());
        }
        Ok(StreamSummary {
            rows: self.rows,
            bytes: self.bytes,
            hash: self.hasher.finalize(),
        })
    }
}
fn validate_row(row: &PrefillEntity) -> Result<()> {
    if row.attributes.len() > MAX_USER_ATTRIBUTES as usize
        || row.attributes.windows(2).any(|w| w[0].0 >= w[1].0)
        || row
            .attributes
            .iter()
            .any(|(name, value)| name.as_str().starts_with('$') || value.encoded_len() > 128 * 1024)
        || row.payload.len() > 128 * 1024
        || row.content_type.len() > 128 * 1024
        || row.flags & !3 != 0
        || row.expires_at_height == 0
        || row.expires_at_height == u64::MAX
    {
        return Err("invalid streaming prefill row".into());
    }
    Ok(())
}
pub fn audit_stream(path: &Path, limits: PrefillLimits) -> Result<StreamSummary> {
    audit_file(&mut File::open(path).map_err(err)?, limits)
}
pub(crate) fn audit_file(file: &mut File, limits: PrefillLimits) -> Result<StreamSummary> {
    file.rewind().map_err(err)?;
    let bytes = file.metadata().map_err(err)?.len();
    let mut reader = StreamReader::new(BufReader::new(file), limits)?;
    let mut progress = Progress::new("audit", reader.rows, bytes);
    progress.report(0, reader.bytes, None, false);
    while reader.next_row()?.is_some() {
        progress.report(reader.consumed, reader.bytes, None, false);
    }
    let summary = reader.finish()?;
    progress.report(summary.rows, summary.bytes, None, true);
    Ok(summary)
}

/// An audited local inode, bound to one signed genesis. The descriptor is kept
/// open through bulk construction so replacing a pathname cannot substitute its input.
/// Construction still checks the complete stream hash to detect changes to the inode.
pub struct VerifiedStream {
    file: File,
    summary: StreamSummary,
    genesis_hash: B256,
}
impl VerifiedStream {
    pub fn open(g: &SignedGenesis, path: &Path) -> Result<Self> {
        Self::from_file(g, File::open(path).map_err(err)?)
    }
    /// Audit the actual descriptor the caller retained or copied.
    pub fn from_file(g: &SignedGenesis, mut file: File) -> Result<Self> {
        g.validate().map_err(err)?;
        let p = g
            .prefill
            .as_ref()
            .filter(|p| p.version == 2)
            .ok_or("expected streaming prefill v2")?;
        let summary = audit_file(&mut file, p.limits)?;
        if summary.rows != p.row_count
            || summary.bytes != p.canonical_bytes
            || summary.hash != p.stream_hash
        {
            return Err("prefill commitment mismatch".into());
        }
        Ok(Self {
            file,
            summary,
            genesis_hash: g.hash(),
        })
    }
    /// Check that a newly retained hard link names the inode we audited. This is
    /// an association check, not a substitute for hashing mutable file contents.
    pub fn check_path_identity(&self, path: &Path) -> Result<()> {
        let opened = rustix::fs::fstat(&self.file).map_err(err)?;
        let retained = rustix::fs::stat(path).map_err(err)?;
        if opened.st_dev != retained.st_dev || opened.st_ino != retained.st_ino {
            return Err("prefill pathname changed after verification".into());
        }
        Ok(())
    }
}

pub(crate) fn import_identity(
    base: &SignedGenesis,
    limits: PrefillLimits,
    sponsor: [u8; 20],
    summary: &StreamSummary,
) -> B256 {
    let mut base = base.clone();
    base.prefill = None;
    let mut bytes = encode(&base);
    bytes.extend_from_slice(&encode(&limits));
    bytes.extend_from_slice(&sponsor);
    bytes.extend_from_slice(summary.hash.as_slice());
    bytes.extend_from_slice(&summary.rows.to_be_bytes());
    bytes.extend_from_slice(&summary.bytes.to_be_bytes());
    digest("arkiv/prefill-import/v2", &bytes)
}
// Retain the CLI byte option as a bounded ingestion tuning input. It never
// selects transaction replay or changes canonical state.
#[allow(clippy::too_many_arguments)]
fn bulk_build(
    base: &SignedGenesis,
    limits: PrefillLimits,
    sponsor: [u8; 20],
    path: &Path,
    work_dir: &Path,
    options: Options,
    batch_bytes: usize,
    summary: &StreamSummary,
) -> Result<(DurableEngine, U256, u64)> {
    crate::genesis_bulk::build_file(
        base,
        limits,
        sponsor,
        &mut File::open(path).map_err(err)?,
        work_dir,
        options,
        batch_bytes,
        summary,
    )
}

/// Verify bounded canonical frames and the signed stream commitment without state replay.
pub fn verify_stream_file(g: &SignedGenesis, stream: &Path) -> Result<()> {
    VerifiedStream::open(g, stream).map(|_| ())
}
/// Independently reconstruct an authenticated launch into a private bulk store.
pub fn prepare_streaming(
    g: &SignedGenesis,
    stream: &Path,
    work_dir: &Path,
    options: Options,
) -> Result<DurableEngine> {
    prepare_verified_streaming(g, VerifiedStream::open(g, stream)?, work_dir, options)
}
/// Reuse an audit of the exact local inode, retaining end-to-end stream checks.
pub fn prepare_verified_streaming(
    g: &SignedGenesis,
    mut stream: VerifiedStream,
    work_dir: &Path,
    options: Options,
) -> Result<DurableEngine> {
    g.validate().map_err(err)?;
    if stream.genesis_hash != g.hash() {
        return Err("verified prefill genesis mismatch".into());
    }
    let p = g
        .prefill
        .as_ref()
        .filter(|p| p.version == 2)
        .ok_or("expected streaming prefill v2")?;
    let (engine, burn, units) = crate::genesis_bulk::build_file(
        g,
        p.limits,
        p.sponsor,
        &mut stream.file,
        work_dir,
        crate::genesis_import_resources::import_options(options),
        DEFAULT_BATCH_BYTES,
        &stream.summary,
    )?;
    if engine.head_snapshot().state_header() != p.state
        || burn != p.burned_wei
        || units != p.storage_units
    {
        return Err("prefill derived state/rent mismatch".into());
    }
    Ok(engine)
}

/// Build a new portable bundle. A deterministic sibling work directory holds
/// private bulk work; only a completely validated output is renamed into place.
pub fn build_bundle(
    base_path: &Path,
    stream: &Path,
    output: &Path,
    sponsor: [u8; 20],
    batch_bytes: usize,
) -> Result<serde_json::Value> {
    if output.exists() {
        return Err("bundle destination already exists".into());
    }
    let file = File::open(base_path).map_err(err)?;
    if file.metadata().map_err(err)?.len() > 65536 {
        return Err("base genesis exceeds 64 KiB".into());
    }
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes).map_err(err)?;
    let mut base: SignedGenesis = decode_exact(&bytes).map_err(err)?;
    base.prefill = None;
    // v2 explicitly extends the cell profile; pricing and expiry reservation must
    // already be selected by the caller's native genesis/profile constructor.
    base.genesis.limits.max_cells = 64;
    let required =
        crate::params::max_expiry_delete_cost(&base.genesis.pricing, &base.genesis.limits)
            .and_then(|v| v.checked_mul(u64::from(base.genesis.params.expiry_per_block)))
            .and_then(|v| v.checked_add(base.genesis.params.user_budget))
            .ok_or("expiry reservation overflow")?;
    base.genesis.params.block_budget = base.genesis.params.block_budget.max(required);
    let limits = PrefillLimits {
        max_entities: MAX_ENTITIES,
        max_canonical_bytes: MAX_CANONICAL_BYTES,
        max_storage_units: MAX_STORAGE_UNITS,
    };
    let summary = audit_stream(stream, limits)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(err)?;
    let name = output
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("invalid bundle output name")?;
    let work = parent.join(format!(".{name}.stream-work"));
    let (engine, burn, units) = bulk_build(
        &base,
        limits,
        sponsor,
        stream,
        &work,
        crate::genesis_import_resources::import_options(Options::default()),
        batch_bytes,
        &summary,
    )?;
    let mut launch = PrefillLaunch {
        version: 2,
        limits,
        sponsor,
        stream_hash: summary.hash,
        row_count: summary.rows,
        canonical_bytes: summary.bytes,
        recipe_hash: B256::ZERO,
        state: engine.head_snapshot().state_header(),
        burned_wei: burn,
        storage_units: units,
    };
    launch.recipe_hash = recipe_hash(&base, &launch);
    base.prefill = Some(launch);
    base.validate().map_err(err)?;
    let report = summary_report(&base)?;
    let stage = parent.join(format!(".{name}.publish-{}", std::process::id()));
    fs::create_dir(&stage).map_err(err)?;
    let result: Result<()> = (|| {
        check_disk(&stage, summary.bytes)?;
        write_new(&stage.join("genesis.bin"), &encode(&base))?;
        let mut source = File::open(stream).map_err(err)?;
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(stage.join("prefill.bin"))
            .map_err(err)?;
        std::io::copy(
            &mut Read::by_ref(&mut source).take(summary.bytes + 1),
            &mut target,
        )
        .map_err(err)?;
        target.sync_all().map_err(err)?;
        if audit_stream(&stage.join("prefill.bin"), limits)? != summary {
            return Err("prefill changed during bundle publication".into());
        }
        write_new(
            &stage.join("report.json"),
            &serde_json::to_vec_pretty(&report).map_err(err)?,
        )?;
        File::open(&stage).map_err(err)?.sync_all().map_err(err)?;
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            &stage,
            rustix::fs::CWD,
            output,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(err)?;
        File::open(parent).map_err(err)?.sync_all().map_err(err)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result?;
    drop(engine);
    fs::remove_dir_all(&work)
        .map_err(|e| format!("bundle published; staging cleanup failed: {e}"))?;
    File::open(parent).map_err(err)?.sync_all().map_err(err)?;
    Ok(report)
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}
pub fn summary_report(g: &SignedGenesis) -> Result<serde_json::Value> {
    let p = g.prefill.as_ref().ok_or("missing prefill")?;
    Ok(
        serde_json::json!({"version":2,"genesisHash":g.hash().to_string(),"stateRoot":p.state.root().to_string(),"recipeHash":p.recipe_hash.to_string(),"streamHash":p.stream_hash.to_string(),"rowCount":p.row_count.to_string(),"canonicalBytes":p.canonical_bytes.to_string(),"storageUnits":p.storage_units.to_string(),"burnedWei":p.burned_wei.to_string(),"sponsor":format!("0x{}",p.sponsor.iter().map(|b|format!("{b:02x}")).collect::<String>())}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts;
    use crate::signed::protocol::{SignedLimits, default_native_genesis};
    use arkiv_db::{CellName, OwnerId, Value};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "arkiv-stream-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn base() -> SignedGenesis {
        SignedGenesis {
            genesis: default_native_genesis(9450, 1000),
            run_id: [7; 16],
            proposer: [1; 20],
            block_period_ms: 1000,
            limits: SignedLimits::default(),
            accounts: accounts::AccountConfig::new(vec![(
                OwnerId::new([2; 20]),
                U256::from(10).pow(U256::from(30)),
            )])
            .unwrap(),
            prefill: None,
        }
    }
    fn row(n: u64) -> PrefillEntity {
        PrefillEntity {
            creator: [2; 20],
            create_nonce: n,
            salt: n as u128,
            expires_at_height: 100 + n / 16,
            flags: 0,
            content_type: "text/plain".into(),
            payload: b"hello".to_vec(),
            attributes: vec![(CellName::new("label").unwrap(), Value::str("hello"))],
        }
    }
    fn limits() -> PrefillLimits {
        PrefillLimits {
            max_entities: MAX_ENTITIES,
            max_canonical_bytes: MAX_CANONICAL_BYTES,
            max_storage_units: MAX_STORAGE_UNITS,
        }
    }
    fn stream(rows: &[PrefillEntity]) -> Vec<u8> {
        let mut b = b"ARKPFL02".to_vec();
        b.extend_from_slice(&2u32.to_be_bytes());
        b.extend_from_slice(&(rows.len() as u64).to_be_bytes());
        for row in rows {
            let f = encode(row);
            b.extend_from_slice(&(f.len() as u32).to_be_bytes());
            b.extend(f);
        }
        b
    }
    fn signed_stream(rows: &[PrefillEntity]) -> SignedGenesis {
        let mut g = crate::genesis_bundle::build_genesis(
            base(),
            PrefillLimits {
                max_entities: 256,
                max_canonical_bytes: crate::genesis_bundle::MAX_CANONICAL_BYTES,
                max_storage_units: crate::genesis_bundle::MAX_STORAGE_UNITS,
            },
            [2; 20],
            rows,
        )
        .unwrap();
        let mut launch = g.prefill.take().unwrap();
        let bytes = stream(rows);
        launch.version = 2;
        launch.limits = limits();
        launch.stream_hash = digest(STREAM_DOMAIN, &bytes);
        launch.canonical_bytes = bytes.len() as u64;
        launch.recipe_hash = recipe_hash(&g, &launch);
        g.prefill = Some(launch);
        g.validate().unwrap();
        g
    }
    #[test]
    fn verified_descriptor_survives_path_replacement_and_standalone_verify_reaudits() {
        let tmp = Temp::new();
        let rows = [row(0), row(1)];
        let g = signed_stream(&rows);
        let path = tmp.0.join("prefill.bin");
        fs::write(&path, stream(&rows)).unwrap();
        let verified = VerifiedStream::open(&g, &path).unwrap();
        verified.check_path_identity(&path).unwrap();
        // The old inode stays open, while this pathname now names bad input.
        fs::rename(&path, tmp.0.join("original.bin")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(verified.check_path_identity(&path).is_err());
        let engine =
            prepare_verified_streaming(&g, verified, &tmp.0.join("work"), Options::default())
                .unwrap();
        assert_eq!(
            engine.head_snapshot().state_header(),
            g.prefill.as_ref().unwrap().state
        );
        assert!(verify_stream_file(&g, &path).is_err());
        assert!(prepare_streaming(&g, &path, &tmp.0.join("bad"), Options::default()).is_err());
    }
    #[test]
    fn verified_bulk_rejects_inode_mutation_truncation_and_changed_genesis() {
        let tmp = Temp::new();
        let rows = [row(0), row(1)];
        let g = signed_stream(&rows);
        let path = tmp.0.join("prefill.bin");
        let valid = stream(&rows);
        for mutation in [false, true] {
            fs::write(&path, &valid).unwrap();
            let verified = VerifiedStream::open(&g, &path).unwrap();
            let changed = if mutation {
                let mut rows = rows.clone();
                rows[1].payload[0] ^= 1;
                stream(&rows)
            } else {
                valid[..valid.len() - 1].to_vec()
            };
            fs::write(&path, changed).unwrap();
            let error = prepare_verified_streaming(
                &g,
                verified,
                &tmp.0.join(format!("bad-{mutation}")),
                Options::default(),
            )
            .err()
            .unwrap();
            if mutation {
                assert_eq!(error, "prefill stream changed during import");
            }
        }
        fs::write(&path, valid).unwrap();
        let verified = VerifiedStream::open(&g, &path).unwrap();
        let mut changed = g.clone();
        changed.run_id[0] ^= 1;
        let mut launch = changed.prefill.take().unwrap();
        launch.recipe_hash = recipe_hash(&changed, &launch);
        changed.prefill = Some(launch);
        let error = prepare_verified_streaming(
            &changed,
            verified,
            &tmp.0.join("identity"),
            Options::default(),
        )
        .err()
        .unwrap();
        assert_eq!(error, "verified prefill genesis mismatch");
    }
    #[test]
    fn retained_link_must_name_the_audited_inode() {
        let tmp = Temp::new();
        let rows = [row(0)];
        let g = signed_stream(&rows);
        let temporary = tmp.0.join("pending");
        let target = tmp.0.join("prefill.bin");
        let original = tmp.0.join("original");
        fs::write(&temporary, stream(&rows)).unwrap();
        let verified = VerifiedStream::open(&g, &temporary).unwrap();
        fs::rename(&temporary, &original).unwrap();
        fs::write(&temporary, b"replacement").unwrap();
        fs::hard_link(&temporary, &target).unwrap();
        assert!(verified.check_path_identity(&target).is_err());
        fs::remove_file(&target).unwrap();
        fs::hard_link(&original, &target).unwrap();
        verified.check_path_identity(&target).unwrap();
    }
    #[test]
    fn verified_finalized_restart_still_checks_prefix_and_derived_state() {
        let tmp = Temp::new();
        let rows = [row(0), row(1)];
        let g = signed_stream(&rows);
        let path = tmp.0.join("prefill.bin");
        let work = tmp.0.join("work");
        fs::write(&path, stream(&rows)).unwrap();
        let engine = prepare_streaming(&g, &path, &work, Options::default()).unwrap();
        let state = engine.head_snapshot().state_header();
        drop(engine);
        let verified = VerifiedStream::open(&g, &path).unwrap();
        let mut changed_rows = rows.clone();
        changed_rows[0].payload[0] ^= 1;
        fs::write(&path, stream(&changed_rows)).unwrap();
        // The post-install audit must also reject an in-place same-size change
        // after bulk construction, while a retained path is used by the serving layer.
        assert!(verify_stream_file(&g, &path).is_err());
        let error = prepare_verified_streaming(&g, verified, &work, Options::default())
            .err()
            .unwrap();
        assert_eq!(error, "prefill stream changed during import");
        fs::write(&path, stream(&rows)).unwrap();
        let resumed = prepare_streaming(&g, &path, &work, Options::default()).unwrap();
        assert_eq!(resumed.head_snapshot().state_header(), state);
        drop(resumed);
        let mut changed = g.clone();
        changed.prefill.as_mut().unwrap().burned_wei += U256::from(1);
        let error = prepare_streaming(&changed, &path, &work, Options::default())
            .err()
            .unwrap();
        assert_eq!(error, "prefill derived state/rent mismatch");
    }
    #[test]
    fn bounded_reader_rejects_bad_order_frames_truncation_and_trailing() {
        let valid = stream(&[row(0), row(1)]);
        let mut r = StreamReader::new(valid.as_slice(), limits()).unwrap();
        while r.next_row().unwrap().is_some() {}
        let summary = r.finish().unwrap();
        assert_eq!(summary.hash, digest(STREAM_DOMAIN, &valid));
        for bad in [
            stream(&[row(1)]),
            stream(&[row(0), row(0)]),
            valid[..valid.len() - 1].to_vec(),
            {
                let mut b = valid.clone();
                b.push(0);
                b
            },
            {
                let mut b = stream(&[row(0)]);
                b[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
                b
            },
        ] {
            let result = (|| {
                let mut r = StreamReader::new(bad.as_slice(), limits())?;
                while r.next_row()?.is_some() {}
                r.finish()
            })();
            assert!(result.is_err());
        }
    }
    #[test]
    fn v2_attributes_do_not_weaken_v1() {
        let mut entity = row(0);
        entity.attributes = (0..29)
            .map(|i| {
                (
                    CellName::new(&format!("attr{i:02}")).unwrap(),
                    Value::str("x"),
                )
            })
            .collect();
        assert!(crate::genesis_bundle::encode_stream(&[entity.clone()]).is_err());
        let bytes = stream(&[entity]);
        let mut r = StreamReader::new(bytes.as_slice(), limits()).unwrap();
        assert_eq!(r.next_row().unwrap().unwrap().attributes.len(), 29);
        r.finish().unwrap();
    }
    #[test]
    fn bulk_matches_v1_oracle_across_progress_settings_and_reopen() {
        let tmp = Temp::new();
        let rows: Vec<_> = (0..20).map(row).collect();
        let path = tmp.0.join("prefill.bin");
        fs::write(&path, stream(&rows)).unwrap();
        let summary = audit_stream(&path, limits()).unwrap();
        let oracle = crate::genesis_bundle::build_genesis(
            base(),
            PrefillLimits {
                max_entities: 256,
                max_canonical_bytes: crate::genesis_bundle::MAX_CANONICAL_BYTES,
                max_storage_units: crate::genesis_bundle::MAX_STORAGE_UNITS,
            },
            [2; 20],
            &rows,
        )
        .unwrap();
        let expected = oracle.prefill.unwrap();
        for batch in [1, 512, 65536] {
            let work = tmp.0.join(format!("batch{batch}"));
            for _ in 0..2 {
                let (engine, burn, units) = bulk_build(
                    &base(),
                    limits(),
                    [2; 20],
                    &path,
                    &work,
                    Options::default(),
                    batch,
                    &summary,
                )
                .unwrap();
                assert_eq!(engine.head_snapshot().state_header(), expected.state);
                assert_eq!(burn, expected.burned_wei);
                assert_eq!(units, expected.storage_units);
            }
        }
    }
    #[test]
    fn bounded_object_flushes_preserve_state() {
        let tmp = Temp::new();
        let rows: Vec<_> = (0..20).map(row).collect();
        let path = tmp.0.join("prefill.bin");
        fs::write(&path, stream(&rows)).unwrap();
        let summary = audit_stream(&path, limits()).unwrap();
        let options = Options {
            prepare_objects: 100,
            ..Options::default()
        };
        let (limited, burn, units) = bulk_build(
            &base(),
            limits(),
            [2; 20],
            &path,
            &tmp.0.join("limited"),
            options,
            65536,
            &summary,
        )
        .unwrap();
        assert_eq!(limited.head().sequence(), 0);
        let (normal, normal_burn, normal_units) = bulk_build(
            &base(),
            limits(),
            [2; 20],
            &path,
            &tmp.0.join("normal"),
            Options::default(),
            65536,
            &summary,
        )
        .unwrap();
        assert_eq!(
            limited.head_snapshot().state_header(),
            normal.head_snapshot().state_header()
        );
        assert_eq!((burn, units), (normal_burn, normal_units));
    }
    #[test]
    fn rebuilds_incomplete_scratch_after_later_frame_failure() {
        let tmp = Temp::new();
        let rows: Vec<_> = (0..20).map(row).collect();
        let path = tmp.0.join("prefill.bin");
        let valid = stream(&rows);
        fs::write(&path, &valid).unwrap();
        let summary = audit_stream(&path, limits()).unwrap();
        // Simulate a truncated input after work has begun. Recovery discards
        // incomplete private scratch and cannot repeat applied rent or nonces.
        fs::write(&path, &valid[..valid.len() - 10]).unwrap();
        assert!(
            bulk_build(
                &base(),
                limits(),
                [2; 20],
                &path,
                &tmp.0.join("resume"),
                Options::default(),
                1,
                &summary
            )
            .is_err()
        );
        fs::write(&path, &valid).unwrap();
        let (resumed, burn, units) = bulk_build(
            &base(),
            limits(),
            [2; 20],
            &path,
            &tmp.0.join("resume"),
            Options::default(),
            512,
            &summary,
        )
        .unwrap();
        let (clean, clean_burn, clean_units) = bulk_build(
            &base(),
            limits(),
            [2; 20],
            &path,
            &tmp.0.join("clean"),
            Options::default(),
            65536,
            &summary,
        )
        .unwrap();
        assert_eq!(
            resumed.head_snapshot().state_header(),
            clean.head_snapshot().state_header()
        );
        assert_eq!((burn, units), (clean_burn, clean_units));
    }
    #[test]
    fn more_than_v1_rows_and_bulk_identity() {
        let tmp = Temp::new();
        let rows: Vec<_> = (0..300).map(row).collect();
        let path = tmp.0.join("prefill.bin");
        fs::write(&path, stream(&rows)).unwrap();
        let summary = audit_stream(&path, limits()).unwrap();
        let (engine, _, _) = bulk_build(
            &base(),
            limits(),
            [2; 20],
            &path,
            &tmp.0.join("work"),
            Options::default(),
            4096,
            &summary,
        )
        .unwrap();
        assert_eq!(engine.head_snapshot().state_header().catalog_len, 1);
        drop(engine);
        let mut changed = base();
        changed.run_id[0] += 1;
        assert!(
            bulk_build(
                &changed,
                limits(),
                [2; 20],
                &path,
                &tmp.0.join("work"),
                Options::default(),
                4096,
                &summary
            )
            .is_err()
        );
    }
    #[test]
    fn bulk_all_types_creators_counters_and_empty_match_oracle() {
        use arkiv_db::{Budget, Projection, RecordKey};
        let tmp = Temp::new();
        let values = [
            Value::Bool(true),
            Value::I32(-17),
            Value::U64(u64::MAX),
            Value::U256([0xff; 32]),
            Value::Dec([0x80; 32]),
            Value::Bytes32([8; 32]),
            Value::str("héllo"),
            Value::Addr([0; 20]),
            Value::Key([3; 32]),
        ];
        let mut rows = Vec::new();
        for creator in [[2; 20], [3; 20]] {
            for n in 0..6 {
                let mut r = row(n);
                r.creator = creator;
                r.flags = (n % 4) as u8;
                r.expires_at_height = 100 + n % 2;
                r.attributes = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (CellName::new(&format!("attr{i}")).unwrap(), v.clone()))
                    .collect();
                rows.push(r);
            }
        }
        for (label, rows) in [("rich", rows), ("empty", Vec::new())] {
            let path = tmp.0.join(format!("{label}.bin"));
            fs::write(&path, stream(&rows)).unwrap();
            let g = signed_stream(&rows);
            let engine =
                prepare_streaming(&g, &path, &tmp.0.join(label), Options::default()).unwrap();
            assert_eq!(
                engine.head_snapshot().state_header(),
                g.prefill.as_ref().unwrap().state
            );
            for (i, r) in rows.iter().enumerate() {
                let key = crate::arkiv::derive_entity_key(
                    g.genesis.chain_id,
                    &r.creator,
                    r.create_nonce,
                    r.salt,
                );
                let record = engine
                    .head_snapshot()
                    .try_get_record(
                        crate::arkiv::entity_namespace(),
                        &RecordKey::new(&key).unwrap(),
                        &Projection::ALL,
                        Budget::unlimited(),
                    )
                    .unwrap()
                    .unwrap()
                    .value
                    .unwrap();
                assert_eq!(record.id.ordinal(), i as u64 + 1);
                let projected = crate::arkiv::project(&record).unwrap();
                assert_eq!(projected.attributes, r.attributes);
                assert_eq!(projected.payload, r.payload);
            }
            for creator in [[2; 20], [3; 20]] {
                assert_eq!(
                    engine
                        .head_snapshot()
                        .try_host_get(&crate::arkiv::nonce_key(&creator))
                        .unwrap(),
                    (!rows.is_empty()).then(|| 6u64.to_be_bytes().to_vec())
                );
            }
            for height in [100, 101] {
                for key in [
                    crate::exec::total_key(height),
                    crate::exec::due_key(height, crate::arkiv::entity_namespace()),
                ] {
                    assert_eq!(
                        engine.head_snapshot().try_host_get(&key).unwrap(),
                        (!rows.is_empty()).then(|| 6u64.to_be_bytes().to_vec())
                    );
                }
            }
        }
    }
    #[test]
    fn bulk_rejects_protocol_engine_capacity_and_funding_violations() {
        let tmp = Temp::new();
        let mut cases = Vec::new();
        for (name, value) in [
            ("bad:name", Value::U64(1)),
            ("and", Value::U64(1)),
            ("okay", Value::I64(1)),
            ("okay", Value::bytes(b"not an attribute")),
            ("okay", Value::str("control\n")),
        ] {
            let mut r = row(0);
            r.attributes = vec![(CellName::new(name).unwrap(), value)];
            cases.push((base(), vec![r]));
        }
        let mut r = row(0);
        r.content_type = "a".repeat(129);
        cases.push((base(), vec![r]));
        let mut r = row(0);
        r.attributes = (0..31)
            .map(|i| {
                (
                    CellName::new(&format!("attr{i:02}")).unwrap(),
                    Value::U64(1),
                )
            })
            .collect();
        let mut g = base();
        g.genesis.limits.max_cells = 64;
        cases.push((g, vec![r]));
        let mut g = base();
        g.genesis.limits.max_cells = 8;
        cases.push((g, vec![row(0)]));
        let mut g = base();
        g.genesis.params.max_lifetime = 99;
        cases.push((g, vec![row(0)]));
        let mut g = base();
        g.genesis.params.expiry_per_block = 1;
        cases.push((g, vec![row(0), row(1)]));
        let mut g = base();
        g.accounts = accounts::AccountConfig::new(Vec::new()).unwrap();
        cases.push((g, vec![row(0)]));
        let mut g = base();
        g.genesis.params.max_lifetime = u64::MAX;
        let mut r = row(0);
        r.expires_at_height = u64::MAX - 1;
        r.payload = vec![5; 128 * 1024];
        cases.push((g, vec![r]));
        for (i, (g, rows)) in cases.into_iter().enumerate() {
            let path = tmp.0.join(format!("{i}.bin"));
            fs::write(&path, stream(&rows)).unwrap();
            let summary = audit_stream(&path, limits()).unwrap();
            let work = tmp.0.join(format!("work{i}"));
            assert!(
                bulk_build(
                    &g,
                    limits(),
                    [2; 20],
                    &path,
                    &work,
                    Options::default(),
                    1024,
                    &summary
                )
                .is_err(),
                "case{i}"
            );
            assert!(
                !work.join("bulk-v1/complete").exists(),
                "case{i} published complete"
            );
        }
        let path = tmp.0.join("storage.bin");
        fs::write(&path, stream(&[row(0)])).unwrap();
        let summary = audit_stream(&path, limits()).unwrap();
        let mut cap = limits();
        cap.max_storage_units = 0;
        assert!(
            bulk_build(
                &base(),
                cap,
                [2; 20],
                &path,
                &tmp.0.join("storage"),
                Options::default(),
                1024,
                &summary
            )
            .is_err()
        );
    }
    #[test]
    fn bulk_completed_receipt_corruption_is_rejected() {
        let tmp = Temp::new();
        let rows = [row(0)];
        let g = signed_stream(&rows);
        let path = tmp.0.join("input.bin");
        fs::write(&path, stream(&rows)).unwrap();
        let work = tmp.0.join("work");
        drop(prepare_streaming(&g, &path, &work, Options::default()).unwrap());
        let receipt = work.join("bulk-v1/complete/complete.json");
        let original = fs::read(&receipt).unwrap();
        let mut corrupt: serde_json::Value = serde_json::from_slice(&original).unwrap();
        corrupt["units"] = serde_json::json!(corrupt["units"].as_u64().unwrap() + 1);
        fs::write(&receipt, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        assert!(
            prepare_streaming(&g, &path, &work, Options::default())
                .unwrap_err()
                .contains("identity or bounds")
        );
        fs::write(&receipt, original).unwrap();
        drop(prepare_streaming(&g, &path, &work, Options::default()).unwrap());
    }
    #[test]
    fn bulk_lock_protects_scratch_and_legacy_checkpoint_is_never_selected() {
        let tmp = Temp::new();
        let rows = [row(0)];
        let g = signed_stream(&rows);
        let path = tmp.0.join("input.bin");
        fs::write(&path, stream(&rows)).unwrap();
        let work = tmp.0.join("work");
        fs::create_dir_all(work.join("bulk-v1")).unwrap();
        fs::write(work.join("CURRENT"), b"0\n").unwrap();
        fs::write(
            work.join("import-0000000000000000.redb"),
            b"legacy unrelated checkpoint",
        )
        .unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(work.join("bulk-v1/LOCK"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(prepare_streaming(&g, &path, &work, Options::default()).is_err());
        assert!(!work.join("bulk-v1/IDENTITY").exists());
        drop(lock);
        drop(prepare_streaming(&g, &path, &work, Options::default()).unwrap());
        assert_eq!(
            fs::read(work.join("import-0000000000000000.redb")).unwrap(),
            b"legacy unrelated checkpoint"
        );
    }
}
