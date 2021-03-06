//! The `bank` module tracks client accounts and the progress of on-chain
//! programs. It offers a high-level API that signs transactions
//! on behalf of the caller, and a low-level API for when they have
//! already been signed and verified.

use crate::accounts::{Accounts, ErrorCounters, InstructionAccounts, InstructionLoaders};
use crate::blockhash_queue::BlockhashQueue;
use crate::runtime::{ProcessInstruction, Runtime};
use crate::status_cache::StatusCache;
use bincode::serialize;
use hashbrown::HashMap;
use log::*;
use solana_metrics::counter::Counter;
use solana_sdk::account::Account;
use solana_sdk::genesis_block::GenesisBlock;
use solana_sdk::hash::{extend_and_hash, Hash};
use solana_sdk::native_loader;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::system_transaction::SystemTransaction;
use solana_sdk::timing::{duration_as_us, MAX_RECENT_BLOCKHASHES, NUM_TICKS_PER_SECOND};
use solana_sdk::transaction::{Transaction, TransactionError};
use solana_vote_api::vote_instruction::Vote;
use solana_vote_api::vote_state::{Lockout, VoteState};
use std::result;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Reasons a transaction might be rejected.
#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub struct EpochSchedule {
    /// The maximum number of slots in each epoch.
    pub slots_per_epoch: u64,

    /// A number of slots before slot_index 0. Used to calculate finalized staked nodes.
    pub stakers_slot_offset: u64,

    /// basically: log2(slots_per_epoch)
    pub first_normal_epoch: u64,

    /// basically: 2.pow(first_normal_epoch)
    pub first_normal_slot: u64,
}

impl EpochSchedule {
    pub fn new(slots_per_epoch: u64, stakers_slot_offset: u64, warmup: bool) -> Self {
        let (first_normal_epoch, first_normal_slot) = if warmup {
            let next_power_of_two = slots_per_epoch.next_power_of_two();
            let log2_slots_per_epoch = next_power_of_two.trailing_zeros();

            (u64::from(log2_slots_per_epoch), next_power_of_two - 1)
        } else {
            (0, 0)
        };
        EpochSchedule {
            slots_per_epoch,
            stakers_slot_offset,
            first_normal_epoch,
            first_normal_slot,
        }
    }

    /// get the length of the given epoch (in slots)
    pub fn get_slots_in_epoch(&self, epoch: u64) -> u64 {
        if epoch < self.first_normal_epoch {
            2u64.pow(epoch as u32)
        } else {
            self.slots_per_epoch
        }
    }

    /// get the epoch for which the given slot should save off
    ///  information about stakers
    pub fn get_stakers_epoch(&self, slot: u64) -> u64 {
        if slot < self.first_normal_slot {
            // until we get to normal slots, behave as if stakers_slot_offset == slots_per_epoch

            self.get_epoch_and_slot_index(slot).0 + 1
        } else {
            self.first_normal_epoch
                + (slot - self.first_normal_slot + self.stakers_slot_offset) / self.slots_per_epoch
        }
    }

    /// get epoch and offset into the epoch for the given slot
    pub fn get_epoch_and_slot_index(&self, slot: u64) -> (u64, u64) {
        if slot < self.first_normal_slot {
            let epoch = if slot < 2 {
                slot as u32
            } else {
                (slot + 2).next_power_of_two().trailing_zeros() - 1
            };

            let epoch_len = 2u64.pow(epoch);

            (u64::from(epoch), slot - (epoch_len - 1))
        } else {
            (
                self.first_normal_epoch + ((slot - self.first_normal_slot) / self.slots_per_epoch),
                (slot - self.first_normal_slot) % self.slots_per_epoch,
            )
        }
    }
}

pub type Result<T> = result::Result<T, TransactionError>;

type BankStatusCache = StatusCache<TransactionError>;

/// Manager for the state of all accounts and programs after processing its entries.
#[derive(Default)]
pub struct Bank {
    /// where all the Accounts are stored
    accounts: Arc<Accounts>,

    /// Bank accounts fork id
    accounts_id: u64,

    /// A cache of signature statuses
    status_cache: RwLock<BankStatusCache>,

    /// FIFO queue of `recent_blockhash` items
    blockhash_queue: RwLock<BlockhashQueue>,

    /// Previous checkpoint of this bank
    parent: RwLock<Option<Arc<Bank>>>,

    /// Hash of this Bank's state. Only meaningful after freezing.
    hash: RwLock<Hash>,

    /// Hash of this Bank's parent's state
    parent_hash: Hash,

    /// Bank tick height
    tick_height: AtomicUsize, // TODO: Use AtomicU64 if/when available

    /// The number of ticks in each slot.
    ticks_per_slot: u64,

    /// Bank fork (i.e. slot, i.e. block)
    slot: u64,

    /// The pubkey to send transactions fees to.
    collector_id: Pubkey,

    /// initialized from genesis
    epoch_schedule: EpochSchedule,

    /// staked nodes on epoch boundaries, saved off when a bank.slot() is at
    ///   a leader schedule boundary
    epoch_vote_accounts: HashMap<u64, HashMap<Pubkey, Account>>,

    /// A boolean reflecting whether any entries were recorded into the PoH
    /// stream for the slot == self.slot
    is_delta: AtomicBool,

    /// The runtime executation environment
    runtime: Runtime,
}

impl Default for BlockhashQueue {
    fn default() -> Self {
        Self::new(MAX_RECENT_BLOCKHASHES)
    }
}

impl Bank {
    pub fn new(genesis_block: &GenesisBlock) -> Self {
        Self::new_with_paths(&genesis_block, None)
    }

    pub fn new_with_paths(genesis_block: &GenesisBlock, paths: Option<String>) -> Self {
        let mut bank = Self::default();
        bank.accounts = Arc::new(Accounts::new(bank.slot, paths));
        bank.process_genesis_block(genesis_block);

        // genesis needs stakes for all epochs up to the epoch implied by
        //  slot = 0 and genesis configuration
        let vote_accounts: HashMap<_, _> = bank.vote_accounts().collect();
        for i in 0..=bank.get_stakers_epoch(bank.slot) {
            bank.epoch_vote_accounts.insert(i, vote_accounts.clone());
        }

        bank
    }

    /// Create a new bank that points to an immutable checkpoint of another bank.
    pub fn new_from_parent(parent: &Arc<Bank>, collector_id: &Pubkey, slot: u64) -> Self {
        parent.freeze();
        assert_ne!(slot, parent.slot());

        let mut bank = Self::default();
        bank.blockhash_queue = RwLock::new(parent.blockhash_queue.read().unwrap().clone());
        bank.tick_height
            .store(parent.tick_height.load(Ordering::SeqCst), Ordering::SeqCst);
        bank.ticks_per_slot = parent.ticks_per_slot;
        bank.epoch_schedule = parent.epoch_schedule;

        bank.slot = slot;
        bank.parent = RwLock::new(Some(parent.clone()));
        bank.parent_hash = parent.hash();
        bank.collector_id = *collector_id;

        // Accounts needs a unique id
        static BANK_ACCOUNTS_ID: AtomicUsize = AtomicUsize::new(1);
        bank.accounts_id = BANK_ACCOUNTS_ID.fetch_add(1, Ordering::Relaxed) as u64;
        bank.accounts = parent.accounts.clone();
        bank.accounts
            .new_from_parent(bank.accounts_id, parent.accounts_id);

        bank.epoch_vote_accounts = {
            let mut epoch_vote_accounts = parent.epoch_vote_accounts.clone();
            let epoch = bank.get_stakers_epoch(bank.slot);
            // update epoch_vote_states cache
            //  if my parent didn't populate for this epoch, we've
            //  crossed a boundary
            if epoch_vote_accounts.get(&epoch).is_none() {
                epoch_vote_accounts.insert(epoch, bank.vote_accounts().collect());
            }
            epoch_vote_accounts
        };

        bank
    }

