use {
    super::{
        consumer::Consumer,
        immutable_deserialized_packet::ImmutableDeserializedPacket,
        latest_unprocessed_votes::{
            LatestUnprocessedVotes, LatestValidatorVotePacket, VoteBatchInsertionMetrics,
            VoteSource,
        },
        leader_slot_metrics::LeaderSlotMetricsTracker,
        BankingStageStats,
    },
    arrayvec::ArrayVec,
    solana_accounts_db::account_locks::validate_account_locks,
    solana_measure::measure_us,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk::transaction::SanitizedTransaction,
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    std::sync::{atomic::Ordering, Arc},
};

// Step-size set to be 64, equal to the maximum batch/entry size.
pub const UNPROCESSED_BUFFER_STEP_SIZE: usize = 64;
/// Maximum number of votes a single receive call will accept
const MAX_NUM_VOTES_RECEIVE: usize = 10_000;

#[derive(Debug)]
pub struct VoteStorage {
    latest_unprocessed_votes: Arc<LatestUnprocessedVotes>,
    vote_source: VoteSource,
}

fn consume_scan_should_process_packet(
    bank: &Bank,
    banking_stage_stats: &BankingStageStats,
    packet: &ImmutableDeserializedPacket,
    reached_end_of_slot: bool,
    error_counters: &mut TransactionErrorMetrics,
    sanitized_transactions: &mut Vec<RuntimeTransaction<SanitizedTransaction>>,
    slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
) -> bool {
    // If end of the slot, return should process (quick loop after reached end of slot)
    if reached_end_of_slot {
        return true;
    }

    // Try to sanitize the packet. Ignore deactivation slot since we are
    // immediately attempting to process the transaction.
    let (maybe_sanitized_transaction, sanitization_time_us) = measure_us!(packet
        .build_sanitized_transaction(
            bank.vote_only_bank(),
            bank,
            bank.get_reserved_account_keys(),
        )
        .map(|(tx, _deactivation_slot)| tx));

    slot_metrics_tracker.increment_transactions_from_packets_us(sanitization_time_us);
    banking_stage_stats
        .packet_conversion_elapsed
        .fetch_add(sanitization_time_us, Ordering::Relaxed);

    if let Some(sanitized_transaction) = maybe_sanitized_transaction {
        let message = sanitized_transaction.message();

        // Check the number of locks and whether there are duplicates
        if validate_account_locks(
            message.account_keys(),
            bank.get_transaction_account_lock_limit(),
        )
        .is_err()
        {
            return false;
        }

        if Consumer::check_fee_payer_unlocked(bank, &sanitized_transaction, error_counters).is_err()
        {
            return false;
        }
        sanitized_transactions.push(sanitized_transaction);
        true
    } else {
        false
    }
}

