// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::time::{Instant, Duration};

use util::*;
use util::using_queue::{UsingQueue, GetAction};
use account_provider::{AccountProvider, SignError as AccountError};
use state::{State, CleanupMode};
use client::{MiningBlockChainClient, Executive, Executed, EnvInfo, TransactOptions, BlockId, CallAnalytics, TransactionId};
use client::TransactionImportResult;
use executive::contract_address;
use block::{ClosedBlock, IsBlock, Block};
use error::*;
use transaction::{Action, UnverifiedTransaction, PendingTransaction, SignedTransaction, Condition as TransactionCondition};
use receipt::{Receipt, RichReceipt};
use spec::Spec;
use engines::{Engine, Seal};
use miner::{MinerService, MinerStatus, TransactionQueue, RemovalReason, TransactionQueueDetailsProvider, PrioritizationStrategy,
	AccountDetails, TransactionOrigin};
use miner::banning_queue::{BanningTransactionQueue, Threshold};
use miner::work_notify::{WorkPoster, NotifyWork};
use miner::price_info::PriceInfo;
use miner::local_transactions::{Status as LocalTransactionStatus};
use miner::service_transaction_checker::ServiceTransactionChecker;
use header::BlockNumber;

/// Different possible definitions for pending transaction set.
#[derive(Debug, PartialEq)]
pub enum PendingSet {
	/// Always just the transactions in the queue. These have had only cheap checks.
	AlwaysQueue,
	/// Always just the transactions in the sealing block. These have had full checks but
	/// may be empty if the node is not actively mining or has force_sealing enabled.
	AlwaysSealing,
	/// Try the sealing block, but if it is not currently sealing, fallback to the queue.
	SealingOrElseQueue,
}

/// Type of the gas limit to apply to the transaction queue.
#[derive(Debug, PartialEq)]
pub enum GasLimit {
	/// Depends on the block gas limit and is updated with every block.
	Auto,
	/// No limit.
	None,
	/// Set to a fixed gas value.
	Fixed(U256),
}

/// Transaction queue banning settings.
#[derive(Debug, PartialEq, Clone)]
pub enum Banning {
	/// Banning in transaction queue is disabled
	Disabled,
	/// Banning in transaction queue is enabled
	Enabled {
		/// Upper limit of transaction processing time before banning.
		offend_threshold: Duration,
		/// Number of similar offending transactions before banning.
		min_offends: u16,
		/// Number of seconds the offender is banned for.
		ban_duration: Duration,
	},
}

/// Configures the behaviour of the miner.
#[derive(Debug, PartialEq)]
pub struct MinerOptions {
	/// URLs to notify when there is new work.
	pub new_work_notify: Vec<String>,
	/// Force the miner to reseal, even when nobody has asked for work.
	pub force_sealing: bool,
	/// Reseal on receipt of new external transactions.
	pub reseal_on_external_tx: bool,
	/// Reseal on receipt of new local transactions.
	pub reseal_on_own_tx: bool,
	/// Minimum period between transaction-inspired reseals.
	pub reseal_min_period: Duration,
	/// Maximum period between blocks (enables force sealing after that).
	pub reseal_max_period: Duration,
	/// Maximum amount of gas to bother considering for block insertion.
	pub tx_gas_limit: U256,
	/// Maximum size of the transaction queue.
	pub tx_queue_size: usize,
	/// Strategy to use for prioritizing transactions in the queue.
	pub tx_queue_strategy: PrioritizationStrategy,
	/// Whether we should fallback to providing all the queue's transactions or just pending.
	pub pending_set: PendingSet,
	/// How many historical work packages can we store before running out?
	pub work_queue_size: usize,
	/// Can we submit two different solutions for the same block and expect both to result in an import?
	pub enable_resubmission: bool,
	/// Global gas limit for all transaction in the queue except for local and retracted.
	pub tx_queue_gas_limit: GasLimit,
	/// Banning settings.
	pub tx_queue_banning: Banning,
	/// Do we refuse to accept service transactions even if sender is certified.
	pub refuse_service_transactions: bool,
}

impl Default for MinerOptions {
	fn default() -> Self {
		MinerOptions {
			new_work_notify: vec![],
			force_sealing: false,
			reseal_on_external_tx: false,
			reseal_on_own_tx: true,
			tx_gas_limit: !U256::zero(),
			tx_queue_size: 1024,
			tx_queue_gas_limit: GasLimit::Auto,
			tx_queue_strategy: PrioritizationStrategy::GasPriceOnly,
			pending_set: PendingSet::AlwaysQueue,
			reseal_min_period: Duration::from_secs(2),
			reseal_max_period: Duration::from_secs(120),
			work_queue_size: 20,
			enable_resubmission: true,
			tx_queue_banning: Banning::Disabled,
			refuse_service_transactions: false,
		}
	}
}

/// Options for the dynamic gas price recalibrator.
#[derive(Debug, PartialEq)]
pub struct GasPriceCalibratorOptions {
	/// Base transaction price to match against.
	pub usd_per_tx: f32,
	/// How frequently we should recalibrate.
	pub recalibration_period: Duration,
}

/// The gas price validator variant for a `GasPricer`.
#[derive(Debug, PartialEq)]
pub struct GasPriceCalibrator {
	options: GasPriceCalibratorOptions,
	next_calibration: Instant,
}

impl GasPriceCalibrator {
	fn recalibrate<F: Fn(U256) + Sync + Send + 'static>(&mut self, set_price: F) {
		trace!(target: "miner", "Recalibrating {:?} versus {:?}", Instant::now(), self.next_calibration);
		if Instant::now() >= self.next_calibration {
			let usd_per_tx = self.options.usd_per_tx;
			trace!(target: "miner", "Getting price info");

			PriceInfo::get(move |price: PriceInfo| {
				trace!(target: "miner", "Price info arrived: {:?}", price);
				let usd_per_eth = price.ethusd;
				let wei_per_usd: f32 = 1.0e18 / usd_per_eth;
				let gas_per_tx: f32 = 21000.0;
				let wei_per_gas: f32 = wei_per_usd * usd_per_tx / gas_per_tx;
				info!(target: "miner", "Updated conversion rate to Ξ1 = {} ({} wei/gas)", Colour::White.bold().paint(format!("US${:.2}", usd_per_eth)), Colour::Yellow.bold().paint(format!("{}", wei_per_gas)));
				set_price(U256::from(wei_per_gas as u64));
			});

			self.next_calibration = Instant::now() + self.options.recalibration_period;
		}
	}
}

/// Struct to look after updating the acceptable gas price of a miner.
#[derive(Debug, PartialEq)]
pub enum GasPricer {
	/// A fixed gas price in terms of Wei - always the argument given.
	Fixed(U256),
	/// Gas price is calibrated according to a fixed amount of USD.
	Calibrated(GasPriceCalibrator),
}