    pub fn collector_id(&self) -> Pubkey {
        self.collector_id
    }

    pub fn slot(&self) -> u64 {
        self.slot
    }

    pub fn hash(&self) -> Hash {
        *self.hash.read().unwrap()
    }

    pub fn is_frozen(&self) -> bool {
        *self.hash.read().unwrap() != Hash::default()
    }

    pub fn freeze(&self) {
        let mut hash = self.hash.write().unwrap();

        if *hash == Hash::default() {
            //  freeze is a one-way trip, idempotent
            *hash = self.hash_internal_state();
        }
    }

    /// squash the parent's state up into this Bank,
    ///   this Bank becomes a root
    pub fn squash(&self) {
        self.freeze();

        let parents = self.parents();
        *self.parent.write().unwrap() = None;

        self.accounts.squash(self.accounts_id);

        let parent_caches: Vec<_> = parents
            .iter()
            .map(|b| b.status_cache.read().unwrap())
            .collect();
        self.status_cache.write().unwrap().squash(&parent_caches);
    }

    /// Return the more recent checkpoint of this bank instance.
    pub fn parent(&self) -> Option<Arc<Bank>> {
        self.parent.read().unwrap().clone()
    }

    fn process_genesis_block(&mut self, genesis_block: &GenesisBlock) {
        assert!(genesis_block.mint_id != Pubkey::default());
        assert!(genesis_block.bootstrap_leader_id != Pubkey::default());
        assert!(genesis_block.bootstrap_leader_vote_account_id != Pubkey::default());
        assert!(genesis_block.lamports >= genesis_block.bootstrap_leader_lamports);
        assert!(genesis_block.bootstrap_leader_lamports >= 2);

        // Bootstrap leader collects fees until `new_from_parent` is called.
        self.collector_id = genesis_block.bootstrap_leader_id;

        let mint_lamports = genesis_block.lamports - genesis_block.bootstrap_leader_lamports;
        self.deposit(&genesis_block.mint_id, mint_lamports);

        let bootstrap_leader_lamports = 1;
        let bootstrap_leader_stake =
            genesis_block.bootstrap_leader_lamports - bootstrap_leader_lamports;
        self.deposit(
            &genesis_block.bootstrap_leader_id,
            bootstrap_leader_lamports,
        );

        // Construct a vote account for the bootstrap_leader such that the leader_scheduler
        // will be forced to select it as the leader for height 0
        let mut bootstrap_leader_vote_account = Account {
            lamports: bootstrap_leader_stake,
            data: vec![0; VoteState::max_size() as usize],
            owner: solana_vote_api::id(),
            executable: false,
        };

        let mut vote_state = VoteState::new(&genesis_block.bootstrap_leader_id);
        vote_state.votes.push_back(Lockout::new(&Vote::new(0)));
        vote_state
            .serialize(&mut bootstrap_leader_vote_account.data)
            .unwrap();

        self.accounts.store_slow(
            self.accounts_id,
            &genesis_block.bootstrap_leader_vote_account_id,
            &bootstrap_leader_vote_account,
        );

        self.blockhash_queue
            .write()
            .unwrap()
            .genesis_hash(&genesis_block.hash());

        self.ticks_per_slot = genesis_block.ticks_per_slot;

        self.epoch_schedule = EpochSchedule::new(
            genesis_block.slots_per_epoch,
            genesis_block.stakers_slot_offset,
            genesis_block.epoch_warmup,
        );

        // Add native programs mandatory for the runtime to function
        self.add_native_program("solana_system_program", &solana_sdk::system_program::id());
        self.add_native_program("solana_bpf_loader", &solana_sdk::bpf_loader::id());
        self.add_native_program("solana_vote_program", &solana_vote_api::id());

        // Add additional native programs specified in the genesis block
        for (name, program_id) in &genesis_block.native_programs {
            self.add_native_program(name, program_id);
        }
    }

    pub fn add_native_program(&self, name: &str, program_id: &Pubkey) {
        debug!("Adding native program {} under {:?}", name, program_id);
        let account = native_loader::create_program_account(name);
        self.accounts
            .store_slow(self.accounts_id, program_id, &account);
    }

    /// Return the last block hash registered.
    pub fn last_blockhash(&self) -> Hash {
        self.blockhash_queue.read().unwrap().last_hash()
    }

    /// Forget all signatures. Useful for benchmarking.
    pub fn clear_signatures(&self) {
        self.status_cache.write().unwrap().clear();
    }

    fn update_transaction_statuses(&self, txs: &[Transaction], res: &[Result<()>]) {
        let mut status_cache = self.status_cache.write().unwrap();
        for (i, tx) in txs.iter().enumerate() {
            match &res[i] {
                Ok(_) => {
                    if !tx.signatures.is_empty() {
                        status_cache.add(&tx.signatures[0]);
                    }
                }
                Err(TransactionError::BlockhashNotFound) => (),
                Err(TransactionError::DuplicateSignature) => (),
                Err(TransactionError::AccountNotFound) => (),
                Err(e) => {
                    if !tx.signatures.is_empty() {
                        status_cache.add(&tx.signatures[0]);
                        status_cache.save_failure_status(&tx.signatures[0], e.clone());
                    }
                }
            }
        }
    }

    /// Looks through a list of tick heights and stakes, and finds the latest
    /// tick that has achieved confirmation
    pub fn get_confirmation_timestamp(
        &self,
        mut slots_and_stakes: Vec<(u64, u64)>,
        supermajority_stake: u64,
    ) -> Option<u64> {
        // Sort by slot height
        slots_and_stakes.sort_by(|a, b| b.0.cmp(&a.0));

        let max_slot = self.slot();
        let min_slot = max_slot.saturating_sub(MAX_RECENT_BLOCKHASHES as u64);

        let mut total_stake = 0;
        for (slot, stake) in slots_and_stakes.iter() {
            if *slot >= min_slot && *slot <= max_slot {
                total_stake += stake;
                if total_stake > supermajority_stake {
                    return self
                        .blockhash_queue
                        .read()
                        .unwrap()
                        .hash_height_to_timestamp(*slot);
                }
            }
        }

        None
    }

    /// Tell the bank which Entry IDs exist on the ledger. This function
    /// assumes subsequent calls correspond to later entries, and will boot
    /// the oldest ones once its internal cache is full. Once boot, the
    /// bank will reject transactions using that `hash`.
    pub fn register_tick(&self, hash: &Hash) {
        if self.is_frozen() {
            warn!("=========== FIXME: register_tick() working on a frozen bank! ================");
        }

        // TODO: put this assert back in
        // assert!(!self.is_frozen());

        let current_tick_height = {
            self.tick_height.fetch_add(1, Ordering::SeqCst);
            self.tick_height.load(Ordering::SeqCst) as u64
        };
        inc_new_counter_info!("bank-register_tick-registered", 1);

        // Register a new block hash if at the last tick in the slot
        if current_tick_height % self.ticks_per_slot == self.ticks_per_slot - 1 {
            let mut blockhash_queue = self.blockhash_queue.write().unwrap();
            blockhash_queue.register_hash(hash);
        }

        if current_tick_height % NUM_TICKS_PER_SECOND == 0 {
            self.status_cache.write().unwrap().new_cache(hash);
        }
    }

