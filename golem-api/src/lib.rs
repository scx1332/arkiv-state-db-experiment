//! Drop-in `golemdb_api`: golem-db's public API with [`Database`] served by
//! the arkiv engine.
//!
//! Every type, trait and builder is golem-db's own, re-exported from the
//! real `golemdb-api` crate, so a consumer of that crate compiles unchanged.
//! Only [`Database`] is defined here, with golem-db's constructors and
//! accessors. `GOLEMDB_BACKEND` selects the engine behind it at run time:
//!
//! - `arkiv` (default): `DurableEngine` on redb for [`Database::open`] and
//!   [`Database::open_with_options`], the in-memory `Engine` for
//!   [`Database::open_memory`];
//! - `arkiv-memory`: the in-memory `Engine` for every constructor;
//! - `golemdb`: golem-db's own `Database`, unchanged. A suite that passes on
//!   golem-db passes through this crate with this backend, which is how the
//!   kit checks that the crate itself adds nothing.
//!
//! [`ArkivDb`] is the adapter: golem-db's [`Api`] on the engine. The modules
//! say what is exact, what is a stand-in and what is not served; the kit's
//! README lists it.

use std::sync::Arc;

pub use real::*;

mod admission;
mod arkiv;
mod immutable;
mod query;

pub use arkiv::{ArkivDb, ArkivOptions, FILE, engine_genesis};
pub use immutable::IMMUTABLE_FILE;
pub use query::{BOUNDS, Fallback, Plan, QueryStats, Translator};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Golem,
    Arkiv,
    ArkivMemory,
}

fn backend() -> Backend {
    match std::env::var("GOLEMDB_BACKEND").as_deref() {
        Ok("golemdb") => Backend::Golem,
        Ok("arkiv-memory") => Backend::ArkivMemory,
        Ok("arkiv") | Ok("") | Err(_) => Backend::Arkiv,
        Ok(other) => {
            panic!("GOLEMDB_BACKEND must be golemdb, arkiv or arkiv-memory, not {other:?}")
        }
    }
}

fn options() -> ArkivOptions {
    ArkivOptions::from_env()
}

/// golem-db's cloneable database handle, over the selected engine.
#[derive(Clone)]
pub struct Database {
    inner: Arc<dyn Api + Send + Sync>,
    genesis: Genesis,
    info: OpenInfo,
}

impl Database {
    fn from_real(db: real::Database) -> Self {
        Self {
            genesis: db.genesis().clone(),
            info: *db.info(),
            inner: Arc::new(db),
        }
    }

    fn from_arkiv(db: ArkivDb) -> Self {
        Self {
            genesis: db.genesis().clone(),
            info: *db.info(),
            inner: Arc::new(db),
        }
    }

    pub fn open_memory(genesis: &Genesis) -> OpenResult<Self> {
        match backend() {
            Backend::Golem => real::Database::open_memory(genesis).map(Self::from_real),
            _ => ArkivDb::open_memory(genesis, options()).map(Self::from_arkiv),
        }
    }

    /// golem-db's `from_store` takes one of golem-db's own key/value stores,
    /// which the arkiv engine cannot run on. With the `golemdb` backend it is
    /// golem-db's; with an arkiv backend it is refused, so that a caller
    /// never measures golem-db's engine while asking for the other one.
    pub fn from_store<S>(store: S, config: &OpenConfig) -> OpenResult<Self>
    where
        S: golemdb_storage::Store + Clone + Send + Sync + 'static,
    {
        match backend() {
            Backend::Golem => real::Database::from_store(store, config).map(Self::from_real),
            _ => Err(OpenError::InvalidConfig(
                "Database::from_store takes a golem-db store; the arkiv backends have their own \
                 (use open, open_with_options or open_memory)"
                    .into(),
            )),
        }
    }

    pub fn open(path: impl AsRef<std::path::Path>, config: &OpenConfig) -> OpenResult<Self> {
        Self::open_with_options(path, config, MdbxOptions::default())
    }