impl GasPricer {
	/// Create a new Calibrated `GasPricer`.
	pub fn new_calibrated(options: GasPriceCalibratorOptions) -> GasPricer {
		GasPricer::Calibrated(GasPriceCalibrator {
			options: options,
			next_calibration: Instant::now(),
		})
	}

	/// Create a new Fixed `GasPricer`.
	pub fn new_fixed(gas_price: U256) -> GasPricer {
		GasPricer::Fixed(gas_price)
	}

	fn recalibrate<F: Fn(U256) + Sync + Send + 'static>(&mut self, set_price: F) {
		match *self {
			GasPricer::Fixed(ref max) => set_price(max.clone()),
			GasPricer::Calibrated(ref mut cal) => cal.recalibrate(set_price),
		}
	}
}

struct SealingWork {
	queue: UsingQueue<ClosedBlock>,
	enabled: bool,
}

/// Keeps track of transactions using priority queue and holds currently mined block.
/// Handles preparing work for "work sealing" or seals "internally" if Engine does not require work.
pub struct Miner {
	// NOTE [ToDr]  When locking always lock in this order!
	transaction_queue: Arc<RwLock<BanningTransactionQueue>>,
	sealing_work: Mutex<SealingWork>,
	next_allowed_reseal: Mutex<Instant>,
	next_mandatory_reseal: RwLock<Instant>,
	sealing_block_last_request: Mutex<u64>,
	// for sealing...
	options: MinerOptions,

	gas_range_target: RwLock<(U256, U256)>,
	author: RwLock<Address>,
	extra_data: RwLock<Bytes>,
	engine: Arc<Engine>,

	accounts: Option<Arc<AccountProvider>>,
	notifiers: RwLock<Vec<Box<NotifyWork>>>,
	gas_pricer: Mutex<GasPricer>,
	service_transaction_action: ServiceTransactionAction,
}

impl Miner {
	/// Push notifier that will handle new jobs
	pub fn push_notifier(&self, notifier: Box<NotifyWork>) {
		self.notifiers.write().push(notifier);
		self.sealing_work.lock().enabled = true;
	}

	/// Creates new instance of miner Arc.
	pub fn new(options: MinerOptions, gas_pricer: GasPricer, spec: &Spec, accounts: Option<Arc<AccountProvider>>) -> Arc<Miner> {
		Arc::new(Miner::new_raw(options, gas_pricer, spec, accounts))
	}

	/// Creates new instance of miner.
	fn new_raw(options: MinerOptions, gas_pricer: GasPricer, spec: &Spec, accounts: Option<Arc<AccountProvider>>) -> Miner {
		let gas_limit = match options.tx_queue_gas_limit {
			GasLimit::Fixed(ref limit) => *limit,
			_ => !U256::zero(),
		};

		let txq = TransactionQueue::with_limits(options.tx_queue_strategy, options.tx_queue_size, gas_limit, options.tx_gas_limit);
		let txq = match options.tx_queue_banning {
			Banning::Disabled => BanningTransactionQueue::new(txq, Threshold::NeverBan, Duration::from_secs(180)),
			Banning::Enabled { ban_duration, min_offends, .. } => BanningTransactionQueue::new(
				txq,
				Threshold::BanAfter(min_offends),
				ban_duration,
			),
		};

		let notifiers: Vec<Box<NotifyWork>> = match options.new_work_notify.is_empty() {
			true => Vec::new(),
			false => vec![Box::new(WorkPoster::new(&options.new_work_notify))],
		};

		let service_transaction_action = match options.refuse_service_transactions {
			true => ServiceTransactionAction::Refuse,
			false => ServiceTransactionAction::Check(ServiceTransactionChecker::default()),
		};

		Miner {
			transaction_queue: Arc::new(RwLock::new(txq)),
			next_allowed_reseal: Mutex::new(Instant::now()),
			next_mandatory_reseal: RwLock::new(Instant::now() + options.reseal_max_period),
			sealing_block_last_request: Mutex::new(0),
			sealing_work: Mutex::new(SealingWork{
				queue: UsingQueue::new(options.work_queue_size),
				enabled: options.force_sealing
					|| !options.new_work_notify.is_empty()
					|| spec.engine.seals_internally().is_some()
			}),
			gas_range_target: RwLock::new((U256::zero(), U256::zero())),
			author: RwLock::new(Address::default()),
			extra_data: RwLock::new(Vec::new()),
			options: options,
			accounts: accounts,
			engine: spec.engine.clone(),
			notifiers: RwLock::new(notifiers),
			gas_pricer: Mutex::new(gas_pricer),
			service_transaction_action: service_transaction_action,
		}
	}

	/// Creates new instance of miner with accounts and with given spec.
	pub fn with_spec_and_accounts(spec: &Spec, accounts: Option<Arc<AccountProvider>>) -> Miner {
		Miner::new_raw(Default::default(), GasPricer::new_fixed(20_000_000_000u64.into()), spec, accounts)
	}

	/// Creates new instance of miner without accounts, but with given spec.
	pub fn with_spec(spec: &Spec) -> Miner {
		Miner::new_raw(Default::default(), GasPricer::new_fixed(20_000_000_000u64.into()), spec, None)
	}

	fn forced_sealing(&self) -> bool {
		self.options.force_sealing || !self.notifiers.read().is_empty()
	}

	/// Clear all pending block states
	pub fn clear(&self) {
		self.sealing_work.lock().queue.reset();
	}

	/// Get `Some` `clone()` of the current pending block's state or `None` if we're not sealing.
	pub fn pending_state(&self) -> Option<State<::state_db::StateDB>> {
		self.sealing_work.lock().queue.peek_last_ref().map(|b| b.block().fields().state.clone())
	}

	/// Get `Some` `clone()` of the current pending block or `None` if we're not sealing.
	pub fn pending_block(&self) -> Option<Block> {
		self.sealing_work.lock().queue.peek_last_ref().map(|b| b.to_base())
	}