    /// Process a Transaction. This is used for unit tests and simply calls the vector Bank::process_transactions method.
    pub fn process_transaction(&self, tx: &Transaction) -> Result<()> {
        let txs = vec![tx.clone()];
        self.process_transactions(&txs)[0].clone()?;
        tx.signatures
            .get(0)
            .map_or(Ok(()), |sig| self.get_signature_status(sig).unwrap())
    }

    pub fn lock_accounts(&self, txs: &[Transaction]) -> Vec<Result<()>> {
        if self.is_frozen() {
            warn!("=========== FIXME: lock_accounts() working on a frozen bank! ================");
        }
        // TODO: put this assert back in
        // assert!(!self.is_frozen());
        self.accounts.lock_accounts(self.accounts_id, txs)
    }

    pub fn unlock_accounts(&self, txs: &[Transaction], results: &[Result<()>]) {
        self.accounts
            .unlock_accounts(self.accounts_id, txs, results)
    }

    fn load_accounts(
        &self,
        txs: &[Transaction],
        results: Vec<Result<()>>,
        error_counters: &mut ErrorCounters,
    ) -> Vec<Result<(InstructionAccounts, InstructionLoaders)>> {
        self.accounts
            .load_accounts(self.accounts_id, txs, results, error_counters)
    }
    fn check_age(
        &self,
        txs: &[Transaction],
        lock_results: Vec<Result<()>>,
        max_age: usize,
        error_counters: &mut ErrorCounters,
    ) -> Vec<Result<()>> {
        let hash_queue = self.blockhash_queue.read().unwrap();
        txs.iter()
            .zip(lock_results.into_iter())
            .map(|(tx, lock_res)| {
                if lock_res.is_ok() && !hash_queue.check_hash_age(tx.recent_blockhash, max_age) {
                    error_counters.reserve_blockhash += 1;
                    Err(TransactionError::BlockhashNotFound)
                } else {
                    lock_res
                }
            })
            .collect()
    }
    fn check_signatures(
        &self,
        txs: &[Transaction],
        lock_results: Vec<Result<()>>,
        error_counters: &mut ErrorCounters,
    ) -> Vec<Result<()>> {
        let parents = self.parents();
        let mut caches = vec![self.status_cache.read().unwrap()];
        caches.extend(parents.iter().map(|b| b.status_cache.read().unwrap()));
        txs.iter()
            .zip(lock_results.into_iter())
            .map(|(tx, lock_res)| {
                if tx.signatures.is_empty() {
                    return lock_res;
                }
                if lock_res.is_ok() && StatusCache::has_signature_all(&caches, &tx.signatures[0]) {
                    error_counters.duplicate_signature += 1;
                    Err(TransactionError::DuplicateSignature)
                } else {
                    lock_res
                }
            })
            .collect()
    }
    #[allow(clippy::type_complexity)]
    pub fn load_and_execute_transactions(
        &self,
        txs: &[Transaction],
        lock_results: Vec<Result<()>>,
        max_age: usize,
    ) -> (
        Vec<Result<(InstructionAccounts, InstructionLoaders)>>,
        Vec<Result<()>>,
    ) {
        debug!("processing transactions: {}", txs.len());
        let mut error_counters = ErrorCounters::default();
        let now = Instant::now();
        let age_results = self.check_age(txs, lock_results, max_age, &mut error_counters);
        let sig_results = self.check_signatures(txs, age_results, &mut error_counters);
        let mut loaded_accounts = self.load_accounts(txs, sig_results, &mut error_counters);
        let tick_height = self.tick_height();

        let load_elapsed = now.elapsed();
        let now = Instant::now();
        let executed: Vec<Result<()>> = loaded_accounts
            .iter_mut()
            .zip(txs.iter())
            .map(|(accs, tx)| match accs {
                Err(e) => Err(e.clone()),
                Ok((ref mut accounts, ref mut loaders)) => {
                    self.runtime
                        .execute_transaction(tx, loaders, accounts, tick_height)
                }
            })
            .collect();

        let execution_elapsed = now.elapsed();

        debug!(
            "load: {}us execute: {}us txs_len={}",
            duration_as_us(&load_elapsed),
            duration_as_us(&execution_elapsed),
            txs.len(),
        );
        let mut tx_count = 0;
        let mut err_count = 0;
        for (r, tx) in executed.iter().zip(txs.iter()) {
            if r.is_ok() {
                tx_count += 1;
            } else {
                if err_count == 0 {
                    info!("tx error: {:?} {:?}", r, tx);
                }
                err_count += 1;
            }
        }
        if err_count > 0 {
            info!("{} errors of {} txs", err_count, err_count + tx_count);
            inc_new_counter_info!(
                "bank-process_transactions-account_not_found",
                error_counters.account_not_found
            );
            inc_new_counter_info!("bank-process_transactions-error_count", err_count);
        }

        self.accounts
            .increment_transaction_count(self.accounts_id, tx_count);

        inc_new_counter_info!("bank-process_transactions-txs", tx_count);
        if 0 != error_counters.blockhash_not_found {
            inc_new_counter_info!(
                "bank-process_transactions-error-blockhash_not_found",
                error_counters.blockhash_not_found
            );
        }
        if 0 != error_counters.reserve_blockhash {
            inc_new_counter_info!(
                "bank-process_transactions-error-reserve_blockhash",
                error_counters.reserve_blockhash
            );
        }
        if 0 != error_counters.duplicate_signature {
            inc_new_counter_info!(
                "bank-process_transactions-error-duplicate_signature",
                error_counters.duplicate_signature
            );
        }
        if 0 != error_counters.insufficient_funds {
            inc_new_counter_info!(
                "bank-process_transactions-error-insufficient_funds",
                error_counters.insufficient_funds
            );
        }
        if 0 != error_counters.account_loaded_twice {
            inc_new_counter_info!(
                "bank-process_transactions-account_loaded_twice",
                error_counters.account_loaded_twice
            );
        }
        (loaded_accounts, executed)
    }

    fn filter_program_errors_and_collect_fee(
        &self,
        txs: &[Transaction],
        executed: &[Result<()>],
    ) -> Vec<Result<()>> {
        let mut fees = 0;
        let results = txs
            .iter()
            .zip(executed.iter())
            .map(|(tx, res)| match *res {
                Err(TransactionError::InstructionError(_, _)) => {
                    // Charge the transaction fee even in case of InstructionError
                    self.withdraw(&tx.account_keys[0], tx.fee)?;
                    fees += tx.fee;
                    Ok(())
                }
                Ok(()) => {
                    fees += tx.fee;
                    Ok(())
                }
                _ => res.clone(),
            })
            .collect();
        self.deposit(&self.collector_id, fees);
        results
    }