    /// `options` are MDBX capacity settings; the arkiv backends ignore them.
    pub fn open_with_options(
        path: impl AsRef<std::path::Path>,
        config: &OpenConfig,
        mdbx: MdbxOptions,
    ) -> OpenResult<Self> {
        match backend() {
            Backend::Golem => {
                real::Database::open_with_options(path, config, mdbx).map(Self::from_real)
            }
            Backend::Arkiv => ArkivDb::open(path.as_ref(), config, options()).map(Self::from_arkiv),
            Backend::ArkivMemory => {
                ArkivDb::open_memory(&config.genesis, options()).map(Self::from_arkiv)
            }
        }
    }

    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    pub fn info(&self) -> &OpenInfo {
        &self.info
    }
}

impl Api for Database {
    fn create(&self, branch: BranchId, operation: RecordOp<op::Create>) -> Metered<RecordKey> {
        self.inner.create(branch, operation)
    }
    fn get(&self, target: ReadTarget, operation: RecordOp<op::Get>) -> Metered<Record> {
        self.inner.get(target, operation)
    }
    fn patch(&self, branch: BranchId, operation: RecordOp<op::Patch>) -> Metered<()> {
        self.inner.patch(branch, operation)
    }
    fn delete(&self, branch: BranchId, operation: RecordOp<op::Delete>) -> Metered<()> {
        self.inner.delete(branch, operation)
    }
    fn query(&self, commit_id: Option<CommitId>, operation: QueryOp) -> Metered<QueryPage> {
        self.inner.query(commit_id, operation)
    }
    fn count(&self, commit_id: Option<CommitId>, operation: CountOp) -> Metered<u64> {
        self.inner.count(commit_id, operation)
    }
    fn metering_model(&self, commit_id: Option<CommitId>) -> Result<MeteringModelInfo> {
        self.inner.metering_model(commit_id)
    }
    fn model_weights(
        &self,
        commit_id: Option<CommitId>,
        version: Option<u64>,
    ) -> Result<ModelWeights> {
        self.inner.model_weights(commit_id, version)
    }
    fn head(&self) -> Result<CommitId> {
        self.inner.head()
    }
    fn begin(&self) -> Result<BranchId> {
        self.inner.begin()
    }
    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo> {
        self.inner.branch_info(branch)
    }
    fn checkpoint(&self, branch: BranchId) -> Result<()> {
        self.inner.checkpoint(branch)
    }
    fn rollback(&self, branch: BranchId) -> Result<()> {
        self.inner.rollback(branch)
    }
    fn seal(&self, branch: BranchId) -> Result<SealInfo> {
        self.inner.seal(branch)
    }
    fn commit(&self, branch: BranchId) -> Result<CommitId> {
        self.inner.commit(branch)
    }
    fn discard(&self, branch: BranchId) -> Result<()> {
        self.inner.discard(branch)
    }
    fn immutable_data_schema(&self, segment: &str) -> Result<ImmutableDataSchema> {
        self.inner.immutable_data_schema(segment)
    }
    fn immutable_data_append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> Result<ImmutableDataOrdinal> {
        self.inner.immutable_data_append(branch, segment, key, row)
    }
    fn immutable_data_truncate(
        &self,
        branch: BranchId,
        segment: &str,
        from_ordinal: ImmutableDataOrdinal,
    ) -> Result<()> {
        self.inner
            .immutable_data_truncate(branch, segment, from_ordinal)
    }
    fn immutable_data_get(
        &self,
        segment: &str,
        address: ImmutableDataAddress,
        options: &ImmutableDataReadOptions,
    ) -> Result<ImmutableDataRow> {
        self.inner.immutable_data_get(segment, address, options)
    }
    fn immutable_data_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
        options: &ImmutableDataReadOptions,
    ) -> Result<Vec<ImmutableDataRow>> {
        self.inner.immutable_data_rows_of(segment, commit, options)
    }
}