	#[cfg_attr(feature="dev", allow(match_same_arms))]
	/// Prepares new block for sealing including top transactions from queue.
	fn prepare_block(&self, chain: &MiningBlockChainClient) -> (ClosedBlock, Option<H256>) {
		let _timer = PerfTimer::new("prepare_block");
		let chain_info = chain.chain_info();
		let (transactions, mut open_block, original_work_hash) = {
			let nonce_cap = if chain_info.best_block_number + 1 >= self.engine.params().dust_protection_transition {
				Some((self.engine.params().nonce_cap_increment * (chain_info.best_block_number + 1)).into())
			} else { None };
			let transactions = {self.transaction_queue.read().top_transactions_at(chain_info.best_block_number, chain_info.best_block_timestamp, nonce_cap)};
			let mut sealing_work = self.sealing_work.lock();
			let last_work_hash = sealing_work.queue.peek_last_ref().map(|pb| pb.block().fields().header.hash());
			let best_hash = chain_info.best_block_hash;
/*
			// check to see if last ClosedBlock in would_seals is actually same parent block.
			// if so
			//   duplicate, re-open and push any new transactions.
			//   if at least one was pushed successfully, close and enqueue new ClosedBlock;
			//   otherwise, leave everything alone.
			// otherwise, author a fresh block.
*/
			let open_block = match sealing_work.queue.pop_if(|b| b.block().fields().header.parent_hash() == &best_hash) {
				Some(old_block) => {
					trace!(target: "miner", "prepare_block: Already have previous work; updating and returning");
					// add transactions to old_block
					old_block.reopen(&*self.engine)
				}
				None => {
					// block not found - create it.
					trace!(target: "miner", "prepare_block: No existing work - making new block");
					chain.prepare_open_block(
						self.author(),
						(self.gas_floor_target(), self.gas_ceil_target()),
						self.extra_data()
					)
				}
			};
			(transactions, open_block, last_work_hash)
		};

		let mut invalid_transactions = HashSet::new();
		let mut transactions_to_penalize = HashSet::new();
		let block_number = open_block.block().fields().header.number();

		// TODO Push new uncles too.
		let mut tx_count: usize = 0;
		let tx_total = transactions.len();
		for tx in transactions {
			let hash = tx.hash();
			let start = Instant::now();
			let result = open_block.push_transaction(tx, None);
			let took = start.elapsed();

			// Check for heavy transactions
			match self.options.tx_queue_banning {
				Banning::Enabled { ref offend_threshold, .. } if &took > offend_threshold => {
					match self.transaction_queue.write().ban_transaction(&hash) {
						true => {
							warn!(target: "miner", "Detected heavy transaction. Banning the sender and recipient/code.");
						},
						false => {
							transactions_to_penalize.insert(hash);
							debug!(target: "miner", "Detected heavy transaction. Penalizing sender.")
						}
					}
				},
				_ => {},
			}
			trace!(target: "miner", "Adding tx {:?} took {:?}", hash, took);
			match result {
				Err(Error::Execution(ExecutionError::BlockGasLimitReached { gas_limit, gas_used, gas })) => {
					debug!(target: "miner", "Skipping adding transaction to block because of gas limit: {:?} (limit: {:?}, used: {:?}, gas: {:?})", hash, gas_limit, gas_used, gas);

					// Penalize transaction if it's above current gas limit
					if gas > gas_limit {
						transactions_to_penalize.insert(hash);
					}

					// Exit early if gas left is smaller then min_tx_gas
					let min_tx_gas: U256 = 21000.into();	// TODO: figure this out properly.
					if gas_limit - gas_used < min_tx_gas {
						break;
					}
				},
				// Invalid nonce error can happen only if previous transaction is skipped because of gas limit.
				// If there is errornous state of transaction queue it will be fixed when next block is imported.
				Err(Error::Execution(ExecutionError::InvalidNonce { expected, got })) => {
					debug!(target: "miner", "Skipping adding transaction to block because of invalid nonce: {:?} (expected: {:?}, got: {:?})", hash, expected, got);
				},
				// already have transaction - ignore
				Err(Error::Transaction(TransactionError::AlreadyImported)) => {},
				Err(e) => {
					invalid_transactions.insert(hash);
					debug!(target: "miner",
						   "Error adding transaction to block: number={}. transaction_hash={:?}, Error: {:?}",
						   block_number, hash, e);
				},
				_ => {
					tx_count += 1;
				}	// imported ok
			}
		}
		trace!(target: "miner", "Pushed {}/{} transactions", tx_count, tx_total);

		let block = open_block.close();

		let fetch_nonce = |a: &Address| chain.latest_nonce(a);

		{
			let mut queue = self.transaction_queue.write();
			for hash in invalid_transactions {
				queue.remove(&hash, &fetch_nonce, RemovalReason::Invalid);
			}
			for hash in transactions_to_penalize {
				queue.penalize(&hash);
			}
		}
		(block, original_work_hash)
	}

	/// Asynchronously updates minimal gas price for transaction queue
	pub fn recalibrate_minimal_gas_price(&self) {
		debug!(target: "miner", "minimal_gas_price: recalibrating...");
		let txq = self.transaction_queue.clone();
		self.gas_pricer.lock().recalibrate(move |price| {
			debug!(target: "miner", "minimal_gas_price: Got gas price! {}", price);
			txq.write().set_minimal_gas_price(price);
		});
	}

	/// Check is reseal is allowed and necessary.
	fn requires_reseal(&self, best_block: BlockNumber) -> bool {
		let has_local_transactions = self.transaction_queue.read().has_local_pending_transactions();
		let mut sealing_work = self.sealing_work.lock();
		if sealing_work.enabled {
			trace!(target: "miner", "requires_reseal: sealing enabled");
			let last_request = *self.sealing_block_last_request.lock();
			let should_disable_sealing = !self.forced_sealing()
				&& !has_local_transactions
				&& self.engine.seals_internally().is_none()
				&& best_block > last_request
				&& best_block - last_request > SEALING_TIMEOUT_IN_BLOCKS;

			trace!(target: "miner", "requires_reseal: should_disable_sealing={}; best_block={}, last_request={}", should_disable_sealing, best_block, last_request);

			if should_disable_sealing {
				trace!(target: "miner", "Miner sleeping (current {}, last {})", best_block, last_request);
				sealing_work.enabled = false;
				sealing_work.queue.reset();
				false
			} else {
				// sealing enabled and we don't want to sleep.
				*self.next_allowed_reseal.lock() = Instant::now() + self.options.reseal_min_period;
				true
			}
		} else {
			trace!(target: "miner", "requires_reseal: sealing is disabled");
			false
		}
	}

	/// Attempts to perform internal sealing (one that does not require work) and handles the result depending on the type of Seal.
	fn seal_and_import_block_internally(&self, chain: &MiningBlockChainClient, block: ClosedBlock) -> bool {
		if !block.transactions().is_empty() || self.forced_sealing() || Instant::now() > *self.next_mandatory_reseal.read() {
			trace!(target: "miner", "seal_block_internally: attempting internal seal.");
			match self.engine.generate_seal(block.block()) {
				// Save proposal for later seal submission and broadcast it.
				Seal::Proposal(seal) => {
					trace!(target: "miner", "Received a Proposal seal.");
					*self.next_mandatory_reseal.write() = Instant::now() + self.options.reseal_max_period;
					{
						let mut sealing_work = self.sealing_work.lock();
						sealing_work.queue.push(block.clone());
						sealing_work.queue.use_last_ref();
					}
					block
						.lock()
						.seal(&*self.engine, seal)
						.map(|sealed| { chain.broadcast_proposal_block(sealed); true })
						.unwrap_or_else(|e| {
							warn!("ERROR: seal failed when given internally generated seal: {}", e);
							false
						})
				},
				// Directly import a regular sealed block.
				Seal::Regular(seal) => {
					*self.next_mandatory_reseal.write() = Instant::now() + self.options.reseal_max_period;
					block
						.lock()
						.seal(&*self.engine, seal)
						.map(|sealed| chain.import_sealed_block(sealed).is_ok())
						.unwrap_or_else(|e| {
							warn!("ERROR: seal failed when given internally generated seal: {}", e);
							false
						})
				},
				Seal::None => false,
			}
		} else {
			false
		}
	}

