//! A durable catalog of proposer-authenticated native headers and Eq proofs.
//! This follower never executes transaction bodies and makes no finality claim.
use super::{
    Error, Result,
    protocol::{SignedGenesis, SignedHeader},
};
use arkiv_db::proof::{EqRequest, VerifiedContinuation, VerifiedEq, verify_eq};
use arkiv_db::{B256, decode_exact, digest, encode};
use arkiv_db_storage::versioned::{CommitBatch, CommitOutcome, Head, Table, VersionedStore};
use std::{collections::BTreeMap, sync::Arc};

const PROFILE: &[u8] = b"ARKSIGNEDLIGHT2";
const PROFILE_KEY: &[u8] = b"signed-light/profile";
const GENESIS_KEY: &[u8] = b"signed-light/genesis";
const CONFLICT_KEY: &[u8] = b"signed-light/conflict";
const CERTIFICATE_LEN: usize = 4 + 32 + 156 + 65;
const GENESIS_CAP: usize = 64 * 1024;
pub const HEADER_BATCH: usize = 64;

pub struct Light {
    store: Arc<dyn VersionedStore>,
    pub genesis: SignedGenesis,
    pub head: crate::Header,
    pub health: &'static str,
    pub observed: u64,
    sequence: Head,
}
impl Light {
    pub fn create(
        store: Arc<dyn VersionedStore>,
        genesis: SignedGenesis,
        certificate: SignedHeader,
    ) -> Result<Self> {
        genesis.spec()?;
        authenticate(&genesis, &certificate)?;
        if certificate.header != genesis.header() {
            return Err(Error("IdentityMismatch"));
        }
        let bytes = encode(&genesis);
        if bytes.len() > GENESIS_CAP {
            return Err(Error("CapacityReached"));
        }
        let head = certificate.header;
        let sequence = catalog_head(genesis.hash(), 0, &head, 0);
        let mut batch = CommitBatch::new(None, sequence);
        batch
            .put_record(Table::Meta, PROFILE_KEY, PROFILE)
            .put_record(Table::Meta, GENESIS_KEY, bytes)
            .put_record(Table::Blocks, 0u64.to_be_bytes(), encode(&certificate))
            .put_record(Table::Inclusions, 0u64.to_be_bytes(), encode(&certificate))
            .put_record(Table::Manifests, 0u64.to_be_bytes(), encode(&(head, 0u64)))
            .put_record(
                Table::BlockIndex,
                head.hash().as_slice(),
                0u64.to_be_bytes(),
            );
        match store.commit(&batch) {
            CommitOutcome::Committed => {}
            CommitOutcome::NotCommitted(_) => return Err(Error("StorageUnavailable")),
            CommitOutcome::Indeterminate(_) => return Err(Error("CommitUncertain")),
        }
        Ok(Self {
            store,
            genesis,
            head,
            health: "following",
            observed: 0,
            sequence,
        })
    }

    pub fn open(store: Arc<dyn VersionedStore>, expected: &SignedGenesis) -> Result<Self> {
        expected.spec()?;
        if store
            .read_record_bounded(Table::Meta, PROFILE_KEY, 32)?
            .as_deref()
            != Some(PROFILE)
        {
            return Err(Error("IdentityMismatch"));
        }
        let bytes = store
            .read_record_bounded(Table::Meta, GENESIS_KEY, GENESIS_CAP)?
            .ok_or(Error("StorageCorrupt"))?;
        let genesis: SignedGenesis = decode_exact(&bytes).map_err(|_| Error("StorageCorrupt"))?;
        if genesis != *expected {
            return Err(Error("IdentityMismatch"));
        }
        if let Some(evidence) =
            store.read_record_bounded(Table::Meta, CONFLICT_KEY, 2 * CERTIFICATE_LEN)?
        {
            let (first, second): (SignedHeader, SignedHeader) =
                decode_exact(&evidence).map_err(|_| Error("StorageCorrupt"))?;
            authenticate(&genesis, &first).map_err(|_| Error("StorageCorrupt"))?;
            authenticate(&genesis, &second).map_err(|_| Error("StorageCorrupt"))?;
            if first.header.height != second.header.height
                || first.header.hash() == second.header.hash()
            {
                return Err(Error("StorageCorrupt"));
            }
            return Err(Error("ChainConflict"));
        }
        let stored = store.head()?.ok_or(Error("StorageCorrupt"))?;
        let manifest = store
            .read_record_bounded(Table::Manifests, &stored.height.to_be_bytes(), 164)?
            .ok_or(Error("StorageCorrupt"))?;
        let (head, observed): (crate::Header, u64) =
            decode_exact(&manifest).map_err(|_| Error("StorageCorrupt"))?;
        if catalog_head(genesis.hash(), stored.height, &head, observed) != stored
            || observed < head.height
        {
            return Err(Error("StorageCorrupt"));
        }
        let mut light = Self {
            head: genesis.header(),
            genesis,
            store,
            health: "following",
            observed,
            sequence: stored,
        };
        let certificate = light.read_header(head.height)?;
        if certificate.header != head {
            return Err(Error("StorageCorrupt"));
        }
        // Bounded continuity validation: the accepted head and its immediate
        // predecessor, plus the genesis certificate. No historical scan.
        if light.read_header(0)?.header != light.genesis.header() {
            return Err(Error("StorageCorrupt"));
        }
        if head.height > 0 {
            let parent = light.read_header(head.height - 1)?;
            crate::validate::check_header(
                &light.genesis.spec()?,
                &parent.header,
                &certificate.header,
            )
            .map_err(|_| Error("StorageCorrupt"))?;
        }
        light.head = certificate.header;
        Ok(light)
    }