    pub fn commit_transactions(
        &self,
        txs: &[Transaction],
        loaded_accounts: &[Result<(InstructionAccounts, InstructionLoaders)>],
        executed: &[Result<()>],
    ) -> Vec<Result<()>> {
        if self.is_frozen() {
            warn!("=========== FIXME: commit_transactions() working on a frozen bank! ================");
        }

        self.is_delta.store(true, Ordering::Relaxed);

        // TODO: put this assert back in
        // assert!(!self.is_frozen());
        let now = Instant::now();
        self.accounts
            .store_accounts(self.accounts_id, txs, executed, loaded_accounts);

        // once committed there is no way to unroll
        let write_elapsed = now.elapsed();
        debug!(
            "store: {}us txs_len={}",
            duration_as_us(&write_elapsed),
            txs.len(),
        );
        self.update_transaction_statuses(txs, &executed);
        self.filter_program_errors_and_collect_fee(txs, executed)
    }

    /// Process a batch of transactions.
    #[must_use]
    pub fn load_execute_and_commit_transactions(
        &self,
        txs: &[Transaction],
        lock_results: Vec<Result<()>>,
        max_age: usize,
    ) -> Vec<Result<()>> {
        let (loaded_accounts, executed) =
            self.load_and_execute_transactions(txs, lock_results, max_age);

        self.commit_transactions(txs, &loaded_accounts, &executed)
    }

    #[must_use]
    pub fn process_transactions(&self, txs: &[Transaction]) -> Vec<Result<()>> {
        let lock_results = self.lock_accounts(txs);
        let results =
            self.load_execute_and_commit_transactions(txs, lock_results, MAX_RECENT_BLOCKHASHES);
        self.unlock_accounts(txs, &results);
        results
    }

    /// Create, sign, and process a Transaction from `keypair` to `to` of
    /// `n` lamports where `blockhash` is the last Entry ID observed by the client.
    pub fn transfer(
        &self,
        n: u64,
        keypair: &Keypair,
        to: &Pubkey,
        blockhash: Hash,
    ) -> Result<Signature> {
        let tx = SystemTransaction::new_account(keypair, to, n, blockhash, 0);
        let signature = tx.signatures[0];
        self.process_transaction(&tx).map(|_| signature)
    }

    pub fn read_balance(account: &Account) -> u64 {
        account.lamports
    }
    /// Each program would need to be able to introspect its own state
    /// this is hard-coded to the Budget language
    pub fn get_balance(&self, pubkey: &Pubkey) -> u64 {
        self.get_account(pubkey)
            .map(|x| Self::read_balance(&x))
            .unwrap_or(0)
    }

    /// Compute all the parents of the bank in order
    pub fn parents(&self) -> Vec<Arc<Bank>> {
        let mut parents = vec![];
        let mut bank = self.parent();
        while let Some(parent) = bank {
            parents.push(parent.clone());
            bank = parent.parent();
        }
        parents
    }

    pub fn withdraw(&self, pubkey: &Pubkey, lamports: u64) -> Result<()> {
        match self.get_account(pubkey) {
            Some(mut account) => {
                if lamports > account.lamports {
                    return Err(TransactionError::InsufficientFundsForFee);
                }

                account.lamports -= lamports;
                self.accounts.store_slow(self.accounts_id, pubkey, &account);
                Ok(())
            }
            None => Err(TransactionError::AccountNotFound),
        }
    }

    pub fn deposit(&self, pubkey: &Pubkey, lamports: u64) {
        let mut account = self.get_account(pubkey).unwrap_or_default();
        account.lamports += lamports;
        self.accounts.store_slow(self.accounts_id, pubkey, &account);
    }

    pub fn get_account(&self, pubkey: &Pubkey) -> Option<Account> {
        self.accounts.load_slow(self.accounts_id, pubkey)
    }

    pub fn get_program_accounts_modified_since_parent(
        &self,
        program_id: &Pubkey,
    ) -> Vec<(Pubkey, Account)> {
        self.accounts
            .load_by_program_slow_no_parent(self.accounts_id, program_id)
    }

    pub fn get_account_modified_since_parent(&self, pubkey: &Pubkey) -> Option<Account> {
        self.accounts.load_slow_no_parent(self.accounts_id, pubkey)
    }

    pub fn transaction_count(&self) -> u64 {
        self.accounts.transaction_count(self.accounts_id)
    }

    pub fn get_signature_status(&self, signature: &Signature) -> Option<Result<()>> {
        let parents = self.parents();
        let mut caches = vec![self.status_cache.read().unwrap()];
        caches.extend(parents.iter().map(|b| b.status_cache.read().unwrap()));
        StatusCache::get_signature_status_all(&caches, signature)
    }

    pub fn has_signature(&self, signature: &Signature) -> bool {
        let parents = self.parents();
        let mut caches = vec![self.status_cache.read().unwrap()];
        caches.extend(parents.iter().map(|b| b.status_cache.read().unwrap()));
        StatusCache::has_signature_all(&caches, signature)
    }

    /// Hash the `accounts` HashMap. This represents a validator's interpretation
    ///  of the delta of the ledger since the last vote and up to now
    fn hash_internal_state(&self) -> Hash {
        // If there are no accounts, return the same hash as we did before
        // checkpointing.
        if !self.accounts.has_accounts(self.accounts_id) {
            return self.parent_hash;
        }

        let accounts_delta_hash = self.accounts.hash_internal_state(self.accounts_id);
        extend_and_hash(&self.parent_hash, &serialize(&accounts_delta_hash).unwrap())
    }

    /// Return the number of ticks per slot
    pub fn ticks_per_slot(&self) -> u64 {
        self.ticks_per_slot
    }

    /// Return the number of ticks since genesis.
    pub fn tick_height(&self) -> u64 {
        // tick_height is using an AtomicUSize because AtomicU64 is not yet a stable API.
        // Until we can switch to AtomicU64, fail if usize is not the same as u64
        assert_eq!(std::usize::MAX, 0xFFFF_FFFF_FFFF_FFFF);
        self.tick_height.load(Ordering::SeqCst) as u64
    }

    /// Return the number of slots per epoch for the given epoch
    pub fn get_slots_in_epoch(&self, epoch: u64) -> u64 {
        self.epoch_schedule.get_slots_in_epoch(epoch)
    }

    /// returns the epoch for which this bank's stakers_slot_offset and slot would
    ///  need to cache stakers
    pub fn get_stakers_epoch(&self, slot: u64) -> u64 {
        self.epoch_schedule.get_stakers_epoch(slot)
    }

    /// current vote accounts for this bank
    pub fn vote_accounts(&self) -> impl Iterator<Item = (Pubkey, Account)> {
        self.accounts.get_vote_accounts(self.accounts_id)
    }

    ///  vote accounts for the specific epoch
    pub fn epoch_vote_accounts(&self, epoch: u64) -> Option<&HashMap<Pubkey, Account>> {
        self.epoch_vote_accounts.get(&epoch)
    }

    /// given a slot, return the epoch and offset into the epoch this slot falls
    /// e.g. with a fixed number for slots_per_epoch, the calculation is simply:
    ///
    ///  ( slot/slots_per_epoch, slot % slots_per_epoch )
    ///
    pub fn get_epoch_and_slot_index(&self, slot: u64) -> (u64, u64) {
        self.epoch_schedule.get_epoch_and_slot_index(slot)
    }

    pub fn is_votable(&self) -> bool {
        let max_tick_height = (self.slot + 1) * self.ticks_per_slot - 1;
        self.is_delta.load(Ordering::Relaxed) && self.tick_height() == max_tick_height
    }