	/// Prepares work which has to be done to seal.
	fn prepare_work(&self, block: ClosedBlock, original_work_hash: Option<H256>) {
		let (work, is_new) = {
			let mut sealing_work = self.sealing_work.lock();
			let last_work_hash = sealing_work.queue.peek_last_ref().map(|pb| pb.block().fields().header.hash());
			trace!(target: "miner", "prepare_work: Checking whether we need to reseal: orig={:?} last={:?}, this={:?}", original_work_hash, last_work_hash, block.block().fields().header.hash());
			let (work, is_new) = if last_work_hash.map_or(true, |h| h != block.block().fields().header.hash()) {
				trace!(target: "miner", "prepare_work: Pushing a new, refreshed or borrowed pending {}...", block.block().fields().header.hash());
				let pow_hash = block.block().fields().header.hash();
				let number = block.block().fields().header.number();
				let difficulty = *block.block().fields().header.difficulty();
				let is_new = original_work_hash.map_or(true, |h| block.block().fields().header.hash() != h);
				sealing_work.queue.push(block);
				// If push notifications are enabled we assume all work items are used.
				if !self.notifiers.read().is_empty() && is_new {
					sealing_work.queue.use_last_ref();
				}
				(Some((pow_hash, difficulty, number)), is_new)
			} else {
				(None, false)
			};
			trace!(target: "miner", "prepare_work: leaving (last={:?})", sealing_work.queue.peek_last_ref().map(|b| b.block().fields().header.hash()));
			(work, is_new)
		};
		if is_new {
			work.map(|(pow_hash, difficulty, number)| {
				for notifier in self.notifiers.read().iter() {
					notifier.notify(pow_hash, difficulty, number)
				}
			});
		}
	}

	fn update_gas_limit(&self, client: &MiningBlockChainClient) {
		let gas_limit = client.best_block_header().gas_limit();
		let mut queue = self.transaction_queue.write();
		queue.set_gas_limit(gas_limit);
		if let GasLimit::Auto = self.options.tx_queue_gas_limit {
			// Set total tx queue gas limit to be 20x the block gas limit.
			queue.set_total_gas_limit(gas_limit * 20.into());
		}
	}

	/// Returns true if we had to prepare new pending block.
	fn prepare_work_sealing(&self, client: &MiningBlockChainClient) -> bool {
		trace!(target: "miner", "prepare_work_sealing: entering");
		let prepare_new = {
			let mut sealing_work = self.sealing_work.lock();
			let have_work = sealing_work.queue.peek_last_ref().is_some();
			trace!(target: "miner", "prepare_work_sealing: have_work={}", have_work);
			if !have_work {
				sealing_work.enabled = true;
				true
			} else {
				false
			}
		};
		if prepare_new {
			// --------------------------------------------------------------------------
			// | NOTE Code below requires transaction_queue and sealing_work locks.     |
			// | Make sure to release the locks before calling that method.             |
			// --------------------------------------------------------------------------
			let (block, original_work_hash) = self.prepare_block(client);
			self.prepare_work(block, original_work_hash);
		}
		let mut sealing_block_last_request = self.sealing_block_last_request.lock();
		let best_number = client.chain_info().best_block_number;
		if *sealing_block_last_request != best_number {
			trace!(target: "miner", "prepare_work_sealing: Miner received request (was {}, now {}) - waking up.", *sealing_block_last_request, best_number);
			*sealing_block_last_request = best_number;
		}

		// Return if we restarted
		prepare_new
	}

	fn add_transactions_to_queue(
		&self,
		client: &MiningBlockChainClient,
		transactions: Vec<UnverifiedTransaction>,
		default_origin: TransactionOrigin,
		condition: Option<TransactionCondition>,
		transaction_queue: &mut BanningTransactionQueue,
	) -> Vec<Result<TransactionImportResult, Error>> {
		let accounts = self.accounts.as_ref()
			.and_then(|provider| provider.accounts().ok())
			.map(|accounts| accounts.into_iter().collect::<HashSet<_>>());

		let best_block_header = client.best_block_header().decode();
		let insertion_time = client.chain_info().best_block_number;

		transactions.into_iter()
			.map(|tx| {
				let hash = tx.hash();
				if client.transaction_block(TransactionId::Hash(hash)).is_some() {
					debug!(target: "miner", "Rejected tx {:?}: already in the blockchain", hash);
					return Err(Error::Transaction(TransactionError::AlreadyImported));
				}
				match self.engine.verify_transaction_basic(&tx, &best_block_header)
					.and_then(|_| self.engine.verify_transaction(tx, &best_block_header))
				{
					Err(e) => {
						debug!(target: "miner", "Rejected tx {:?} with invalid signature: {:?}", hash, e);
						Err(e)
					},
					Ok(transaction) => {
						let origin = accounts.as_ref().and_then(|accounts| {
							match accounts.contains(&transaction.sender()) {
								true => Some(TransactionOrigin::Local),
								false => None,
							}
						}).unwrap_or(default_origin);

						// try to install service transaction checker before appending transactions
						self.service_transaction_action.update_from_chain_client(client);

						let details_provider = TransactionDetailsProvider::new(client, &self.service_transaction_action);
						match origin {
							TransactionOrigin::Local | TransactionOrigin::RetractedBlock => {
								transaction_queue.add(transaction, origin, insertion_time, condition.clone(), &details_provider)
							},
							TransactionOrigin::External => {
								transaction_queue.add_with_banlist(transaction, insertion_time, &details_provider)
							},
						}
					},
				}
			})
			.collect()
	}

	/// Are we allowed to do a non-mandatory reseal?
	fn tx_reseal_allowed(&self) -> bool { Instant::now() > *self.next_allowed_reseal.lock() }

	#[cfg_attr(feature="dev", allow(wrong_self_convention))]
	#[cfg_attr(feature="dev", allow(redundant_closure))]
	fn from_pending_block<H, F, G>(&self, latest_block_number: BlockNumber, from_chain: F, map_block: G) -> H
		where F: Fn() -> H, G: Fn(&ClosedBlock) -> H {
		let sealing_work = self.sealing_work.lock();
		sealing_work.queue.peek_last_ref().map_or_else(
			|| from_chain(),
			|b| {
				if b.block().header().number() > latest_block_number {
					map_block(b)
				} else {
					from_chain()
				}
			}
		)
	}
}

const SEALING_TIMEOUT_IN_BLOCKS : u64 = 5;

impl MinerService for Miner {