    fn available(&self) -> Result<()> {
        match self.health {
            "chain-conflict" => Err(Error("ChainConflict")),
            "storage-fenced" => Err(Error("CommitUncertain")),
            _ => Ok(()),
        }
    }
    fn read_header(&self, height: u64) -> Result<SignedHeader> {
        let bytes = self
            .store
            .read_record_bounded(Table::Blocks, &height.to_be_bytes(), CERTIFICATE_LEN)?
            .ok_or(Error("StorageCorrupt"))?;
        let certificate: SignedHeader =
            decode_exact(&bytes).map_err(|_| Error("StorageCorrupt"))?;
        authenticate(&self.genesis, &certificate).map_err(|_| Error("StorageCorrupt"))?;
        if certificate.header.height != height
            || self
                .read_observation(height)?
                .is_none_or(|prior| prior.header.hash() != certificate.header.hash())
            || self
                .store
                .read_record_bounded(Table::BlockIndex, certificate.header.hash().as_slice(), 8)?
                .as_deref()
                != Some(height.to_be_bytes().as_slice())
        {
            return Err(Error("StorageCorrupt"));
        }
        Ok(certificate)
    }
    fn read_observation(&self, height: u64) -> Result<Option<SignedHeader>> {
        self.store
            .read_record_bounded(Table::Inclusions, &height.to_be_bytes(), CERTIFICATE_LEN)?
            .map(|bytes| {
                let certificate: SignedHeader =
                    decode_exact(&bytes).map_err(|_| Error("StorageCorrupt"))?;
                authenticate(&self.genesis, &certificate).map_err(|_| Error("StorageCorrupt"))?;
                if certificate.header.height != height {
                    return Err(Error("StorageCorrupt"));
                }
                Ok(certificate)
            })
            .transpose()
    }
    pub fn header(&self, height: u64) -> Result<SignedHeader> {
        self.available()?;
        if height > self.head.height {
            return Err(Error("BlockNotSynced"));
        }
        self.read_header(height)
    }

    fn freeze(&mut self, first: &SignedHeader, second: &SignedHeader) -> Result<()> {
        // Defense in depth: no caller can write a conflict marker from an
        // unauthenticated header, including one found in persistent storage.
        authenticate(&self.genesis, first)?;
        authenticate(&self.genesis, second)?;
        if first.header.height != second.header.height
            || first.header.hash() == second.header.hash()
        {
            return Err(Error("InvalidRequest"));
        }
        self.health = "chain-conflict";
        self.commit_catalog(
            self.head,
            self.observed.max(first.header.height),
            &[],
            &[],
            Some(encode(&(first.clone(), second.clone()))),
        )?;
        Err(Error("ChainConflict"))
    }

