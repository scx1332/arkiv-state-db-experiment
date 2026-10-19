//! Version-two file-backed prefill. Private engine sequences are import batches,
//! never chain heights. The canonical state is independent of batch boundaries.
use crate::{
    accounts, arkiv,
    genesis_bundle::{PrefillEntity, PrefillLaunch, PrefillLimits, recipe_hash},
    signed::protocol::SignedGenesis,
};
use alloy_primitives::{Keccak256, U256};
use arkiv_db::{
    B256, Budget, CellName, Decoder, NamespaceName, OwnerId, Projection, RecordKey, Value, WriteOp,
    decode_exact, digest,
    durable::{DurableEngine, EngineGenesis, Options, Publication},
    encode,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    path::Path,
};

pub use crate::genesis_streaming_compaction::check_disk;

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
pub const DEFAULT_BATCH_BYTES: usize = 64 * 1024;

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
    let mut reader = StreamReader::new(BufReader::new(File::open(path).map_err(err)?), limits)?;
    while reader.next_row()?.is_some() {}
    reader.finish()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u32,
    identity: String,
    rows: u64,
    bytes: u64,
    prefix: String,
    units: u64,
    burn: String,
    finalized: bool,
}
fn checkpoint_bytes(cp: &Checkpoint) -> Result<Vec<u8>> {
    serde_json::to_vec(cp).map_err(err)
}
fn publish(engine: &mut DurableEngine, branch: arkiv_db::Branch, cp: &Checkpoint) -> Result<()> {
    engine
        .prepare_import(branch.seal(), &checkpoint_bytes(cp)?)
        .map_err(|failure| {
            if matches!(
                &failure.error,
                arkiv_db::durable::DurableError::ResourceLimit(_)
            ) {
                format!("batch preparation limit: {}", failure.error)
            } else {
                err(failure)
            }
        })?;
    match engine.commit().map_err(err)? {
        Publication::Committed(_) => Ok(()),
        other => Err(format!(
            "import commit not acknowledged; reopen to reconcile checkpoint: {other:?}"
        )),
    }
}
fn import_identity(
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
// Preparation limits are local resource policy. Retry from the last atomic
// checkpoint with smaller batches; never change canonical state or stream bytes.
#[allow(clippy::too_many_arguments)]
fn replay(
    base: &SignedGenesis,
    limits: PrefillLimits,
    sponsor: [u8; 20],
    path: &Path,
    work_dir: &Path,
    options: Options,
    mut batch_bytes: usize,
    summary: &StreamSummary,
) -> Result<(DurableEngine, U256, u64)> {
    loop {
        match replay_once(
            base,
            limits,
            sponsor,
            path,
            work_dir,
            options.clone(),
            batch_bytes,
            summary,
        ) {
            Err(error) if batch_bytes > 1 && error.starts_with("batch preparation limit:") => {
                batch_bytes = (batch_bytes / 2).max(1);
            }
            result => return result,
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn replay_once(
    base: &SignedGenesis,
    limits: PrefillLimits,
    sponsor: [u8; 20],
    path: &Path,
    work_dir: &Path,
    options: Options,
    batch_bytes: usize,
    summary: &StreamSummary,
) -> Result<(DurableEngine, U256, u64)> {
    if batch_bytes == 0 || batch_bytes > 4 * 1024 * 1024 {
        return Err("batch bytes must be 1..=4194304".into());
    }
    validate_limits(limits)?;
    base.accounts.validate().map_err(err)?;
    if base.genesis.pricing != crate::gas::schedule() {
        return Err("streaming prefill requires GLM pricing".into());
    }
    check_disk(work_dir, 0)?;
    let identity = import_identity(base, limits, sponsor, summary);
    let mut genesis = EngineGenesis::new(identity.0, base.genesis.pricing.clone());
    genesis.limits = base.genesis.limits;
    let mut engine = crate::genesis_streaming_compaction::open_stage(
        work_dir,
        genesis.clone(),
        options.clone(),
    )?;
    let mut reader = StreamReader::new(BufReader::new(File::open(path).map_err(err)?), limits)?;
    let checkpoint = engine.import_checkpoint(4096).map_err(err)?;
    let mut initialized = checkpoint.is_some();
    let mut cp = match checkpoint {
        Some(bytes) => serde_json::from_slice::<Checkpoint>(&bytes).map_err(err)?,
        None => Checkpoint {
            version: 2,
            identity: identity.to_string(),
            rows: 0,
            bytes: 20,
            prefix: reader.prefix_hash().to_string(),
            units: 0,
            burn: "0".into(),
            finalized: false,
        },
    };
    if cp.version != 2
        || cp.identity != identity.to_string()
        || cp.rows > summary.rows
        || cp.units > limits.max_storage_units
    {
        return Err("import checkpoint identity or bounds mismatch".into());
    }
    // Replay the prefix through the bounded parser/hash, never through the engine.
    // This binds every recovered cursor to exactly the input bytes already applied.
    while reader.consumed < cp.rows {
        reader.next_row()?.ok_or("checkpoint beyond stream")?;
    }
    if reader.bytes != cp.bytes || reader.prefix_hash().to_string() != cp.prefix {
        return Err("import checkpoint prefix mismatch".into());
    }
    let mut burn = U256::from_str_radix(&cp.burn, 10).map_err(err)?;
    while reader.consumed < reader.rows || !initialized {
        check_disk(work_dir, options.prepare_bytes as u64)?;
        let mut branch = engine.begin(engine.head(), None).map_err(err)?;
        if !initialized {
            branch
                .try_execute(
                    OwnerId::new(base.proposer),
                    WriteOp::CreateNamespace {
                        name: NamespaceName::new("entities").map_err(err)?,
                    },
                    Budget::unlimited(),
                )
                .map_err(err)?
                .map_err(err)?;
        }
        let start = reader.bytes;
        while reader.bytes - start < batch_bytes as u64 {
            let Some(row) = reader.next_row()? else { break };
            let key = arkiv::derive_entity_key(
                base.genesis.chain_id,
                &row.creator,
                row.create_nonce,
                row.salt,
            );
            let expires = row.expires_at_height;
            let mut attrs = row.attributes;
            attrs.push((
                CellName::new("$payload").map_err(err)?,
                Value::bytes(&row.payload),
            ));
            attrs.push((
                CellName::new("$contentType").map_err(err)?,
                Value::str(&row.content_type),
            ));
            let context = arkiv::ArkivExecutionContext {
                caller: row.creator,
                block_height: 0,
                chain_id: base.genesis.chain_id,
                max_lifetime: base.genesis.params.max_lifetime,
                expiry_per_block: base.genesis.params.expiry_per_block,
            };
            let op = arkiv::ArkivOperation::Create {
                salt: row.salt,
                flags: row.flags,
                attributes: attrs,
                expiry: crate::Expiry {
                    expires_at: expires,
                    min_lifetime: 0,
                },
            };
            if !matches!(
                arkiv::execute_operation(
                    &mut branch,
                    arkiv::entity_namespace(),
                    &context,
                    &op,
                    Budget::unlimited()
                )
                .map_err(err)?,
                crate::OpOutcome::Applied { .. }
            ) {
                return Err(format!(
                    "prefill entity {} rejected by entity/expiry validation",
                    reader.consumed
                ));
            }
            let stored = branch
                .try_get_record(
                    arkiv::entity_namespace(),
                    &RecordKey::new(&key).ok_or("invalid entity key")?,
                    &Projection::ALL,
                    Budget::unlimited(),
                )
                .map_err(err)?
                .map_err(err)?
                .value
                .ok_or("created prefill entity missing")?;
            let units = crate::gas::storage_units(&stored.cells).map_err(err)?.total;
            cp.units = cp
                .units
                .checked_add(units)
                .ok_or("storage units overflow")?;
            if cp.units > limits.max_storage_units {
                return Err("prefill storage limit".into());
            }
            let rent = crate::gas::rent(0, units, None, Some(expires), 0)
                .map_err(err)?
                .gas;
            burn = burn
                .checked_add(accounts::fee(rent))
                .ok_or("prefill rent overflow")?;
            if burn > base.accounts.initial_balance(OwnerId::new(sponsor)) {
                return Err("prefill sponsor underfunded".into());
            }
        }
        cp.rows = reader.consumed;
        cp.bytes = reader.bytes;
        cp.prefix = reader.prefix_hash().to_string();
        cp.burn = burn.to_string();
        publish(&mut engine, branch, &cp)?;
        initialized = true;
        engine = crate::genesis_streaming_compaction::maybe_compact(
            work_dir,
            engine,
            &genesis,
            options.clone(),
            false,
        )?;
    }
    if reader.finish()? != *summary {
        return Err("prefill stream changed during import".into());
    }
    if !cp.finalized {
        let mut branch = engine.begin(engine.head(), None).map_err(err)?;
        if burn != U256::ZERO {
            let balance = base
                .accounts
                .initial_balance(OwnerId::new(sponsor))
                .checked_sub(burn)
                .ok_or("prefill sponsor underfunded")?;
            accounts::put(
                &mut branch,
                OwnerId::new(sponsor),
                accounts::Account { nonce: 0, balance },
            )
            .map_err(err)?;
            branch
                .try_host_put(accounts::BURN_KEY, &burn.to_be_bytes::<32>())
                .map_err(err)?
                .map_err(err)?;
        }
        cp.finalized = true;
        publish(&mut engine, branch, &cp)?;
    }
    Ok((engine, burn, cp.units))
}

/// Verify bounded canonical frames and the signed stream commitment without state replay.
pub fn verify_stream_file(g: &SignedGenesis, stream: &Path) -> Result<()> {
    g.validate().map_err(err)?;
    let p = g
        .prefill
        .as_ref()
        .filter(|p| p.version == 2)
        .ok_or("expected streaming prefill v2")?;
    let summary = audit_stream(stream, p.limits)?;
    if summary.rows != p.row_count
        || summary.bytes != p.canonical_bytes
        || summary.hash != p.stream_hash
    {
        return Err("prefill commitment mismatch".into());
    }
    Ok(())
}
/// Independently reconstruct an authenticated launch into a private resumable store.
pub fn prepare_streaming(
    g: &SignedGenesis,
    stream: &Path,
    work_dir: &Path,
    options: Options,
) -> Result<DurableEngine> {
    g.validate().map_err(err)?;
    let p = g
        .prefill
        .as_ref()
        .filter(|p| p.version == 2)
        .ok_or("expected streaming prefill v2")?;
    let summary = audit_stream(stream, p.limits)?;
    if summary.rows != p.row_count
        || summary.bytes != p.canonical_bytes
        || summary.hash != p.stream_hash
    {
        return Err("prefill commitment mismatch".into());
    }
    let (engine, burn, units) = replay(
        g,
        p.limits,
        p.sponsor,
        stream,
        work_dir,
        options,
        DEFAULT_BATCH_BYTES,
        &summary,
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
/// resumable import state; only a completely validated output is renamed into place.
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
    let (engine, burn, units) = replay(
        &base,
        limits,
        sponsor,
        stream,
        &work,
        Options::default(),
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
    use crate::signed::protocol::{SignedLimits, default_native_genesis};
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
    fn replay_matches_v1_oracle_across_batches_and_reopen() {
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
                let (engine, burn, units) = replay(
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
    fn preparation_limits_reduce_batches_without_changing_state() {
        let tmp = Temp::new();
        let rows: Vec<_> = (0..20).map(row).collect();
        let path = tmp.0.join("prefill.bin");
        fs::write(&path, stream(&rows)).unwrap();
        let summary = audit_stream(&path, limits()).unwrap();
        let options = Options {
            prepare_objects: 100,
            ..Options::default()
        };
        let (limited, burn, units) = replay(
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
        assert!(limited.head().sequence() > 2);
        let (normal, normal_burn, normal_units) = replay(
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
    fn resumes_committed_prefix_after_later_frame_failure() {
        let tmp = Temp::new();
        let rows: Vec<_> = (0..20).map(row).collect();
        let path = tmp.0.join("prefill.bin");
        let valid = stream(&rows);
        fs::write(&path, &valid).unwrap();
        let summary = audit_stream(&path, limits()).unwrap();
        // Simulate an input truncation after several durable batches. The pinned
        // identity stays fixed, and recovery must not repeat applied rent/nonces.
        fs::write(&path, &valid[..valid.len() - 10]).unwrap();
        assert!(
            replay(
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
        let (resumed, burn, units) = replay(
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
        let (clean, clean_burn, clean_units) = replay(
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
    fn more_than_v1_rows_and_checkpoint_identity() {
        let tmp = Temp::new();
        let rows: Vec<_> = (0..300).map(row).collect();
        let path = tmp.0.join("prefill.bin");
        fs::write(&path, stream(&rows)).unwrap();
        let summary = audit_stream(&path, limits()).unwrap();
        let (engine, _, _) = replay(
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
            replay(
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
}