	fn clear_and_reset(&self, chain: &MiningBlockChainClient) {
		self.transaction_queue.write().clear();
		// --------------------------------------------------------------------------
		// | NOTE Code below requires transaction_queue and sealing_work locks.     |
		// | Make sure to release the locks before calling that method.             |
		// --------------------------------------------------------------------------
		self.update_sealing(chain);
	}

	fn status(&self) -> MinerStatus {
		let status = self.transaction_queue.read().status();
		let sealing_work = self.sealing_work.lock();
		MinerStatus {
			transactions_in_pending_queue: status.pending,
			transactions_in_future_queue: status.future,
			transactions_in_pending_block: sealing_work.queue.peek_last_ref().map_or(0, |b| b.transactions().len()),
		}
	}

	fn call(&self, client: &MiningBlockChainClient, t: &SignedTransaction, analytics: CallAnalytics) -> Result<Executed, CallError> {
		let sealing_work = self.sealing_work.lock();
		match sealing_work.queue.peek_last_ref() {
			Some(work) => {
				let block = work.block();

				// TODO: merge this code with client.rs's fn call somwhow.
				let header = block.header();
				let last_hashes = Arc::new(client.last_hashes());
				let env_info = EnvInfo {
					number: header.number(),
					author: *header.author(),
					timestamp: header.timestamp(),
					difficulty: *header.difficulty(),
					last_hashes: last_hashes,
					gas_used: U256::zero(),
					gas_limit: U256::max_value(),
				};
				// that's just a copy of the state.
				let mut state = block.state().clone();
				let original_state = if analytics.state_diffing { Some(state.clone()) } else { None };

				let sender = t.sender();
				let balance = state.balance(&sender).map_err(ExecutionError::from)?;
				let needed_balance = t.value + t.gas * t.gas_price;
				if balance < needed_balance {
					// give the sender a sufficient balance
					state.add_balance(&sender, &(needed_balance - balance), CleanupMode::NoEmpty)
						.map_err(ExecutionError::from)?;
				}
				let options = TransactOptions { tracing: analytics.transaction_tracing, vm_tracing: analytics.vm_tracing, check_nonce: false };
				let mut ret = Executive::new(&mut state, &env_info, &*self.engine).transact(t, options)?;

				// TODO gav move this into Executive.
				if let Some(original) = original_state {
					ret.state_diff = Some(state.diff_from(original).map_err(ExecutionError::from)?);
				}

				Ok(ret)
			},
			None => client.call(t, BlockId::Latest, analytics)
		}
	}

	// TODO: The `chain.latest_x` actually aren't infallible, they just panic on corruption.
	// TODO: return trie::Result<T> here, or other.
	fn balance(&self, chain: &MiningBlockChainClient, address: &Address) -> Option<U256> {
		self.from_pending_block(
			chain.chain_info().best_block_number,
			|| Some(chain.latest_balance(address)),
			|b| b.block().fields().state.balance(address).ok(),
		)
	}

	fn storage_at(&self, chain: &MiningBlockChainClient, address: &Address, position: &H256) -> Option<H256> {
		self.from_pending_block(
			chain.chain_info().best_block_number,
			|| Some(chain.latest_storage_at(address, position)),
			|b| b.block().fields().state.storage_at(address, position).ok(),
		)
	}

	fn nonce(&self, chain: &MiningBlockChainClient, address: &Address) -> Option<U256> {
		self.from_pending_block(
			chain.chain_info().best_block_number,
			|| Some(chain.latest_nonce(address)),
			|b| b.block().fields().state.nonce(address).ok(),
		)
	}

	fn code(&self, chain: &MiningBlockChainClient, address: &Address) -> Option<Option<Bytes>> {
		self.from_pending_block(
			chain.chain_info().best_block_number,
			|| Some(chain.latest_code(address)),
			|b| b.block().fields().state.code(address).ok().map(|c| c.map(|c| (&*c).clone()))
		)
	}

	fn set_author(&self, author: Address) {
		if self.engine.seals_internally().is_some() {
			let mut sealing_work = self.sealing_work.lock();
			sealing_work.enabled = true;
		}
		*self.author.write() = author;
	}

	fn set_engine_signer(&self, address: Address, password: String) -> Result<(), AccountError> {
		if self.engine.seals_internally().is_some() {
			if let Some(ref ap) = self.accounts {
				ap.sign(address.clone(), Some(password.clone()), Default::default())?;
				// Limit the scope of the locks.
				{
					let mut sealing_work = self.sealing_work.lock();
					sealing_work.enabled = true;
					*self.author.write() = address;
				}
				// --------------------------------------------------------------------------
				// | NOTE Code below may require author and sealing_work locks              |
				// | (some `Engine`s call `EngineClient.update_sealing()`)                  |.
				// | Make sure to release the locks before calling that method.             |
				// --------------------------------------------------------------------------
				self.engine.set_signer(ap.clone(), address, password);
			}
		}
		Ok(())
	}

	fn set_extra_data(&self, extra_data: Bytes) {
		*self.extra_data.write() = extra_data;
	}

	/// Set the gas limit we wish to target when sealing a new block.
	fn set_gas_floor_target(&self, target: U256) {
		self.gas_range_target.write().0 = target;
	}

	fn set_gas_ceil_target(&self, target: U256) {
		self.gas_range_target.write().1 = target;
	}

	fn set_minimal_gas_price(&self, min_gas_price: U256) {
		self.transaction_queue.write().set_minimal_gas_price(min_gas_price);
	}

	fn minimal_gas_price(&self) -> U256 {
		*self.transaction_queue.read().minimal_gas_price()
	}

	fn sensible_gas_price(&self) -> U256 {
		// 10% above our minimum.
		*self.transaction_queue.read().minimal_gas_price() * 110.into() / 100.into()
	}

	fn sensible_gas_limit(&self) -> U256 {
		self.gas_range_target.read().0 / 5.into()
	}

	fn transactions_limit(&self) -> usize {
		self.transaction_queue.read().limit()
	}

	fn set_transactions_limit(&self, limit: usize) {
		self.transaction_queue.write().set_limit(limit)
	}

	fn set_tx_gas_limit(&self, limit: U256) {
		self.transaction_queue.write().set_tx_gas_limit(limit)
	}

	/// Get the author that we will seal blocks as.
	fn author(&self) -> Address {
		*self.author.read()
	}

	/// Get the extra_data that we will seal blocks with.
	fn extra_data(&self) -> Bytes {
		self.extra_data.read().clone()
	}

	/// Get the gas limit we wish to target when sealing a new block.
	fn gas_floor_target(&self) -> U256 {
		self.gas_range_target.read().0
	}

	/// Get the gas limit we wish to target when sealing a new block.
	fn gas_ceil_target(&self) -> U256 {
		self.gas_range_target.read().1
	}