    pub fn append(&mut self, headers: &[SignedHeader], sampled: &SignedHeader) -> Result<()> {
        self.available()?;
        if headers.len() > HEADER_BATCH {
            return Err(Error("InvalidRequest"));
        }
        authenticate(&self.genesis, sampled)?;
        for certificate in headers {
            authenticate(&self.genesis, certificate)?;
        }
        let mut observations: BTreeMap<u64, SignedHeader> = BTreeMap::new();
        let mut observed = self.observed;
        for certificate in headers.iter().chain(std::iter::once(sampled)) {
            let height = certificate.header.height;
            observed = observed.max(height);
            let prior = if let Some(prior) = observations.get(&height) {
                Some(prior.clone())
            } else {
                self.read_observation(height)?
            };
            if let Some(prior) = prior {
                if prior.header.hash() != certificate.header.hash() {
                    return self.freeze(&prior, certificate);
                }
            } else {
                observations.insert(height, certificate.clone());
            }
        }
        let observations: Vec<_> = observations.into_values().collect();
        let result = self.validate_append(headers, sampled);
        let (current, accepted) = match result {
            Ok(value) => value,
            Err(error) => {
                // An authentic certificate remains evidence even when its
                // batch cannot extend our chain. Preserve it without accepting
                // that header as a proof root.
                if !observations.is_empty() {
                    self.commit_catalog(self.head, observed, &[], &observations, None)?;
                }
                return Err(error);
            }
        };
        if !accepted.is_empty() || !observations.is_empty() {
            self.commit_catalog(current, observed, &accepted, &observations, None)?;
        }
        self.health = if self.head.height == sampled.header.height {
            "following"
        } else {
            "stalled"
        };
        Ok(())
    }

    fn validate_append(
        &self,
        headers: &[SignedHeader],
        sampled: &SignedHeader,
    ) -> Result<(crate::Header, Vec<SignedHeader>)> {
        let mut current = self.head;
        let mut accepted: Vec<SignedHeader> = Vec::new();
        let spec = self.genesis.spec()?;
        for certificate in headers {
            let h = &certificate.header;
            if h.height <= self.head.height {
                let known = self.read_header(h.height)?;
                if known.header.hash() != h.hash() {
                    return Err(Error("StorageCorrupt"));
                }
                continue;
            }
            if let Some(known) = accepted
                .iter()
                .find(|known| known.header.height == h.height)
            {
                if known.header.hash() != h.hash() {
                    return Err(Error("StorageCorrupt"));
                }
                continue;
            }
            if current.height.checked_add(1) != Some(h.height) {
                return Err(Error("BlockNotSynced"));
            }
            crate::validate::check_header(&spec, &current, h)
                .map_err(|_| Error("InvalidHeader"))?;
            current = *h;
            accepted.push(certificate.clone());
        }
        if let Some(known) = accepted
            .iter()
            .find(|known| known.header.height == sampled.header.height)
            && known.header.hash() != sampled.header.hash()
        {
            return Err(Error("StorageCorrupt"));
        }
        if current.height > sampled.header.height {
            if accepted.is_empty() {
                return Ok((self.head, accepted));
            }
            return Err(Error("BlockNotSynced"));
        }
        Ok((current, accepted))
    }

    fn commit_catalog(
        &mut self,
        head: crate::Header,
        observed: u64,
        accepted: &[SignedHeader],
        observations: &[SignedHeader],
        conflict: Option<Vec<u8>>,
    ) -> Result<()> {
        let number = self
            .sequence
            .height
            .checked_add(1)
            .ok_or(Error("CapacityReached"))?;
        let next = catalog_head(self.genesis.hash(), number, &head, observed);
        let mut batch = CommitBatch::new(Some(self.sequence), next);
        batch.put_record(
            Table::Manifests,
            number.to_be_bytes(),
            encode(&(head, observed)),
        );
        for certificate in accepted {
            let h = certificate.header;
            batch
                .put_record(Table::Blocks, h.height.to_be_bytes(), encode(certificate))
                .put_record(
                    Table::BlockIndex,
                    h.hash().as_slice(),
                    h.height.to_be_bytes(),
                );
        }
        for certificate in observations {
            batch.put_record(
                Table::Inclusions,
                certificate.header.height.to_be_bytes(),
                encode(certificate),
            );
        }
        if let Some(conflict) = conflict {
            batch.put_record(Table::Meta, CONFLICT_KEY, conflict);
        }
        match self.store.commit(&batch) {
            CommitOutcome::Committed => {
                self.sequence = next;
                self.head = head;
                self.observed = observed;
                Ok(())
            }
            CommitOutcome::NotCommitted(_) => {
                self.health = "storage-fenced";
                Err(Error("StorageUnavailable"))
            }
            CommitOutcome::Indeterminate(_) => {
                self.health = "storage-fenced";
                Err(Error("CommitUncertain"))
            }
        }
    }