impl VoteStorage {
    pub fn new(
        latest_unprocessed_votes: Arc<LatestUnprocessedVotes>,
        vote_source: VoteSource,
    ) -> Self {
        Self {
            latest_unprocessed_votes,
            vote_source,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.latest_unprocessed_votes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.latest_unprocessed_votes.len()
    }

    pub fn max_receive_size(&self) -> usize {
        MAX_NUM_VOTES_RECEIVE
    }

    pub(crate) fn insert_batch(
        &mut self,
        deserialized_packets: Vec<ImmutableDeserializedPacket>,
    ) -> VoteBatchInsertionMetrics {
        self.latest_unprocessed_votes.insert_batch(
            deserialized_packets
                .into_iter()
                .filter_map(|deserialized_packet| {
                    LatestValidatorVotePacket::new_from_immutable(
                        Arc::new(deserialized_packet),
                        self.vote_source,
                        self.latest_unprocessed_votes
                            .should_deprecate_legacy_vote_ixs(),
                    )
                    .ok()
                }),
            false, // should_replenish_taken_votes
        )
    }

    // returns `true` if the end of slot is reached
    pub fn process_packets<F>(
        &mut self,
        bank: Arc<Bank>,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        mut processing_function: F,
    ) -> bool
    where
        F: FnMut(
            usize,
            &mut bool,
            &mut Vec<RuntimeTransaction<SanitizedTransaction>>,
            &mut LeaderSlotMetricsTracker,
        ) -> Option<Vec<usize>>,
    {
        if matches!(self.vote_source, VoteSource::Gossip) {
            panic!("Gossip vote thread should not be processing transactions");
        }

        // Based on the stake distribution present in the supplied bank, drain the unprocessed votes
        // from each validator using a weighted random ordering. Votes from validators with
        // 0 stake are ignored.
        let all_vote_packets = self
            .latest_unprocessed_votes
            .drain_unprocessed(bank.clone());

        let deprecate_legacy_vote_ixs = self
            .latest_unprocessed_votes
            .should_deprecate_legacy_vote_ixs();

        let mut reached_end_of_slot = false;

        let mut sanitized_transactions = Vec::with_capacity(UNPROCESSED_BUFFER_STEP_SIZE);

        let mut error_counters: TransactionErrorMetrics = TransactionErrorMetrics::default();

        let mut vote_packets =
            ArrayVec::<Arc<ImmutableDeserializedPacket>, UNPROCESSED_BUFFER_STEP_SIZE>::new();
        for chunk in all_vote_packets.chunks(UNPROCESSED_BUFFER_STEP_SIZE) {
            vote_packets.clear();
            chunk.iter().for_each(|packet| {
                if consume_scan_should_process_packet(
                    &bank,
                    banking_stage_stats,
                    packet,
                    reached_end_of_slot,
                    &mut error_counters,
                    &mut sanitized_transactions,
                    slot_metrics_tracker,
                ) {
                    vote_packets.push(packet.clone());
                }
            });

            if let Some(retryable_vote_indices) = processing_function(
                vote_packets.len(),
                &mut reached_end_of_slot,
                &mut sanitized_transactions,
                slot_metrics_tracker,
            ) {
                self.latest_unprocessed_votes.insert_batch(
                    retryable_vote_indices.iter().filter_map(|i| {
                        LatestValidatorVotePacket::new_from_immutable(
                            vote_packets[*i].clone(),
                            self.vote_source,
                            deprecate_legacy_vote_ixs,
                        )
                        .ok()
                    }),
                    true, // should_replenish_taken_votes
                );
            } else {
                self.latest_unprocessed_votes.insert_batch(
                    vote_packets.drain(..).filter_map(|packet| {
                        LatestValidatorVotePacket::new_from_immutable(
                            packet,
                            self.vote_source,
                            deprecate_legacy_vote_ixs,
                        )
                        .ok()
                    }),
                    true, // should_replenish_taken_votes
                );
            }
        }

        reached_end_of_slot
    }

    pub fn clear(&mut self) {
        self.latest_unprocessed_votes.clear();
    }

    pub fn cache_epoch_boundary_info(&mut self, bank: &Bank) {
        if matches!(self.vote_source, VoteSource::Gossip) {
            panic!("Gossip vote thread should not be checking epoch boundary");
        }
        self.latest_unprocessed_votes
            .cache_epoch_boundary_info(bank);
    }

    pub fn should_not_process(&self) -> bool {
        // The gossip vote thread does not need to process or forward any votes, that is
        // handled by the tpu vote thread
        matches!(self.vote_source, VoteSource::Gossip)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_perf::packet::{Packet, PacketFlags},
        solana_runtime::genesis_utils,
        solana_sdk::{
            hash::Hash,
            signature::{Keypair, Signer},
        },
        solana_vote::vote_transaction::new_tower_sync_transaction,
        solana_vote_program::vote_state::TowerSync,
        std::error::Error,
    };

    #[test]
    fn test_process_packets_retryable_indexes_reinserted() -> Result<(), Box<dyn Error>> {
        let node_keypair = Keypair::new();
        let genesis_config =
            genesis_utils::create_genesis_config_with_leader(100, &node_keypair.pubkey(), 200)
                .genesis_config;
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let vote_keypair = Keypair::new();
        let mut vote = Packet::from_data(
            None,
            new_tower_sync_transaction(
                TowerSync::default(),
                Hash::new_unique(),
                &node_keypair,
                &vote_keypair,
                &vote_keypair,
                None,
            ),
        )?;
        vote.meta_mut().flags.set(PacketFlags::SIMPLE_VOTE_TX, true);

        let latest_unprocessed_votes =
            LatestUnprocessedVotes::new_for_tests(&[vote_keypair.pubkey()]);
        let mut transaction_storage =
            VoteStorage::new(Arc::new(latest_unprocessed_votes), VoteSource::Tpu);

        transaction_storage.insert_batch(vec![ImmutableDeserializedPacket::new(&vote)?]);
        assert_eq!(1, transaction_storage.len());

        // When processing packets, return all packets as retryable so that they
        // are reinserted into storage
        let _ = transaction_storage.process_packets(
            bank.clone(),
            &BankingStageStats::default(),
            &mut LeaderSlotMetricsTracker::new(0),
            |packets_to_process_len,
             _reached_end_of_slot,
             _sanitized_transactions,
             _slot_metrics_tracker| {
                // Return all packets indexes as retryable
                Some((0..packets_to_process_len).collect())
            },
        );

        // All packets should remain in the transaction storage
        assert_eq!(1, transaction_storage.len());
        Ok(())
    }
}
