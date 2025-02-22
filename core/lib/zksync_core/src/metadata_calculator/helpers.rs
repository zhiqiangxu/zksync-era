//! Various helpers for the metadata calculator.

use serde::Serialize;
#[cfg(test)]
use tokio::sync::mpsc;

use std::{
    collections::BTreeMap,
    future::Future,
    mem,
    path::{Path, PathBuf},
    time::Duration,
};

use zksync_config::configs::database::MerkleTreeMode;
use zksync_dal::StorageProcessor;
use zksync_health_check::{Health, HealthStatus};
use zksync_merkle_tree::{
    domain::{TreeMetadata, ZkSyncTree},
    MerkleTreeColumnFamily,
};
use zksync_storage::RocksDB;
use zksync_types::{block::L1BatchHeader, L1BatchNumber, StorageLog, H256};

use super::metrics::{LoadChangesStage, ReportStage, TreeUpdateStage};

#[derive(Debug, Serialize)]
pub(super) struct TreeHealthCheckDetails {
    pub mode: MerkleTreeMode,
    pub next_l1_batch_to_seal: L1BatchNumber,
}

impl From<TreeHealthCheckDetails> for Health {
    fn from(details: TreeHealthCheckDetails) -> Self {
        Self::from(HealthStatus::Ready).with_details(details)
    }
}

/// Wrapper around the "main" tree implementation used by [`MetadataCalculator`].
///
/// Async methods provided by this wrapper are not cancel-safe! This is probably not an issue;
/// `ZkSyncTree` is only indirectly available via `MetadataCalculator::run()` entrypoint
/// which consumes `self`. That is, if `MetadataCalculator::run()` is cancelled (which we don't currently do,
/// at least not explicitly), all `MetadataCalculator` data including `ZkSyncTree` is discarded.
/// In the unlikely case you get a "`ZkSyncTree` is in inconsistent state" panic,
/// cancellation is most probably the reason.
#[derive(Debug, Default)]
pub(super) struct AsyncTree(Option<ZkSyncTree>);

impl AsyncTree {
    const INCONSISTENT_MSG: &'static str =
        "`ZkSyncTree` is in inconsistent state, which could occur after one of its blocking futures was cancelled";

    pub async fn new(
        db_path: PathBuf,
        mode: MerkleTreeMode,
        multi_get_chunk_size: usize,
        block_cache_capacity: usize,
    ) -> Self {
        tracing::info!(
            "Initializing Merkle tree at `{db_path}` with {multi_get_chunk_size} multi-get chunk size, \
             {block_cache_capacity}B block cache",
            db_path = db_path.display()
        );

        let mut tree = tokio::task::spawn_blocking(move || {
            let db = Self::create_db(&db_path, block_cache_capacity);
            match mode {
                MerkleTreeMode::Full => ZkSyncTree::new(db),
                MerkleTreeMode::Lightweight => ZkSyncTree::new_lightweight(db),
            }
        })
        .await
        .unwrap();

        tree.set_multi_get_chunk_size(multi_get_chunk_size);
        Self(Some(tree))
    }

    fn create_db(path: &Path, block_cache_capacity: usize) -> RocksDB<MerkleTreeColumnFamily> {
        let db = RocksDB::with_cache(path, true, Some(block_cache_capacity));
        if cfg!(test) {
            // We need sync writes for the unit tests to execute reliably. With the default config,
            // some writes to RocksDB may occur, but not be visible to the test code.
            db.with_sync_writes()
        } else {
            db
        }
    }

    fn as_ref(&self) -> &ZkSyncTree {
        self.0.as_ref().expect(Self::INCONSISTENT_MSG)
    }