    /// Add an instruction processor to intercept intructions before the dynamic loader.
    pub fn add_instruction_processor(
        &mut self,
        program_id: Pubkey,
        process_instruction: ProcessInstruction,
    ) {
        self.runtime
            .add_instruction_processor(program_id, process_instruction);

        // Add a bogus executable account to load.
        let bogus_account = Account {
            lamports: 1,
            data: vec![],
            owner: native_loader::id(),
            executable: true,
        };
        self.accounts
            .store_slow(self.accounts_id, &program_id, &bogus_account);
    }

    pub fn is_in_subtree_of(&self, parent: u64) -> bool {
        if self.slot() == parent {
            return true;
        }
        let mut next_parent = self.parent();

        while let Some(p) = next_parent {
            if p.slot() == parent {
                return true;
            } else if p.slot() < parent {
                return false;
            }
            next_parent = p.parent();
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bincode::serialize;
    use solana_sdk::genesis_block::{GenesisBlock, BOOTSTRAP_LEADER_LAMPORTS};
    use solana_sdk::hash;
    use solana_sdk::signature::{Keypair, KeypairUtil};
    use solana_sdk::system_instruction::SystemInstruction;
    use solana_sdk::system_program;
    use solana_sdk::system_transaction::SystemTransaction;
    use solana_sdk::transaction::{CompiledInstruction, InstructionError};

    #[test]
    fn test_bank_new() {
        let (genesis_block, _) = GenesisBlock::new(10_000);
        let bank = Bank::new(&genesis_block);
        assert_eq!(bank.get_balance(&genesis_block.mint_id), 10_000);
    }

    #[test]
    fn test_bank_new_with_leader() {
        let dummy_leader_id = Keypair::new().pubkey();
        let dummy_leader_lamports = BOOTSTRAP_LEADER_LAMPORTS;
        let (genesis_block, _) =
            GenesisBlock::new_with_leader(10_000, &dummy_leader_id, dummy_leader_lamports);
        assert_eq!(
            genesis_block.bootstrap_leader_lamports,
            dummy_leader_lamports
        );
        let bank = Bank::new(&genesis_block);
        assert_eq!(
            bank.get_balance(&genesis_block.mint_id),
            10_000 - dummy_leader_lamports
        );
        assert_eq!(
            bank.get_balance(&dummy_leader_id),
            dummy_leader_lamports - 1 /* 1 token goes to the vote account associated with dummy_leader_lamports */
        );
    }

    #[test]
    fn test_two_payments_to_one_party() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(10_000);
        let pubkey = Keypair::new().pubkey();
        let bank = Bank::new(&genesis_block);
        assert_eq!(bank.last_blockhash(), genesis_block.hash());

        bank.transfer(1_000, &mint_keypair, &pubkey, genesis_block.hash())
            .unwrap();
        assert_eq!(bank.get_balance(&pubkey), 1_000);

        bank.transfer(500, &mint_keypair, &pubkey, genesis_block.hash())
            .unwrap();
        assert_eq!(bank.get_balance(&pubkey), 1_500);
        assert_eq!(bank.transaction_count(), 2);
    }

    #[test]
    fn test_one_source_two_tx_one_batch() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(1);
        let key1 = Keypair::new().pubkey();
        let key2 = Keypair::new().pubkey();
        let bank = Bank::new(&genesis_block);
        assert_eq!(bank.last_blockhash(), genesis_block.hash());

        let t1 = SystemTransaction::new_move(&mint_keypair, &key1, 1, genesis_block.hash(), 0);
        let t2 = SystemTransaction::new_move(&mint_keypair, &key2, 1, genesis_block.hash(), 0);
        let res = bank.process_transactions(&vec![t1.clone(), t2.clone()]);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0], Ok(()));
        assert_eq!(res[1], Err(TransactionError::AccountInUse));
        assert_eq!(bank.get_balance(&mint_keypair.pubkey()), 0);
        assert_eq!(bank.get_balance(&key1), 1);
        assert_eq!(bank.get_balance(&key2), 0);
        assert_eq!(bank.get_signature_status(&t1.signatures[0]), Some(Ok(())));
        // TODO: Transactions that fail to pay a fee could be dropped silently
        assert_eq!(
            bank.get_signature_status(&t2.signatures[0]),
            Some(Err(TransactionError::AccountInUse))
        );
    }

    #[test]
    fn test_one_tx_two_out_atomic_fail() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(1);
        let key1 = Keypair::new().pubkey();
        let key2 = Keypair::new().pubkey();
        let bank = Bank::new(&genesis_block);
        let spend = SystemInstruction::Move { lamports: 1 };
        let instructions = vec![
            CompiledInstruction {
                program_ids_index: 0,
                data: serialize(&spend).unwrap(),
                accounts: vec![0, 1],
            },
            CompiledInstruction {
                program_ids_index: 0,
                data: serialize(&spend).unwrap(),
                accounts: vec![0, 2],
            },
        ];

        let t1 = Transaction::new_with_compiled_instructions(
            &[&mint_keypair],
            &[key1, key2],
            genesis_block.hash(),
            0,
            vec![system_program::id()],
            instructions,
        );
        let res = bank.process_transactions(&vec![t1.clone()]);
        assert_eq!(res.len(), 1);
        assert_eq!(bank.get_balance(&mint_keypair.pubkey()), 1);
        assert_eq!(bank.get_balance(&key1), 0);
        assert_eq!(bank.get_balance(&key2), 0);
        assert_eq!(
            bank.get_signature_status(&t1.signatures[0]),
            Some(Err(TransactionError::InstructionError(
                1,
                InstructionError::new_result_with_negative_lamports(),
            )))
        );
    }

    #[test]
    fn test_one_tx_two_out_atomic_pass() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(2);
        let key1 = Keypair::new().pubkey();
        let key2 = Keypair::new().pubkey();
        let bank = Bank::new(&genesis_block);
        let t1 = SystemTransaction::new_move_many(
            &mint_keypair,
            &[(key1, 1), (key2, 1)],
            genesis_block.hash(),
            0,
        );
        let res = bank.process_transactions(&vec![t1.clone()]);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0], Ok(()));
        assert_eq!(bank.get_balance(&mint_keypair.pubkey()), 0);
        assert_eq!(bank.get_balance(&key1), 1);
        assert_eq!(bank.get_balance(&key2), 1);
        assert_eq!(bank.get_signature_status(&t1.signatures[0]), Some(Ok(())));
    }

    // This test demonstrates that fees are paid even when a program fails.
    #[test]
    fn test_detect_failed_duplicate_transactions() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(2);
        let bank = Bank::new(&genesis_block);
        let dest = Keypair::new();

        // source with 0 program context
        let tx = SystemTransaction::new_account(
            &mint_keypair,
            &dest.pubkey(),
            2,
            genesis_block.hash(),
            1,
        );
        let signature = tx.signatures[0];
        assert!(!bank.has_signature(&signature));

        assert_eq!(
            bank.process_transaction(&tx),
            Err(TransactionError::InstructionError(
                0,
                InstructionError::new_result_with_negative_lamports(),
            ))
        );

        // The lamports didn't move, but the from address paid the transaction fee.
        assert_eq!(bank.get_balance(&dest.pubkey()), 0);

        // This should be the original balance minus the transaction fee.
        assert_eq!(bank.get_balance(&mint_keypair.pubkey()), 1);
    }

    #[test]
    fn test_account_not_found() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(0);
        let bank = Bank::new(&genesis_block);
        let keypair = Keypair::new();
        assert_eq!(
            bank.transfer(1, &keypair, &mint_keypair.pubkey(), genesis_block.hash()),
            Err(TransactionError::AccountNotFound)
        );
        assert_eq!(bank.transaction_count(), 0);
    }

    #[test]
    fn test_insufficient_funds() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(11_000);
        let bank = Bank::new(&genesis_block);
        let pubkey = Keypair::new().pubkey();
        bank.transfer(1_000, &mint_keypair, &pubkey, genesis_block.hash())
            .unwrap();
        assert_eq!(bank.transaction_count(), 1);
        assert_eq!(bank.get_balance(&pubkey), 1_000);
        assert_eq!(
            bank.transfer(10_001, &mint_keypair, &pubkey, genesis_block.hash()),
            Err(TransactionError::InstructionError(
                0,
                InstructionError::new_result_with_negative_lamports(),
            ))
        );
        assert_eq!(bank.transaction_count(), 1);

        let mint_pubkey = mint_keypair.pubkey();
        assert_eq!(bank.get_balance(&mint_pubkey), 10_000);
        assert_eq!(bank.get_balance(&pubkey), 1_000);
    }

    #[test]
    fn test_transfer_to_newb() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(10_000);
        let bank = Bank::new(&genesis_block);
        let pubkey = Keypair::new().pubkey();
        bank.transfer(500, &mint_keypair, &pubkey, genesis_block.hash())
            .unwrap();
        assert_eq!(bank.get_balance(&pubkey), 500);
    }

    #[test]
    fn test_bank_deposit() {
        let (genesis_block, _mint_keypair) = GenesisBlock::new(100);
        let bank = Bank::new(&genesis_block);

        // Test new account
        let key = Keypair::new();
        bank.deposit(&key.pubkey(), 10);
        assert_eq!(bank.get_balance(&key.pubkey()), 10);

        // Existing account
        bank.deposit(&key.pubkey(), 3);
        assert_eq!(bank.get_balance(&key.pubkey()), 13);
    }

    #[test]
    fn test_bank_withdraw() {
        let (genesis_block, _mint_keypair) = GenesisBlock::new(100);
        let bank = Bank::new(&genesis_block);

        // Test no account
        let key = Keypair::new();
        assert_eq!(
            bank.withdraw(&key.pubkey(), 10),
            Err(TransactionError::AccountNotFound)
        );

        bank.deposit(&key.pubkey(), 3);
        assert_eq!(bank.get_balance(&key.pubkey()), 3);

        // Low balance
        assert_eq!(
            bank.withdraw(&key.pubkey(), 10),
            Err(TransactionError::InsufficientFundsForFee)
        );

        // Enough balance
        assert_eq!(bank.withdraw(&key.pubkey(), 2), Ok(()));
        assert_eq!(bank.get_balance(&key.pubkey()), 1);
    }

    #[test]
    fn test_bank_tx_fee() {
        let leader = Keypair::new().pubkey();
        let (genesis_block, mint_keypair) = GenesisBlock::new_with_leader(100, &leader, 3);
        let bank = Bank::new(&genesis_block);
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let tx =
            SystemTransaction::new_move(&mint_keypair, &key1.pubkey(), 2, genesis_block.hash(), 3);
        let initial_balance = bank.get_balance(&leader);
        assert_eq!(bank.process_transaction(&tx), Ok(()));
        assert_eq!(bank.get_balance(&leader), initial_balance + 3);
        assert_eq!(bank.get_balance(&key1.pubkey()), 2);
        assert_eq!(bank.get_balance(&mint_keypair.pubkey()), 100 - 5 - 3);

        let tx = SystemTransaction::new_move(&key1, &key2.pubkey(), 1, genesis_block.hash(), 1);
        assert_eq!(bank.process_transaction(&tx), Ok(()));
        assert_eq!(bank.get_balance(&leader), initial_balance + 4);
        assert_eq!(bank.get_balance(&key1.pubkey()), 0);
        assert_eq!(bank.get_balance(&key2.pubkey()), 1);
        assert_eq!(bank.get_balance(&mint_keypair.pubkey()), 100 - 5 - 3);
    }

    #[test]
    fn test_filter_program_errors_and_collect_fee() {
        let leader = Keypair::new().pubkey();
        let (genesis_block, mint_keypair) = GenesisBlock::new_with_leader(100, &leader, 3);
        let bank = Bank::new(&genesis_block);

        let key = Keypair::new();
        let tx1 =
            SystemTransaction::new_move(&mint_keypair, &key.pubkey(), 2, genesis_block.hash(), 3);
        let tx2 =
            SystemTransaction::new_move(&mint_keypair, &key.pubkey(), 5, genesis_block.hash(), 1);

        let results = vec![
            Ok(()),
            Err(TransactionError::InstructionError(
                1,
                InstructionError::new_result_with_negative_lamports(),
            )),
        ];

        let initial_balance = bank.get_balance(&leader);
        let results = bank.filter_program_errors_and_collect_fee(&vec![tx1, tx2], &results);
        assert_eq!(bank.get_balance(&leader), initial_balance + 3 + 1);
        assert_eq!(results[0], Ok(()));
        assert_eq!(results[1], Ok(()));
    }

    #[test]
    fn test_debits_before_credits() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(2);
        let bank = Bank::new(&genesis_block);
        let keypair = Keypair::new();
        let tx0 = SystemTransaction::new_account(
            &mint_keypair,
            &keypair.pubkey(),
            2,
            genesis_block.hash(),
            0,
        );
        let tx1 = SystemTransaction::new_account(
            &keypair,
            &mint_keypair.pubkey(),
            1,
            genesis_block.hash(),
            0,
        );
        let txs = vec![tx0, tx1];
        let results = bank.process_transactions(&txs);
        assert!(results[1].is_err());

        // Assert bad transactions aren't counted.
        assert_eq!(bank.transaction_count(), 1);
    }

    #[test]
    fn test_process_genesis() {
        let dummy_leader_id = Keypair::new().pubkey();
        let dummy_leader_lamports = 2;
        let (genesis_block, _) =
            GenesisBlock::new_with_leader(5, &dummy_leader_id, dummy_leader_lamports);
        let bank = Bank::new(&genesis_block);
        assert_eq!(bank.get_balance(&genesis_block.mint_id), 3);
        assert_eq!(bank.get_balance(&dummy_leader_id), 1);
    }

    #[test]
    fn test_interleaving_locks() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(3);
        let bank = Bank::new(&genesis_block);
        let alice = Keypair::new();
        let bob = Keypair::new();

        let tx1 = SystemTransaction::new_account(
            &mint_keypair,
            &alice.pubkey(),
            1,
            genesis_block.hash(),
            0,
        );
        let pay_alice = vec![tx1];

        let lock_result = bank.lock_accounts(&pay_alice);
        let results_alice = bank.load_execute_and_commit_transactions(
            &pay_alice,
            lock_result,
            MAX_RECENT_BLOCKHASHES,
        );
        assert_eq!(results_alice[0], Ok(()));

        // try executing an interleaved transfer twice
        assert_eq!(
            bank.transfer(1, &mint_keypair, &bob.pubkey(), genesis_block.hash()),
            Err(TransactionError::AccountInUse)
        );
        // the second time should fail as well
        // this verifies that `unlock_accounts` doesn't unlock `AccountInUse` accounts
        assert_eq!(
            bank.transfer(1, &mint_keypair, &bob.pubkey(), genesis_block.hash()),
            Err(TransactionError::AccountInUse)
        );

        bank.unlock_accounts(&pay_alice, &results_alice);

        assert!(bank
            .transfer(2, &mint_keypair, &bob.pubkey(), genesis_block.hash())
            .is_ok());
    }

    #[test]
    fn test_bank_pay_to_self() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(1);
        let key1 = Keypair::new();
        let bank = Bank::new(&genesis_block);

        bank.transfer(1, &mint_keypair, &key1.pubkey(), genesis_block.hash())
            .unwrap();
        assert_eq!(bank.get_balance(&key1.pubkey()), 1);
        let tx = SystemTransaction::new_move(&key1, &key1.pubkey(), 1, genesis_block.hash(), 0);
        let res = bank.process_transactions(&vec![tx.clone()]);
        assert_eq!(res.len(), 1);
        assert_eq!(bank.get_balance(&key1.pubkey()), 1);

        // TODO: Why do we convert errors to Oks?
        //res[0].clone().unwrap_err();

        bank.get_signature_status(&tx.signatures[0])
            .unwrap()
            .unwrap_err();
    }

    fn new_from_parent(parent: &Arc<Bank>) -> Bank {
        Bank::new_from_parent(parent, &Pubkey::default(), parent.slot() + 1)
    }

    /// Verify that the parent's vector is computed correctly
    #[test]
    fn test_bank_parents() {
        let (genesis_block, _) = GenesisBlock::new(1);
        let parent = Arc::new(Bank::new(&genesis_block));

        let bank = new_from_parent(&parent);
        assert!(Arc::ptr_eq(&bank.parents()[0], &parent));
    }

    /// Verifies that last ids and status cache are correctly referenced from parent
    #[test]
    fn test_bank_parent_duplicate_signature() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(2);
        let key1 = Keypair::new();
        let parent = Arc::new(Bank::new(&genesis_block));

        let tx =
            SystemTransaction::new_move(&mint_keypair, &key1.pubkey(), 1, genesis_block.hash(), 0);
        assert_eq!(parent.process_transaction(&tx), Ok(()));
        let bank = new_from_parent(&parent);
        assert_eq!(
            bank.process_transaction(&tx),
            Err(TransactionError::DuplicateSignature)
        );
    }

    /// Verifies that last ids and accounts are correctly referenced from parent
    #[test]
    fn test_bank_parent_account_spend() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(2);
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let parent = Arc::new(Bank::new(&genesis_block));

        let tx =
            SystemTransaction::new_move(&mint_keypair, &key1.pubkey(), 1, genesis_block.hash(), 0);
        assert_eq!(parent.process_transaction(&tx), Ok(()));
        let bank = new_from_parent(&parent);
        let tx = SystemTransaction::new_move(&key1, &key2.pubkey(), 1, genesis_block.hash(), 0);
        assert_eq!(bank.process_transaction(&tx), Ok(()));
        assert_eq!(parent.get_signature_status(&tx.signatures[0]), None);
    }

    #[test]
    fn test_bank_hash_internal_state() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(2_000);
        let bank0 = Bank::new(&genesis_block);
        let bank1 = Bank::new(&genesis_block);
        let initial_state = bank0.hash_internal_state();
        assert_eq!(bank1.hash_internal_state(), initial_state);

        let pubkey = Keypair::new().pubkey();
        bank0
            .transfer(1_000, &mint_keypair, &pubkey, bank0.last_blockhash())
            .unwrap();
        assert_ne!(bank0.hash_internal_state(), initial_state);
        bank1
            .transfer(1_000, &mint_keypair, &pubkey, bank1.last_blockhash())
            .unwrap();
        assert_eq!(bank0.hash_internal_state(), bank1.hash_internal_state());

        // Checkpointing should not change its state
        let bank2 = new_from_parent(&Arc::new(bank1));
        assert_eq!(bank0.hash_internal_state(), bank2.hash_internal_state());
    }

    #[test]
    fn test_hash_internal_state_genesis() {
        let bank0 = Bank::new(&GenesisBlock::new(10).0);
        let bank1 = Bank::new(&GenesisBlock::new(20).0);
        assert_ne!(bank0.hash_internal_state(), bank1.hash_internal_state());
    }

    #[test]
    fn test_bank_hash_internal_state_squash() {
        let collector_id = Pubkey::default();
        let bank0 = Arc::new(Bank::new(&GenesisBlock::new(10).0));
        let bank1 = Bank::new_from_parent(&bank0, &collector_id, 1);

        // no delta in bank1, hashes match
        assert_eq!(bank0.hash_internal_state(), bank1.hash_internal_state());

        // remove parent
        bank1.squash();
        assert!(bank1.parents().is_empty());

        // hash should still match
        assert_eq!(bank0.hash(), bank1.hash());
    }

    /// Verifies that last ids and accounts are correctly referenced from parent
    #[test]
    fn test_bank_squash() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(2);
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let parent = Arc::new(Bank::new(&genesis_block));

        let tx_move_mint_to_1 =
            SystemTransaction::new_move(&mint_keypair, &key1.pubkey(), 1, genesis_block.hash(), 0);
        assert_eq!(parent.process_transaction(&tx_move_mint_to_1), Ok(()));
        assert_eq!(parent.transaction_count(), 1);

        let bank = new_from_parent(&parent);
        assert_eq!(bank.transaction_count(), parent.transaction_count());
        let tx_move_1_to_2 =
            SystemTransaction::new_move(&key1, &key2.pubkey(), 1, genesis_block.hash(), 0);
        assert_eq!(bank.process_transaction(&tx_move_1_to_2), Ok(()));
        assert_eq!(bank.transaction_count(), 2);
        assert_eq!(parent.transaction_count(), 1);
        assert_eq!(
            parent.get_signature_status(&tx_move_1_to_2.signatures[0]),
            None
        );

        for _ in 0..3 {
            // first time these should match what happened above, assert that parents are ok
            assert_eq!(bank.get_balance(&key1.pubkey()), 0);
            assert_eq!(bank.get_account(&key1.pubkey()), None);
            assert_eq!(bank.get_balance(&key2.pubkey()), 1);
            assert_eq!(
                bank.get_signature_status(&tx_move_mint_to_1.signatures[0]),
                Some(Ok(()))
            );
            assert_eq!(
                bank.get_signature_status(&tx_move_1_to_2.signatures[0]),
                Some(Ok(()))
            );

            // works iteration 0, no-ops on iteration 1 and 2
            bank.squash();

            assert_eq!(parent.transaction_count(), 1);
            assert_eq!(bank.transaction_count(), 2);
        }
    }

    #[test]
    fn test_bank_get_account_in_parent_after_squash() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(500);
        let parent = Arc::new(Bank::new(&genesis_block));

        let key1 = Keypair::new();

        parent
            .transfer(1, &mint_keypair, &key1.pubkey(), genesis_block.hash())
            .unwrap();
        assert_eq!(parent.get_balance(&key1.pubkey()), 1);
        let bank = new_from_parent(&parent);
        bank.squash();
        assert_eq!(parent.get_balance(&key1.pubkey()), 1);
    }

    #[test]
    fn test_bank_epoch_vote_accounts() {
        let leader_id = Keypair::new().pubkey();
        let leader_lamports = 3;
        let (mut genesis_block, _) = GenesisBlock::new_with_leader(5, &leader_id, leader_lamports);

        // set this up weird, forces future generation, odd mod(), etc.
        //  this says: "stakes for slot X should be generated at slot index 3 in slot X-2...
        const SLOTS_PER_EPOCH: u64 = 8;
        const STAKERS_SLOT_OFFSET: u64 = 21;
        genesis_block.slots_per_epoch = SLOTS_PER_EPOCH;
        genesis_block.stakers_slot_offset = STAKERS_SLOT_OFFSET;
        genesis_block.epoch_warmup = false; // allows me to do the normal division stuff below

        let parent = Arc::new(Bank::new(&genesis_block));

        let vote_accounts0: Option<HashMap<_, _>> = parent.epoch_vote_accounts(0).map(|accounts| {
            accounts
                .iter()
                .filter_map(|(pubkey, account)| {
                    if let Ok(vote_state) = VoteState::deserialize(&account.data) {
                        if vote_state.delegate_id == leader_id {
                            Some((*pubkey, true))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect()
        });
        assert!(vote_accounts0.is_some());
        assert!(vote_accounts0.iter().len() != 0);

        let mut i = 1;
        loop {
            if i > STAKERS_SLOT_OFFSET / SLOTS_PER_EPOCH {
                break;
            }
            assert!(parent.epoch_vote_accounts(i).is_some());
            i += 1;
        }

        // child crosses epoch boundary and is the first slot in the epoch
        let child = Bank::new_from_parent(
            &parent,
            &leader_id,
            SLOTS_PER_EPOCH - (STAKERS_SLOT_OFFSET % SLOTS_PER_EPOCH),
        );

        assert!(child.epoch_vote_accounts(i).is_some());

        // child crosses epoch boundary but isn't the first slot in the epoch
        let child = Bank::new_from_parent(
            &parent,
            &leader_id,
            SLOTS_PER_EPOCH - (STAKERS_SLOT_OFFSET % SLOTS_PER_EPOCH) + 1,
        );
        assert!(child.epoch_vote_accounts(i).is_some());
    }

    #[test]
    fn test_zero_signatures() {
        solana_logger::setup();
        let (genesis_block, mint_keypair) = GenesisBlock::new(500);
        let bank = Arc::new(Bank::new(&genesis_block));
        let key = Keypair::new();

        let move_lamports = SystemInstruction::Move { lamports: 1 };

        let mut tx = Transaction::new_with_blockhash_and_fee(
            &mint_keypair.pubkey(),
            &[key.pubkey()],
            &system_program::id(),
            &move_lamports,
            bank.last_blockhash(),
            2,
        );

        assert_eq!(
            bank.process_transaction(&tx),
            Err(TransactionError::MissingSignatureForFee)
        );

        // Set the fee to 0, this should give an InstructionError
        // but since no signature we cannot look up the error.
        tx.fee = 0;

        assert_eq!(bank.process_transaction(&tx), Ok(()));
        assert_eq!(bank.get_balance(&key.pubkey()), 0);
    }

    #[test]
    fn test_bank_get_slots_in_epoch() {
        let (genesis_block, _) = GenesisBlock::new(500);

        let bank = Bank::new(&genesis_block);

        assert_eq!(bank.get_slots_in_epoch(0), 1);
        assert_eq!(bank.get_slots_in_epoch(2), 4);
        assert_eq!(bank.get_slots_in_epoch(5000), genesis_block.slots_per_epoch);
    }

    #[test]
    fn test_epoch_schedule() {
        // one week of slots at 8 ticks/slot, 10 ticks/sec is
        // (1 * 7 * 24 * 4500u64).next_power_of_two();

        // test values between 1 and 16, should cover a good mix
        for slots_per_epoch in 1..=16 {
            let epoch_schedule = EpochSchedule::new(slots_per_epoch, slots_per_epoch / 2, true);

            let mut last_stakers = 0;
            let mut last_epoch = 0;
            let mut last_slots_in_epoch = 1;
            for slot in 0..(2 * slots_per_epoch) {
                // verify that stakers_epoch is continuous over the warmup
                //   and into the first normal epoch

                let stakers = epoch_schedule.get_stakers_epoch(slot);
                if stakers != last_stakers {
                    assert_eq!(stakers, last_stakers + 1);
                    last_stakers = stakers;
                }

                let (epoch, offset) = epoch_schedule.get_epoch_and_slot_index(slot);

                //  verify that epoch increases continuously
                if epoch != last_epoch {
                    assert_eq!(epoch, last_epoch + 1);
                    last_epoch = epoch;

                    // verify that slots in an epoch double continuously
                    //   until they reach slots_per_epoch

                    let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
                    if slots_in_epoch != last_slots_in_epoch {
                        if slots_in_epoch != slots_per_epoch {
                            assert_eq!(slots_in_epoch, last_slots_in_epoch * 2);
                        }
                    }
                    last_slots_in_epoch = slots_in_epoch;
                }
                // verify that the slot offset is less than slots_in_epoch
                assert!(offset < last_slots_in_epoch);
            }

            // assert that these changed  ;)
            assert!(last_stakers != 0); // t
            assert!(last_epoch != 0);
            // assert that we got to "normal" mode
            assert!(last_slots_in_epoch == slots_per_epoch);
        }
    }

    #[test]
    fn test_is_delta_true() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(500);
        let bank = Arc::new(Bank::new(&genesis_block));
        let key1 = Keypair::new();
        let tx_move_mint_to_1 =
            SystemTransaction::new_move(&mint_keypair, &key1.pubkey(), 1, genesis_block.hash(), 0);
        assert_eq!(bank.process_transaction(&tx_move_mint_to_1), Ok(()));
        assert_eq!(bank.is_delta.load(Ordering::Relaxed), true);
    }

    #[test]
    fn test_is_votable() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(500);
        let bank = Arc::new(Bank::new(&genesis_block));
        let key1 = Keypair::new();
        assert_eq!(bank.is_votable(), false);

        // Set is_delta to true
        let tx_move_mint_to_1 =
            SystemTransaction::new_move(&mint_keypair, &key1.pubkey(), 1, genesis_block.hash(), 0);
        assert_eq!(bank.process_transaction(&tx_move_mint_to_1), Ok(()));
        assert_eq!(bank.is_votable(), false);

        // Register enough ticks to hit max tick height
        for i in 0..genesis_block.ticks_per_slot - 1 {
            bank.register_tick(&hash::hash(format!("hello world {}", i).as_bytes()));
        }

        assert_eq!(bank.is_votable(), true);
    }

    #[test]
    fn test_is_in_subtree_of() {
        let (genesis_block, _) = GenesisBlock::new(1);
        let parent = Arc::new(Bank::new(&genesis_block));
        // Bank 1
        let bank = Arc::new(new_from_parent(&parent));
        // Bank 2
        let bank2 = new_from_parent(&bank);
        // Bank 5
        let bank5 = Bank::new_from_parent(&bank, &Pubkey::default(), 5);

        // Parents of bank 2: 0 -> 1 -> 2
        assert!(bank2.is_in_subtree_of(0));
        assert!(bank2.is_in_subtree_of(1));
        assert!(bank2.is_in_subtree_of(2));
        assert!(!bank2.is_in_subtree_of(3));

        // Parents of bank 5: 0 -> 1 -> 5
        assert!(bank5.is_in_subtree_of(0));
        assert!(bank5.is_in_subtree_of(1));
        assert!(!bank5.is_in_subtree_of(2));
        assert!(!bank5.is_in_subtree_of(4));
    }
}