	fn import_external_transactions(
		&self,
		chain: &MiningBlockChainClient,
		transactions: Vec<UnverifiedTransaction>
	) -> Vec<Result<TransactionImportResult, Error>> {
		trace!(target: "external_tx", "Importing external transactions");
		let results = {
			let mut transaction_queue = self.transaction_queue.write();
			self.add_transactions_to_queue(
				chain, transactions, TransactionOrigin::External, None, &mut transaction_queue
			)
		};

		if !results.is_empty() && self.options.reseal_on_external_tx &&	self.tx_reseal_allowed() {
			// --------------------------------------------------------------------------
			// | NOTE Code below requires transaction_queue and sealing_work locks.     |
			// | Make sure to release the locks before calling that method.             |
			// --------------------------------------------------------------------------
			self.update_sealing(chain);
		}
		results
	}

	#[cfg_attr(feature="dev", allow(collapsible_if))]
	fn import_own_transaction(
		&self,
		chain: &MiningBlockChainClient,
		pending: PendingTransaction,
	) -> Result<TransactionImportResult, Error> {

		trace!(target: "own_tx", "Importing transaction: {:?}", pending);

		let imported = {
			// Be sure to release the lock before we call prepare_work_sealing
			let mut transaction_queue = self.transaction_queue.write();
			// We need to re-validate transactions
			let import = self.add_transactions_to_queue(
				chain, vec![pending.transaction.into()], TransactionOrigin::Local, pending.condition, &mut transaction_queue
			).pop().expect("one result returned per added transaction; one added => one result; qed");

			match import {
				Ok(_) => {
					trace!(target: "own_tx", "Status: {:?}", transaction_queue.status());
				},
				Err(ref e) => {
					trace!(target: "own_tx", "Status: {:?}", transaction_queue.status());
					warn!(target: "own_tx", "Error importing transaction: {:?}", e);
				},
			}
			import
		};

		// --------------------------------------------------------------------------
		// | NOTE Code below requires transaction_queue and sealing_work locks.     |
		// | Make sure to release the locks before calling that method.             |
		// --------------------------------------------------------------------------
		if imported.is_ok() && self.options.reseal_on_own_tx && self.tx_reseal_allowed() {
			// Make sure to do it after transaction is imported and lock is droped.
			// We need to create pending block and enable sealing.
			if self.engine.seals_internally().unwrap_or(false) || !self.prepare_work_sealing(chain) {
				// If new block has not been prepared (means we already had one)
				// or Engine might be able to seal internally,
				// we need to update sealing.
				self.update_sealing(chain);
			}
		}

		imported
	}

	fn pending_transactions(&self) -> Vec<PendingTransaction> {
		let queue = self.transaction_queue.read();
		queue.pending_transactions(BlockNumber::max_value(), u64::max_value())
	}

	fn local_transactions(&self) -> BTreeMap<H256, LocalTransactionStatus> {
		let queue = self.transaction_queue.read();
		queue.local_transactions()
			.iter()
			.map(|(hash, status)| (*hash, status.clone()))
			.collect()
	}

	fn future_transactions(&self) -> Vec<PendingTransaction> {
		self.transaction_queue.read().future_transactions()
	}

	fn ready_transactions(&self, best_block: BlockNumber, best_block_timestamp: u64) -> Vec<PendingTransaction> {
		let queue = self.transaction_queue.read();
		match self.options.pending_set {
			PendingSet::AlwaysQueue => queue.pending_transactions(best_block, best_block_timestamp),
			PendingSet::SealingOrElseQueue => {
				self.from_pending_block(
					best_block,
					|| queue.pending_transactions(best_block, best_block_timestamp),
					|sealing| sealing.transactions().iter().map(|t| t.clone().into()).collect()
				)
			},
			PendingSet::AlwaysSealing => {
				self.from_pending_block(
					best_block,
					|| vec![],
					|sealing| sealing.transactions().iter().map(|t| t.clone().into()).collect()
				)
			},
		}
	}

	fn pending_transactions_hashes(&self, best_block: BlockNumber) -> Vec<H256> {
		let queue = self.transaction_queue.read();
		match self.options.pending_set {
			PendingSet::AlwaysQueue => queue.pending_hashes(),
			PendingSet::SealingOrElseQueue => {
				self.from_pending_block(
					best_block,
					|| queue.pending_hashes(),
					|sealing| sealing.transactions().iter().map(|t| t.hash()).collect()
				)
			},
			PendingSet::AlwaysSealing => {
				self.from_pending_block(
					best_block,
					|| vec![],
					|sealing| sealing.transactions().iter().map(|t| t.hash()).collect()
				)
			},
		}
	}

	fn transaction(&self, best_block: BlockNumber, hash: &H256) -> Option<PendingTransaction> {
		let queue = self.transaction_queue.read();
		match self.options.pending_set {
			PendingSet::AlwaysQueue => queue.find(hash),
			PendingSet::SealingOrElseQueue => {
				self.from_pending_block(
					best_block,
					|| queue.find(hash),
					|sealing| sealing.transactions().iter().find(|t| &t.hash() == hash).cloned().map(Into::into)
				)
			},
			PendingSet::AlwaysSealing => {
				self.from_pending_block(
					best_block,
					|| None,
					|sealing| sealing.transactions().iter().find(|t| &t.hash() == hash).cloned().map(Into::into)
				)
			},
		}
	}

	fn remove_pending_transaction(&self, chain: &MiningBlockChainClient, hash: &H256) -> Option<PendingTransaction> {
		let mut queue = self.transaction_queue.write();
		let tx = queue.find(hash);
		if tx.is_some() {
			let fetch_nonce = |a: &Address| chain.latest_nonce(a);
			queue.remove(hash, &fetch_nonce, RemovalReason::Canceled);
		}
		tx
	}

	fn pending_receipt(&self, best_block: BlockNumber, hash: &H256) -> Option<RichReceipt> {
		self.from_pending_block(
			best_block,
			|| None,
			|pending| {
				let txs = pending.transactions();
				txs.iter()
					.map(|t| t.hash())
					.position(|t| t == *hash)
					.map(|index| {
						let prev_gas = if index == 0 { Default::default() } else { pending.receipts()[index - 1].gas_used };
						let tx = &txs[index];
						let receipt = &pending.receipts()[index];
						RichReceipt {
							transaction_hash: hash.clone(),
							transaction_index: index,
							cumulative_gas_used: receipt.gas_used,
							gas_used: receipt.gas_used - prev_gas,
							contract_address: match tx.action {
								Action::Call(_) => None,
								Action::Create => {
									let sender = tx.sender();
									Some(contract_address(self.engine.create_address_scheme(pending.header().number()), &sender, &tx.nonce, &tx.data).0)
								}
							},
							logs: receipt.logs.clone(),
							log_bloom: receipt.log_bloom,
							state_root: receipt.state_root,
						}
					})
			}
		)
	}