    /// Verifies the unchanged native Eq format against an accepted certificate.
    pub fn verify(
        &self,
        request: &EqRequest,
        previous: Option<&VerifiedContinuation>,
        bytes: &[u8],
    ) -> Result<VerifiedEq> {
        let h = self.header(request.snapshot.height)?.header;
        if request.snapshot.genesis_hash != self.genesis.hash()
            || request.snapshot.block_hash != h.hash()
            || request.snapshot.state_root != h.state_root
        {
            return Err(Error("InvalidRequest"));
        }
        let verified = verify_eq(
            request,
            previous,
            bytes,
            crate::simulator::wire::proof_limits(),
            self.genesis.genesis.limits,
        )?;
        if request.query.namespace.ordinal() == 1
            && let VerifiedEq::Page(page) = &verified
        {
            for row in &page.rows {
                crate::arkiv::validate_proven_row(row).map_err(|_| Error("InvalidProof"))?;
                let entity = crate::arkiv::project(row).map_err(|_| Error("InvalidProof"))?;
                if entity.created_at > h.height
                    || entity.updated_at > h.height
                    || (entity.expires_at != u64::MAX && entity.expires_at <= h.height)
                {
                    return Err(Error("InvalidProof"));
                }
            }
        }
        Ok(verified)
    }

    /// Complete range evidence anchored in an accepted producer certificate.
    pub fn verify_range(
        &self,
        request: &arkiv_db::proof::RangeRequest,
        bytes: &[u8],
    ) -> Result<arkiv_db::proof::VerifiedRange> {
        let h = self.header(request.snapshot.height)?.header;
        if request.snapshot.genesis_hash != self.genesis.hash()
            || request.snapshot.block_hash != h.hash()
            || request.snapshot.state_root != h.state_root
        {
            return Err(Error("InvalidRequest"));
        }
        let verified = arkiv_db::proof::verify_range(
            request,
            bytes,
            crate::simulator::wire::range_proof_limits()?,
            self.genesis.genesis.limits,
        )?;
        if request.query.namespace.ordinal() == 1
            && let arkiv_db::proof::VerifiedRange::Complete(result) = &verified
        {
            for row in &result.rows {
                crate::arkiv::validate_proven_row(row).map_err(|_| Error("InvalidProof"))?;
                let entity = crate::arkiv::project(row).map_err(|_| Error("InvalidProof"))?;
                if entity.created_at > h.height
                    || entity.updated_at > h.height
                    || (entity.expires_at != u64::MAX && entity.expires_at <= h.height)
                {
                    return Err(Error("InvalidProof"));
                }
            }
        }
        Ok(verified)
    }
}
fn catalog_head(genesis: B256, sequence: u64, header: &crate::Header, observed: u64) -> Head {
    let mut bytes = genesis.to_vec();
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&encode(&(*header, observed)));
    Head {
        height: sequence,
        hash: digest("arkiv/signed-light-catalog/v1", &bytes).0,
    }
}
fn authenticate(genesis: &SignedGenesis, certificate: &SignedHeader) -> Result<()> {
    certificate.authenticate(genesis)?;
    let h = certificate.header;
    if h.chain_id != genesis.genesis.chain_id || h.timestamp_ms != genesis.timestamp(h.height)? {
        return Err(Error("InvalidHeader"));
    }
    Ok(())
}

/// Verify account and burn-counter membership against an already accepted
/// header. The genesis hash authenticates allocation defaults for absent keys.
pub fn verify_account(
    genesis: &SignedGenesis,
    header: &crate::Header,
    actor: arkiv_db::OwnerId,
    account_proof: &[u8],
    supply_proof: &[u8],
) -> Result<(
    crate::accounts::Account,
    alloy_primitives::U256,
    alloy_primitives::U256,
)> {
    let config = &genesis.accounts;
    let root = header.state_root;
    let account = config.resolve(
        actor,
        arkiv_db::proof::verify_host(root, &crate::accounts::account_key(actor), account_proof)?,
    )?;
    let burned = crate::accounts::burned(arkiv_db::proof::verify_host(
        root,
        &crate::accounts::BURN_KEY,
        supply_proof,
    )?)?;
    let initial = config.initial_supply().map_err(|_| Error("InvalidProof"))?;
    if burned > initial || account.balance > initial - burned {
        return Err(Error("InvalidProof"));
    }
    Ok((account, burned, initial))
}