    fn as_mut(&mut self) -> &mut ZkSyncTree {
        self.0.as_mut().expect(Self::INCONSISTENT_MSG)
    }

    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }

    pub fn next_l1_batch_number(&self) -> L1BatchNumber {
        self.as_ref().next_l1_batch_number()
    }

    pub fn root_hash(&self) -> H256 {
        self.as_ref().root_hash()
    }

    pub async fn process_l1_batch(&mut self, storage_logs: Vec<StorageLog>) -> TreeMetadata {
        let mut tree = mem::take(self);
        let (tree, metadata) = tokio::task::spawn_blocking(move || {
            let metadata = tree.as_mut().process_l1_batch(&storage_logs);
            (tree, metadata)
        })
        .await
        .unwrap();

        *self = tree;
        metadata
    }

    pub async fn save(&mut self) {
        let mut tree = mem::take(self);
        *self = tokio::task::spawn_blocking(|| {
            tree.as_mut().save();
            tree
        })
        .await
        .unwrap();
    }

    pub fn revert_logs(&mut self, last_l1_batch_to_keep: L1BatchNumber) {
        self.as_mut().revert_logs(last_l1_batch_to_keep);
    }
}

/// Component implementing the delay policy in [`MetadataCalculator`] when there are no
/// L1 batches to seal.
#[derive(Debug, Clone)]
pub(super) struct Delayer {
    delay_interval: Duration,
    // Notifies the tests about the next L1 batch number and tree root hash when the calculator
    // runs out of L1 batches to process. (Since RocksDB is exclusive, we cannot just create
    // another instance to check these params on the test side without stopping the calc.)
    #[cfg(test)]
    pub delay_notifier: mpsc::UnboundedSender<(L1BatchNumber, H256)>,
}

impl Delayer {
    pub fn new(delay_interval: Duration) -> Self {
        Self {
            delay_interval,
            #[cfg(test)]
            delay_notifier: mpsc::unbounded_channel().0,
        }
    }