	fn pending_receipts(&self, best_block: BlockNumber) -> BTreeMap<H256, Receipt> {
		self.from_pending_block(
			best_block,
			BTreeMap::new,
			|pending| {
				let hashes = pending.transactions()
					.iter()
					.map(|t| t.hash());

				let receipts = pending.receipts().iter().cloned();

				hashes.zip(receipts).collect()
			}
		)
	}

	fn last_nonce(&self, address: &Address) -> Option<U256> {
		self.transaction_queue.read().last_nonce(address)
	}

	/// Update sealing if required.
	/// Prepare the block and work if the Engine does not seal internally.
	fn update_sealing(&self, chain: &MiningBlockChainClient) {
		trace!(target: "miner", "update_sealing");

		if self.requires_reseal(chain.chain_info().best_block_number) {
			// --------------------------------------------------------------------------
			// | NOTE Code below requires transaction_queue and sealing_work locks.     |
			// | Make sure to release the locks before calling that method.             |
			// --------------------------------------------------------------------------
			trace!(target: "miner", "update_sealing: preparing a block");
			let (block, original_work_hash) = self.prepare_block(chain);
			match self.engine.seals_internally() {
				Some(true) => {
					trace!(target: "miner", "update_sealing: engine indicates internal sealing");
					if self.seal_and_import_block_internally(chain, block) {
						trace!(target: "miner", "update_sealing: imported internally sealed block");
					}
				},
				None => {
					trace!(target: "miner", "update_sealing: engine does not seal internally, preparing work");
					self.prepare_work(block, original_work_hash)
				},
				_ => trace!(target: "miner", "update_sealing: engine is not keen to seal internally right now")
			}
		}
	}

	fn is_sealing(&self) -> bool {
		self.sealing_work.lock().queue.is_in_use()
	}

	fn map_sealing_work<F, T>(&self, chain: &MiningBlockChainClient, f: F) -> Option<T> where F: FnOnce(&ClosedBlock) -> T {
		trace!(target: "miner", "map_sealing_work: entering");
		self.prepare_work_sealing(chain);
		trace!(target: "miner", "map_sealing_work: sealing prepared");
		let mut sealing_work = self.sealing_work.lock();
		let ret = sealing_work.queue.use_last_ref();
		trace!(target: "miner", "map_sealing_work: leaving use_last_ref={:?}", ret.as_ref().map(|b| b.block().fields().header.hash()));
		ret.map(f)
	}

	fn submit_seal(&self, chain: &MiningBlockChainClient, block_hash: H256, seal: Vec<Bytes>) -> Result<(), Error> {
		let result =
			if let Some(b) = self.sealing_work.lock().queue.get_used_if(
				if self.options.enable_resubmission {
					GetAction::Clone
				} else {
					GetAction::Take
				},
				|b| &b.hash() == &block_hash
			) {
				trace!(target: "miner", "Submitted block {}={}={} with seal {:?}", block_hash, b.hash(), b.header().bare_hash(), seal);
				b.lock().try_seal(&*self.engine, seal).or_else(|(e, _)| {
					warn!(target: "miner", "Mined solution rejected: {}", e);
					Err(Error::PowInvalid)
				})
			} else {
				warn!(target: "miner", "Submitted solution rejected: Block unknown or out of date.");
				Err(Error::PowHashInvalid)
			};
		result.and_then(|sealed| {
			let n = sealed.header().number();
			let h = sealed.header().hash();
			chain.import_sealed_block(sealed)?;
			info!(target: "miner", "Submitted block imported OK. #{}: {}", Colour::White.bold().paint(format!("{}", n)), Colour::White.bold().paint(h.hex()));
			Ok(())
		})
	}

	fn chain_new_blocks(&self, chain: &MiningBlockChainClient, _imported: &[H256], _invalid: &[H256], enacted: &[H256], retracted: &[H256]) {
		trace!(target: "miner", "chain_new_blocks");

		// 1. We ignore blocks that were `imported` (because it means that they are not in canon-chain, and transactions
		//	  should be still available in the queue.
		// 2. We ignore blocks that are `invalid` because it doesn't have any meaning in terms of the transactions that
		//    are in those blocks

		// First update gas limit in transaction queue
		self.update_gas_limit(chain);

		// Update minimal gas price
		self.recalibrate_minimal_gas_price();

		// Then import all transactions...
		{

			let mut transaction_queue = self.transaction_queue.write();
			for hash in retracted {
				let block = chain.block(BlockId::Hash(*hash))
					.expect("Client is sending message after commit to db and inserting to chain; the block is available; qed");
				let txs = block.transactions();
				let _ = self.add_transactions_to_queue(
					chain, txs, TransactionOrigin::RetractedBlock, None, &mut transaction_queue
				);
			}
		}

		// ...and at the end remove the old ones
		{
			let fetch_account = |a: &Address| AccountDetails {
				nonce: chain.latest_nonce(a),
				balance: chain.latest_balance(a),
			};
			let time = chain.chain_info().best_block_number;
			let mut transaction_queue = self.transaction_queue.write();
			transaction_queue.remove_old(&fetch_account, time);
		}

		if enacted.len() > 0 {
			// --------------------------------------------------------------------------
			// | NOTE Code below requires transaction_queue and sealing_work locks.     |
			// | Make sure to release the locks before calling that method.             |
			// --------------------------------------------------------------------------
			self.update_sealing(chain);
		}
	}
}

/// Action when service transaction is received
enum ServiceTransactionAction {
	/// Refuse service transaction immediately
	Refuse,
	/// Accept if sender is certified to send service transactions
	Check(ServiceTransactionChecker),
}

impl ServiceTransactionAction {
	pub fn update_from_chain_client(&self, client: &MiningBlockChainClient) {
		if let ServiceTransactionAction::Check(ref checker) = *self {
			checker.update_from_chain_client(client);
		}
	}

	pub fn check(&self, client: &MiningBlockChainClient, tx: &SignedTransaction) -> Result<bool, String> {
		match *self {
			ServiceTransactionAction::Refuse => Err("configured to refuse service transactions".to_owned()),
			ServiceTransactionAction::Check(ref checker) => checker.check(client, tx),
		}
	}
}

struct TransactionDetailsProvider<'a> {
	client: &'a MiningBlockChainClient,
	service_transaction_action: &'a ServiceTransactionAction,
}

impl<'a> TransactionDetailsProvider<'a> {
	pub fn new(client: &'a MiningBlockChainClient, service_transaction_action: &'a ServiceTransactionAction) -> Self {
		TransactionDetailsProvider {
			client: client,
			service_transaction_action: service_transaction_action,
		}
	}
}

impl<'a> TransactionQueueDetailsProvider for TransactionDetailsProvider<'a> {
	fn fetch_account(&self, address: &Address) -> AccountDetails {
		AccountDetails {
			nonce: self.client.latest_nonce(address),
			balance: self.client.latest_balance(address),
		}
	}

	fn estimate_gas_required(&self, tx: &SignedTransaction) -> U256 {
		tx.gas_required(&self.client.latest_schedule()).into()
	}