    #[cfg_attr(not(test), allow(unused))] // `tree` is only used in test mode
    pub fn wait(&self, tree: &AsyncTree) -> impl Future<Output = ()> {
        #[cfg(test)]
        self.delay_notifier
            .send((tree.next_l1_batch_number(), tree.root_hash()))
            .ok();
        tokio::time::sleep(self.delay_interval)
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct L1BatchWithLogs {
    pub header: L1BatchHeader,
    pub storage_logs: Vec<StorageLog>,
}

impl L1BatchWithLogs {
    pub async fn new(
        storage: &mut StorageProcessor<'_>,
        l1_batch_number: L1BatchNumber,
    ) -> Option<Self> {
        tracing::debug!("Loading storage logs data for L1 batch #{l1_batch_number}");
        let load_changes_latency = TreeUpdateStage::LoadChanges.start();

        let header_latency = LoadChangesStage::L1BatchHeader.start();
        let header = storage
            .blocks_dal()
            .get_l1_batch_header(l1_batch_number)
            .await
            .unwrap()?;
        header_latency.report();

        let protective_reads_latency = LoadChangesStage::ProtectiveReads.start();
        let protective_reads = storage
            .storage_logs_dedup_dal()
            .get_protective_reads_for_l1_batch(l1_batch_number)
            .await;
        protective_reads_latency.report_with_count(protective_reads.len());

        let touched_slots_latency = LoadChangesStage::TouchedSlots.start();
        let mut touched_slots = storage
            .storage_logs_dal()
            .get_touched_slots_for_l1_batch(l1_batch_number)
            .await;
        touched_slots_latency.report_with_count(touched_slots.len());

        let mut storage_logs = BTreeMap::new();
        for storage_key in protective_reads {
            touched_slots.remove(&storage_key);
            // ^ As per deduplication rules, all keys in `protective_reads` haven't *really* changed
            // in the considered L1 batch. Thus, we can remove them from `touched_slots` in order to simplify
            // their further processing.

            let log = StorageLog::new_read_log(storage_key, H256::zero());
            // ^ The tree doesn't use the read value, so we set it to zero.
            storage_logs.insert(storage_key, log);
        }
        tracing::debug!(
            "Made touched slots disjoint with protective reads; remaining touched slots: {}",
            touched_slots.len()
        );

        // We don't want to update the tree with zero values which were never written to per storage log
        // deduplication rules. If we write such values to the tree, it'd result in bogus tree hashes because
        // new (bogus) leaf indices would be allocated for them. To filter out those values, it's sufficient
        // to check when a `storage_key` was first written per `initial_writes` table. If this never occurred
        // or occurred after the considered `l1_batch_number`, this means that the write must be ignored.
        //
        // Note that this approach doesn't filter out no-op writes of the same value, but this is fine;
        // since no new leaf indices are allocated in the tree for them, such writes are no-op on the tree side as well.
        let hashed_keys_for_zero_values: Vec<_> = touched_slots
            .iter()
            .filter_map(|(key, value)| {
                // Only zero values are worth checking for initial writes; non-zero values are always
                // written per deduplication rules.
                value.is_zero().then(|| key.hashed_key())
            })
            .collect();
        metrics::histogram!(
            "server.metadata_calculator.load_changes.zero_values",
            hashed_keys_for_zero_values.len() as f64
        );

        let latency = LoadChangesStage::InitialWritesForZeroValues.start();
        let l1_batches_for_initial_writes = storage
            .storage_logs_dal()
            .get_l1_batches_for_initial_writes(&hashed_keys_for_zero_values)
            .await;
        latency.report_with_count(hashed_keys_for_zero_values.len());

        for (storage_key, value) in touched_slots {
            let write_matters = if value.is_zero() {
                let initial_write_batch_for_key =
                    l1_batches_for_initial_writes.get(&storage_key.hashed_key());
                initial_write_batch_for_key.map_or(false, |&number| number <= l1_batch_number)
            } else {
                true
            };

            if write_matters {
                storage_logs.insert(storage_key, StorageLog::new_write_log(storage_key, value));
            }
        }

        load_changes_latency.report();
        Some(Self {
            header,
            storage_logs: storage_logs.into_values().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use db_test_macro::db_test;
    use zksync_contracts::BaseSystemContracts;
    use zksync_dal::ConnectionPool;
    use zksync_types::{
        proofs::PrepareBasicCircuitsJob, protocol_version::L1VerifierConfig,
        system_contracts::get_system_smart_contracts, Address, L2ChainId, ProtocolVersionId,
        StorageKey, StorageLogKind,
    };

    use super::*;
    use crate::{
        genesis::{ensure_genesis_state, GenesisParams},
        metadata_calculator::tests::{extend_db_state, gen_storage_logs, reset_db_state},
    };

    impl L1BatchWithLogs {
        /// Old, slower method of loading storage logs. We want to test its equivalence to the new implementation.
        async fn slow(
            storage: &mut StorageProcessor<'_>,
            l1_batch_number: L1BatchNumber,
        ) -> Option<Self> {
            let header = storage
                .blocks_dal()
                .get_l1_batch_header(l1_batch_number)
                .await
                .unwrap()?;
            let protective_reads = storage
                .storage_logs_dedup_dal()
                .get_protective_reads_for_l1_batch(l1_batch_number)
                .await;
            let touched_slots = storage
                .storage_logs_dal()
                .get_touched_slots_for_l1_batch(l1_batch_number)
                .await;

            let mut storage_logs = BTreeMap::new();

            let hashed_keys: Vec<_> = protective_reads
                .iter()
                .chain(touched_slots.keys())
                .map(StorageKey::hashed_key)
                .collect();
            let previous_values = storage
                .storage_logs_dal()
                .get_previous_storage_values(&hashed_keys, l1_batch_number)
                .await;

            for storage_key in protective_reads {
                let previous_value = previous_values[&storage_key.hashed_key()].unwrap_or_default();
                // Sanity check: value must not change for slots that require protective reads.
                if let Some(value) = touched_slots.get(&storage_key) {
                    assert_eq!(
                        previous_value, *value,
                        "Value was changed for slot that requires protective read"
                    );
                }

                storage_logs.insert(
                    storage_key,
                    StorageLog::new_read_log(storage_key, previous_value),
                );
            }

            for (storage_key, value) in touched_slots {
                let previous_value = previous_values[&storage_key.hashed_key()].unwrap_or_default();
                if previous_value != value {
                    storage_logs.insert(storage_key, StorageLog::new_write_log(storage_key, value));
                }
            }

            Some(Self {
                header,
                storage_logs: storage_logs.into_values().collect(),
            })
        }
    }

    fn mock_genesis_params() -> GenesisParams {
        GenesisParams {
            first_validator: Address::repeat_byte(0x01),
            protocol_version: ProtocolVersionId::latest(),
            base_system_contracts: BaseSystemContracts::load_from_disk(),
            system_contracts: get_system_smart_contracts(),
            first_l1_verifier_config: L1VerifierConfig::default(),
            first_verifier_address: Address::zero(),
        }
    }

    #[db_test]
    async fn loaded_logs_equivalence_basics(pool: ConnectionPool) {
        ensure_genesis_state(
            &mut pool.access_storage().await.unwrap(),
            L2ChainId::from(270),
            &mock_genesis_params(),
        )
        .await
        .unwrap();
        reset_db_state(&pool, 5).await;

        let mut storage = pool.access_storage().await.unwrap();
        for l1_batch_number in 0..=5 {
            let l1_batch_number = L1BatchNumber(l1_batch_number);
            let batch_with_logs = L1BatchWithLogs::new(&mut storage, l1_batch_number)
                .await
                .unwrap();
            let slow_batch_with_logs = L1BatchWithLogs::slow(&mut storage, l1_batch_number)
                .await
                .unwrap();
            assert_eq!(batch_with_logs, slow_batch_with_logs);
        }
    }

    #[db_test]
    async fn loaded_logs_equivalence_with_zero_no_op_logs(pool: ConnectionPool) {
        let mut storage = pool.access_storage().await.unwrap();
        ensure_genesis_state(&mut storage, L2ChainId::from(270), &mock_genesis_params())
            .await
            .unwrap();

        let mut logs = gen_storage_logs(100..200, 2);
        for log in &mut logs[0] {
            log.value = H256::zero();
        }
        for log in logs[1].iter_mut().step_by(3) {
            log.value = H256::zero();
        }
        extend_db_state(&mut storage, logs).await;

        let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");
        let mut tree =
            AsyncTree::new(temp_dir.path().to_owned(), MerkleTreeMode::Full, 500, 0).await;
        for number in 0..3 {
            assert_log_equivalence(&mut storage, &mut tree, L1BatchNumber(number)).await;
        }
    }

    async fn assert_log_equivalence(
        storage: &mut StorageProcessor<'_>,
        tree: &mut AsyncTree,
        l1_batch_number: L1BatchNumber,
    ) {
        let l1_batch_with_logs = L1BatchWithLogs::new(storage, l1_batch_number)
            .await
            .unwrap();
        let slow_l1_batch_with_logs = L1BatchWithLogs::slow(storage, l1_batch_number)
            .await
            .unwrap();

        // Sanity check: L1 batch headers must be identical
        assert_eq!(l1_batch_with_logs.header, slow_l1_batch_with_logs.header);

        tree.save().await; // Necessary for `reset()` below to work properly
        let tree_metadata = tree.process_l1_batch(l1_batch_with_logs.storage_logs).await;
        tree.as_mut().reset();
        let slow_tree_metadata = tree
            .process_l1_batch(slow_l1_batch_with_logs.storage_logs)
            .await;
        assert_eq!(tree_metadata.root_hash, slow_tree_metadata.root_hash);
        assert_eq!(
            tree_metadata.rollup_last_leaf_index,
            slow_tree_metadata.rollup_last_leaf_index
        );
        assert_eq!(
            tree_metadata.initial_writes,
            slow_tree_metadata.initial_writes
        );
        assert_eq!(
            tree_metadata.initial_writes,
            slow_tree_metadata.initial_writes
        );
        assert_eq!(
            tree_metadata.repeated_writes,
            slow_tree_metadata.repeated_writes
        );
        assert_equivalent_witnesses(
            tree_metadata.witness.unwrap(),
            slow_tree_metadata.witness.unwrap(),
        );
    }

    fn assert_equivalent_witnesses(lhs: PrepareBasicCircuitsJob, rhs: PrepareBasicCircuitsJob) {
        assert_eq!(lhs.next_enumeration_index(), rhs.next_enumeration_index());
        let lhs_paths = lhs.into_merkle_paths();
        let rhs_paths = rhs.into_merkle_paths();
        assert_eq!(lhs_paths.len(), rhs_paths.len());
        for (lhs_path, rhs_path) in lhs_paths.zip(rhs_paths) {
            assert_eq!(lhs_path, rhs_path);
        }
    }

    #[db_test]
    async fn loaded_logs_equivalence_with_non_zero_no_op_logs(pool: ConnectionPool) {
        let mut storage = pool.access_storage().await.unwrap();
        ensure_genesis_state(&mut storage, L2ChainId::from(270), &mock_genesis_params())
            .await
            .unwrap();

        let mut logs = gen_storage_logs(100..120, 1);
        // Entire batch of no-op logs (writing previous values).
        let copied_logs = logs[0].clone();
        logs.push(copied_logs);

        // Batch of effectively no-op logs (overwriting values, then writing old values back).
        let mut updated_and_then_copied_logs: Vec<_> = logs[0]
            .iter()
            .map(|log| StorageLog {
                value: H256::repeat_byte(0xff),
                ..*log
            })
            .collect();
        updated_and_then_copied_logs.extend_from_slice(&logs[0]);
        logs.push(updated_and_then_copied_logs);

        // Batch where half of logs are copied and the other half is writing zero values (which is
        // not a no-op).
        let mut partially_copied_logs = logs[0].clone();
        for log in partially_copied_logs.iter_mut().step_by(2) {
            log.value = H256::zero();
        }
        logs.push(partially_copied_logs);

        // Batch where 2/3 of logs are copied and the other 1/3 is writing new non-zero values.
        let mut partially_copied_logs = logs[0].clone();
        for log in partially_copied_logs.iter_mut().step_by(3) {
            log.value = H256::repeat_byte(0x11);
        }
        logs.push(partially_copied_logs);
        extend_db_state(&mut storage, logs).await;

        let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");
        let mut tree =
            AsyncTree::new(temp_dir.path().to_owned(), MerkleTreeMode::Full, 500, 0).await;
        for batch_number in 0..5 {
            assert_log_equivalence(&mut storage, &mut tree, L1BatchNumber(batch_number)).await;
        }
    }

    #[db_test]
    async fn loaded_logs_equivalence_with_protective_reads(pool: ConnectionPool) {
        let mut storage = pool.access_storage().await.unwrap();
        ensure_genesis_state(&mut storage, L2ChainId::from(270), &mock_genesis_params())
            .await
            .unwrap();

        let mut logs = gen_storage_logs(100..120, 1);
        let logs_copy = logs[0].clone();
        logs.push(logs_copy);
        let read_logs: Vec<_> = logs[1]
            .iter()
            .step_by(3)
            .map(StorageLog::to_test_log_query)
            .collect();
        extend_db_state(&mut storage, logs).await;
        storage
            .storage_logs_dedup_dal()
            .insert_protective_reads(L1BatchNumber(2), &read_logs)
            .await;

        let l1_batch_with_logs = L1BatchWithLogs::new(&mut storage, L1BatchNumber(2))
            .await
            .unwrap();
        // Check that we have protective reads transformed into read logs
        let read_logs_count = l1_batch_with_logs
            .storage_logs
            .iter()
            .filter(|log| log.kind == StorageLogKind::Read)
            .count();
        assert_eq!(read_logs_count, 7);

        let temp_dir = TempDir::new().expect("failed get temporary directory for RocksDB");
        let mut tree =
            AsyncTree::new(temp_dir.path().to_owned(), MerkleTreeMode::Full, 500, 0).await;
        for batch_number in 0..3 {
            assert_log_equivalence(&mut storage, &mut tree, L1BatchNumber(batch_number)).await;
        }
    }
}