	fn is_service_transaction_acceptable(&self, tx: &SignedTransaction) -> Result<bool, String> {
		self.service_transaction_action.check(self.client, tx)
	}
}

#[cfg(test)]
mod tests {

	use std::sync::Arc;
	use std::time::Duration;
	use rustc_hex::FromHex;
	use super::super::{MinerService, PrioritizationStrategy};
	use super::*;
	use block::IsBlock;
	use util::U256;
	use ethkey::{Generator, Random};
	use client::{BlockChainClient, TestBlockChainClient, EachBlockWith, TransactionImportResult};
	use header::BlockNumber;
	use types::transaction::{SignedTransaction, Transaction, PendingTransaction, Action};
	use spec::Spec;
	use tests::helpers::{generate_dummy_client};

	#[test]
	fn should_prepare_block_to_seal() {
		// given
		let client = TestBlockChainClient::default();
		let miner = Miner::with_spec(&Spec::new_test());

		// when
		let sealing_work = miner.map_sealing_work(&client, |_| ());
		assert!(sealing_work.is_some(), "Expected closed block");
	}

	#[test]
	fn should_still_work_after_a_couple_of_blocks() {
		// given
		let client = TestBlockChainClient::default();
		let miner = Miner::with_spec(&Spec::new_test());

		let res = miner.map_sealing_work(&client, |b| b.block().fields().header.hash());
		assert!(res.is_some());
		assert!(miner.submit_seal(&client, res.unwrap(), vec![]).is_ok());

		// two more blocks mined, work requested.
		client.add_blocks(1, EachBlockWith::Uncle);
		miner.map_sealing_work(&client, |b| b.block().fields().header.hash());

		client.add_blocks(1, EachBlockWith::Uncle);
		miner.map_sealing_work(&client, |b| b.block().fields().header.hash());

		// solution to original work submitted.
		assert!(miner.submit_seal(&client, res.unwrap(), vec![]).is_ok());
	}

	fn miner() -> Miner {
		Arc::try_unwrap(Miner::new(
			MinerOptions {
				new_work_notify: Vec::new(),
				force_sealing: false,
				reseal_on_external_tx: false,
				reseal_on_own_tx: true,
				reseal_min_period: Duration::from_secs(5),
				reseal_max_period: Duration::from_secs(120),
				tx_gas_limit: !U256::zero(),
				tx_queue_size: 1024,
				tx_queue_gas_limit: GasLimit::None,
				tx_queue_strategy: PrioritizationStrategy::GasFactorAndGasPrice,
				pending_set: PendingSet::AlwaysSealing,
				work_queue_size: 5,
				enable_resubmission: true,
				tx_queue_banning: Banning::Disabled,
				refuse_service_transactions: false,
			},
			GasPricer::new_fixed(0u64.into()),
			&Spec::new_test(),
			None, // accounts provider
		)).ok().expect("Miner was just created.")
	}

	fn transaction() -> SignedTransaction {
		transaction_with_network_id(2)
	}

	fn transaction_with_network_id(id: u64) -> SignedTransaction {
		let keypair = Random.generate().unwrap();
		Transaction {
			action: Action::Create,
			value: U256::zero(),
			data: "3331600055".from_hex().unwrap(),
			gas: U256::from(100_000),
			gas_price: U256::zero(),
			nonce: U256::zero(),
		}.sign(keypair.secret(), Some(id))
	}

	#[test]
	fn should_make_pending_block_when_importing_own_transaction() {
		// given
		let client = TestBlockChainClient::default();
		let miner = miner();
		let transaction = transaction();
		let best_block = 0;
		// when
		let res = miner.import_own_transaction(&client, PendingTransaction::new(transaction, None));

		// then
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		assert_eq!(miner.pending_transactions().len(), 1);
		assert_eq!(miner.ready_transactions(best_block, 0).len(), 1);
		assert_eq!(miner.pending_transactions_hashes(best_block).len(), 1);
		assert_eq!(miner.pending_receipts(best_block).len(), 1);
		// This method will let us know if pending block was created (before calling that method)
		assert!(!miner.prepare_work_sealing(&client));
	}

	#[test]
	fn should_not_use_pending_block_if_best_block_is_higher() {
		// given
		let client = TestBlockChainClient::default();
		let miner = miner();
		let transaction = transaction();
		let best_block = 10;
		// when
		let res = miner.import_own_transaction(&client, PendingTransaction::new(transaction, None));

		// then
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		assert_eq!(miner.pending_transactions().len(), 1);
		assert_eq!(miner.ready_transactions(best_block, 0).len(), 0);
		assert_eq!(miner.pending_transactions_hashes(best_block).len(), 0);
		assert_eq!(miner.pending_receipts(best_block).len(), 0);
	}

	#[test]
	fn should_import_external_transaction() {
		// given
		let client = TestBlockChainClient::default();
		let miner = miner();
		let transaction = transaction().into();
		let best_block = 0;
		// when
		let res = miner.import_external_transactions(&client, vec![transaction]).pop().unwrap();

		// then
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		assert_eq!(miner.pending_transactions().len(), 1);
		assert_eq!(miner.pending_transactions_hashes(best_block).len(), 0);
		assert_eq!(miner.ready_transactions(best_block, 0).len(), 0);
		assert_eq!(miner.pending_receipts(best_block).len(), 0);
		// This method will let us know if pending block was created (before calling that method)
		assert!(miner.prepare_work_sealing(&client));
	}

	#[test]
	fn should_not_seal_unless_enabled() {
		let miner = miner();
		let client = TestBlockChainClient::default();
		// By default resealing is not required.
		assert!(!miner.requires_reseal(1u8.into()));

		miner.import_external_transactions(&client, vec![transaction().into()]).pop().unwrap().unwrap();
		assert!(miner.prepare_work_sealing(&client));
		// Unless asked to prepare work.
		assert!(miner.requires_reseal(1u8.into()));
	}

	#[test]
	fn internal_seals_without_work() {
		let spec = Spec::new_instant();
		let miner = Miner::with_spec(&spec);

		let client = generate_dummy_client(2);

		assert_eq!(miner.import_external_transactions(&*client, vec![transaction_with_network_id(spec.network_id()).into()]).pop().unwrap().unwrap(), TransactionImportResult::Current);

		miner.update_sealing(&*client);
		client.flush_queue();
		assert!(miner.pending_block().is_none());
		assert_eq!(client.chain_info().best_block_number, 3 as BlockNumber);

		assert_eq!(miner.import_own_transaction(&*client, PendingTransaction::new(transaction_with_network_id(spec.network_id()).into(), None)).unwrap(), TransactionImportResult::Current);

		miner.update_sealing(&*client);
		client.flush_queue();
		assert!(miner.pending_block().is_none());
		assert_eq!(client.chain_info().best_block_number, 4 as BlockNumber);
	}
}
