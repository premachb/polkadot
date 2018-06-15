// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

extern crate ed25519;
extern crate substrate_client as client;
extern crate substrate_codec as codec;
extern crate substrate_extrinsic_pool as extrinsic_pool;
extern crate substrate_primitives as substrate_primitives;
extern crate substrate_runtime_primitives;
extern crate polkadot_runtime as runtime;
extern crate polkadot_primitives as primitives;
extern crate polkadot_api;
extern crate parking_lot;

#[cfg(test)]
extern crate substrate_keyring;

#[macro_use]
extern crate error_chain;

#[macro_use]
extern crate log;

mod error;

use std::{
	cmp::Ordering,
	collections::HashMap,
	ops::Deref,
	sync::Arc,
};

use codec::Slicable;
use extrinsic_pool::{Pool, txpool::{self, Readiness, scoring::{Change, Choice}}};
use extrinsic_pool::api::ExtrinsicPool;
use polkadot_api::PolkadotApi;
use primitives::{AccountId, BlockId, Hash, Index, UncheckedExtrinsic as FutureProofUncheckedExtrinsic};
use runtime::{Address, UncheckedExtrinsic};
use substrate_runtime_primitives::traits::{Bounded, Checkable, Hashing, BlakeTwo256};

pub use extrinsic_pool::txpool::{Options, Status, LightStatus, VerifiedTransaction as VerifiedTransactionOps};
pub use error::{Error, ErrorKind, Result};

/// Type alias for convenience.
pub type CheckedExtrinsic = <UncheckedExtrinsic as Checkable>::Checked;

/// A verified transaction which should be includable and non-inherent.
#[derive(Clone, Debug)]
pub struct VerifiedTransaction {
	original: UncheckedExtrinsic,
	inner: Option<CheckedExtrinsic>,
	sender: Option<AccountId>,
	hash: Hash,
	encoded_size: usize,
}

impl VerifiedTransaction {
	/// Access the underlying transaction.
	pub fn as_transaction(&self) -> &UncheckedExtrinsic {
		&self.original
	}

	/// Convert to primitive unchecked extrinsic.
	pub fn primitive_extrinsic(&self) -> ::primitives::UncheckedExtrinsic {
		Slicable::decode(&mut self.as_transaction().encode().as_slice())
			.expect("UncheckedExtrinsic shares repr with Vec<u8>; qed")
	}

	/// Consume the verified transaction, yielding the checked counterpart.
	pub fn into_inner(self) -> Option<CheckedExtrinsic> {
		self.inner
	}

	/// Get the 256-bit hash of this transaction.
	pub fn hash(&self) -> &Hash {
		&self.hash
	}

	/// Get the account ID of the sender of this transaction.
	pub fn sender(&self) -> Option<AccountId> {
		self.sender
	}

	/// Get the account ID of the sender of this transaction.
	pub fn index(&self) -> Index {
		self.original.extrinsic.index
	}

	/// Get encoded size of the transaction.
	pub fn encoded_size(&self) -> usize {
		self.encoded_size
	}

	/// Returns `true` if the transaction is not yet fully verified.
	pub fn is_fully_verified(&self) -> bool {
		self.inner.is_some()
	}
}

impl txpool::VerifiedTransaction for VerifiedTransaction {
	type Hash = Hash;
	type Sender = Option<AccountId>;

	fn hash(&self) -> &Self::Hash {
		&self.hash
	}

	fn sender(&self) -> &Self::Sender {
		&self.sender
	}

	fn mem_usage(&self) -> usize {
		self.encoded_size // TODO
	}
}

/// Scoring implementation for polkadot transactions.
#[derive(Debug)]
pub struct Scoring;

impl txpool::Scoring<VerifiedTransaction> for Scoring {
	type Score = u64;
	type Event = ();

	fn compare(&self, old: &VerifiedTransaction, other: &VerifiedTransaction) -> Ordering {
		old.index().cmp(&other.index())
	}

	fn choose(&self, old: &VerifiedTransaction, new: &VerifiedTransaction) -> Choice {
		if old.is_fully_verified() {
			assert!(new.is_fully_verified(), "Scoring::choose called with transactions from different senders");
			if old.index() == new.index() {
				// TODO [ToDr] Do we allow replacement? If yes then it should be Choice::ReplaceOld
				return Choice::RejectNew;
			}
		}

		// This will keep both transactions, even though they have the same indices.
		// It's fine for not fully verified transactions, we might also allow it for
		// verified transactions but it would mean that only one of the two is actually valid
		// (most likely the first to be included in the block).
		Choice::InsertNew
	}

	fn update_scores(
		&self,
		xts: &[txpool::Transaction<VerifiedTransaction>],
		scores: &mut [Self::Score],
		_change: Change<()>
	) {
		for i in 0..xts.len() {
			if !xts[i].is_fully_verified() {
				scores[i] = 0;
			} else {
				// all the same score since there are no fees.
				// TODO: prioritize things like misbehavior or fishermen reports
				scores[i] = 1;
			}
		}
	}
	fn should_replace(&self, old: &VerifiedTransaction, _new: &VerifiedTransaction) -> bool {
		// Always replace not fully verified transactions.
		!old.is_fully_verified()
	}
}

/// Readiness evaluator for polkadot transactions.
pub struct Ready<'a, A: 'a + PolkadotApi> {
	at_block: A::CheckedBlockId,
	api: &'a A,
	known_nonces: HashMap<AccountId, ::primitives::Index>,
}

impl<'a, A: 'a + PolkadotApi> Ready<'a, A> {
	/// Create a new readiness evaluator at the given block. Requires that
	/// the ID has already been checked for local corresponding and available state.
	pub fn create(at: A::CheckedBlockId, api: &'a A) -> Self {
		Ready {
			at_block: at,
			api,
			known_nonces: HashMap::new(),
		}
	}
}

impl<'a, T: 'a + PolkadotApi> Clone for Ready<'a, T> {
	fn clone(&self) -> Self {
		Ready {
			at_block: self.at_block.clone(),
			api: self.api,
			known_nonces: self.known_nonces.clone(),
		}
	}
}

impl<'a, A: 'a + PolkadotApi> txpool::Ready<VerifiedTransaction> for Ready<'a, A>
{
	fn is_ready(&mut self, xt: &VerifiedTransaction) -> Readiness {
		let sender = match xt.sender() {
			Some(sender) => sender,
			None => return Readiness::Future
		};

		trace!(target: "transaction-pool", "Checking readiness of {} (from {})", xt.hash, Hash::from(sender));

		// TODO: find a way to handle index error properly -- will need changes to
		// transaction-pool trait.
		let (api, at_block) = (&self.api, &self.at_block);
		let next_index = self.known_nonces.entry(sender)
			.or_insert_with(|| api.index(at_block, sender).ok().unwrap_or_else(Bounded::max_value));

		trace!(target: "transaction-pool", "Next index for sender is {}; xt index is {}", next_index, xt.original.extrinsic.index);

		let result = match xt.original.extrinsic.index.cmp(&next_index) {
			// TODO: this won't work perfectly since accounts can now be killed, returning the nonce
			// to zero.
			// We should detect if the index was reset and mark all transactions as `Stale` for cull to work correctly.
			// Otherwise those transactions will keep occupying the queue.
			// Perhaps we could mark as stale if `index - state_index` > X?
			Ordering::Greater => Readiness::Future,
			Ordering::Equal => Readiness::Ready,
			// TODO [ToDr] Should mark transactions referrencing too old blockhash as `Stale` as well.
			Ordering::Less => Readiness::Stale,
		};

		// remember to increment `next_index`
		*next_index = next_index.saturating_add(1);

		result
	}
}

pub struct Verifier<'a, A: 'a, B> {
	api: &'a A,
	at_block: B,
}

impl<'a, A> txpool::Verifier<UncheckedExtrinsic> for Verifier<'a, A, A::CheckedBlockId> where
	A: 'a + PolkadotApi + Send + Sync,
	A::CheckedBlockId: Sync,
{
	type VerifiedTransaction = VerifiedTransaction;
	type Error = Error;

	fn verify_transaction(&self, uxt: UncheckedExtrinsic) -> Result<Self::VerifiedTransaction> {
		const NO_ACCOUNT: &str = "Account not found.";

		info!("Extrinsic Submitted: {:?}", uxt);

		if !uxt.is_signed() {
			bail!(ErrorKind::IsInherent(uxt))
		}

		let (encoded_size, hash) = uxt.using_encoded(|e| (e.len(), BlakeTwo256::hash(e)));
		// TODO [ToDr] Consider introducing a cache for this.
		let lookup = move |address: Address| match self.api.lookup(&self.at_block, address.clone()) {
			Ok(Some(address)) => Ok(address),
			Ok(None) => Err(NO_ACCOUNT.into()),
			Err(e) => {
				error!("Error looking up address: {:?}: {:?}", address, e);
				Err("API error.")
			},
		};
		let inner = match uxt.clone().check(lookup) {
			Ok(xt) => Some(xt),
			// keep the transaction around in the future pool and attempt to promote it later.
			Err(NO_ACCOUNT) => None,
			Err(e) => bail!(e),
		};
		let sender = inner.as_ref().map(|x| x.signed.clone());

		Ok(VerifiedTransaction {
			original: uxt,
			inner,
			sender,
			hash,
			encoded_size
		})
	}
}

/// The polkadot transaction pool.
///
/// Wraps a `extrinsic_pool::Pool`.
pub struct TransactionPool<A> {
	inner: Pool<Hash, VerifiedTransaction, Scoring, Error>,
	api: A,
}

impl<A> TransactionPool<A> where
	A: PolkadotApi + Send + Sync,
	A::CheckedBlockId: Sync,
{
	/// Create a new transaction pool.
	pub fn new(options: Options, api: A) -> Self {
		TransactionPool {
			inner: Pool::new(options, Scoring),
			api,
		}
	}

	/// Attempt to directly import `UncheckedExtrinsic` without going through serialization.
	pub fn import_unchecked_extrinsic(&self, block: BlockId, uxt: UncheckedExtrinsic) -> Result<Arc<VerifiedTransaction>> {
		let verifier = Verifier {
			api: &self.api,
			at_block: self.api.check_id(block)?,
		};
		self.inner.submit(verifier, vec![uxt]).map(|mut v| v.swap_remove(0))
	}

	pub fn retry_verification(&self) {
		// This function should get all transactions from `None` sender (not fully verified ones)
		// and attempt to verify them again with current block number
	}
}

impl<A> Deref for TransactionPool<A> {
	type Target = Pool<Hash, VerifiedTransaction, Scoring, Error>;

	fn deref(&self) -> &Self::Target {
		&self.inner
	}
}

impl<A> ExtrinsicPool<FutureProofUncheckedExtrinsic, BlockId, Hash> for TransactionPool<A> where
	A: Send + Sync + 'static,
	A: PolkadotApi,
	A::CheckedBlockId: Sync,
{
	type Error = Error;

	fn submit(&self, block: BlockId, xts: Vec<FutureProofUncheckedExtrinsic>) -> Result<Vec<Hash>> {
		// TODO: more general transaction pool, which can handle more kinds of vec-encoded transactions,
		// even when runtime is out of date.
		xts.into_iter()
			.map(|xt| xt.encode())
			.map(|encoded| {
				let decoded = UncheckedExtrinsic::decode(&mut &encoded[..]).ok_or(ErrorKind::InvalidExtrinsicFormat)?;
				let tx = self.import_unchecked_extrinsic(block, decoded)?;
				Ok(*tx.hash())
			})
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::{TransactionPool, Ready};
	use substrate_keyring::Keyring::{self, *};
	use codec::Slicable;
	use polkadot_api::{PolkadotApi, BlockBuilder, CheckedBlockId, Result};
	use primitives::{AccountId, AccountIndex, Block, BlockId, Hash, Index, SessionKey, Timestamp,
		UncheckedExtrinsic as FutureProofUncheckedExtrinsic};
	use runtime::{RawAddress, Call, TimestampCall, BareExtrinsic, Extrinsic, UncheckedExtrinsic};
	use primitives::parachain::{CandidateReceipt, DutyRoster, Id as ParaId};
	use substrate_runtime_primitives::{MaybeUnsigned, generic};

	struct TestBlockBuilder;
	impl BlockBuilder for TestBlockBuilder {
		fn push_extrinsic(&mut self, _extrinsic: FutureProofUncheckedExtrinsic) -> Result<()> { unimplemented!() }
		fn bake(self) -> Result<Block> { unimplemented!() }
	}

	#[derive(Clone)]
	struct TestCheckedBlockId(pub BlockId);
	impl CheckedBlockId for TestCheckedBlockId {
		fn block_id(&self) -> &BlockId { &self.0 }
	}

	fn number_of(at: &TestCheckedBlockId) -> u32 {
		match at.0 {
			generic::BlockId::Number(n) => n as u32,
			_ => 0,
		}
	}

	#[derive(Clone)]
	struct TestPolkadotApi;
	impl PolkadotApi for TestPolkadotApi {
		type CheckedBlockId = TestCheckedBlockId;
		type BlockBuilder = TestBlockBuilder;

		fn check_id(&self, id: BlockId) -> Result<TestCheckedBlockId> { Ok(TestCheckedBlockId(id)) }
		fn session_keys(&self, _at: &TestCheckedBlockId) -> Result<Vec<SessionKey>> { unimplemented!() }
		fn validators(&self, _at: &TestCheckedBlockId) -> Result<Vec<AccountId>> { unimplemented!() }
		fn random_seed(&self, _at: &TestCheckedBlockId) -> Result<Hash> { unimplemented!() }
		fn duty_roster(&self, _at: &TestCheckedBlockId) -> Result<DutyRoster> { unimplemented!() }
		fn timestamp(&self, _at: &TestCheckedBlockId) -> Result<u64> { unimplemented!() }
		fn evaluate_block(&self, _at: &TestCheckedBlockId, _block: Block) -> Result<bool> { unimplemented!() }
		fn active_parachains(&self, _at: &TestCheckedBlockId) -> Result<Vec<ParaId>> { unimplemented!() }
		fn parachain_code(&self, _at: &TestCheckedBlockId, _parachain: ParaId) -> Result<Option<Vec<u8>>> { unimplemented!() }
		fn parachain_head(&self, _at: &TestCheckedBlockId, _parachain: ParaId) -> Result<Option<Vec<u8>>> { unimplemented!() }
		fn build_block(&self, _at: &TestCheckedBlockId, _timestamp: Timestamp, _new_heads: Vec<CandidateReceipt>) -> Result<Self::BlockBuilder> { unimplemented!() }
		fn inherent_extrinsics(&self, _at: &TestCheckedBlockId, _timestamp: Timestamp, _new_heads: Vec<CandidateReceipt>) -> Result<Vec<Vec<u8>>> { unimplemented!() }

		fn index(&self, _at: &TestCheckedBlockId, _account: AccountId) -> Result<Index> {
			Ok((_account[0] as u32) + number_of(_at))
		}
		fn lookup(&self, _at: &TestCheckedBlockId, _address: RawAddress<AccountId, AccountIndex>) -> Result<Option<AccountId>> {
			match _address {
				RawAddress::Id(i) => Ok(Some(i)),
				RawAddress::Index(i) => Ok(match (i < 8, i + (number_of(_at) as u64) % 8) {
					(false, _) => None,
					(_, 0) => Some(Alice.to_raw_public().into()),
					(_, 1) => Some(Bob.to_raw_public().into()),
					(_, 2) => Some(Charlie.to_raw_public().into()),
					(_, 3) => Some(Dave.to_raw_public().into()),
					(_, 4) => Some(Eve.to_raw_public().into()),
					(_, 5) => Some(Ferdie.to_raw_public().into()),
					(_, 6) => Some(One.to_raw_public().into()),
					(_, 7) => Some(Two.to_raw_public().into()),
					_ => None,
				}),
			}
		}
	}

	fn uxt(who: Keyring, nonce: Index, use_id: bool) -> UncheckedExtrinsic {
		let sxt = BareExtrinsic {
			signed: who.to_raw_public().into(),
			index: nonce,
			function: Call::Timestamp(TimestampCall::set(0)),
		};
		let sig = sxt.using_encoded(|e| who.sign(e));
		UncheckedExtrinsic::new(Extrinsic {
			signed: if use_id { RawAddress::Id(sxt.signed) } else { RawAddress::Index(
				match who {
					Alice => 0,
					Bob => 1,
					Charlie => 2,
					Dave => 3,
					Eve => 4,
					Ferdie => 5,
					One => 6,
					Two => 7,
				}
			)},
			index: sxt.index,
			function: sxt.function,
		}, MaybeUnsigned(sig.into())).using_encoded(|e| UncheckedExtrinsic::decode(&mut &e[..])).unwrap()
	}

	fn pool() -> TransactionPool<TestPolkadotApi> {
		TransactionPool::new(Default::default(), TestPolkadotApi)
	}

	#[test]
	fn id_submission_should_work() {
		let pool = pool();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 209, true)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![(Some(Alice.to_raw_public().into()), 209)]);
	}

	#[test]
	fn index_submission_should_work() {
		let pool = pool();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 209, false)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![(Some(Alice.to_raw_public().into()), 209)]);
	}

	#[test]
	fn multiple_id_submission_should_work() {
		let pool = pool();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 209, true)).unwrap();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 210, true)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![(Some(Alice.to_raw_public().into()), 209), (Some(Alice.to_raw_public().into()), 210)]);
	}

	#[test]
	fn multiple_index_submission_should_work() {
		let pool = pool();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 209, false)).unwrap();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 210, false)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![(Some(Alice.to_raw_public().into()), 209), (Some(Alice.to_raw_public().into()), 210)]);
	}

	#[test]
	fn id_based_early_nonce_should_be_culled() {
		let pool = pool();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 208, true)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![]);
	}

	#[test]
	fn index_based_early_nonce_should_be_culled() {
		let pool = pool();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 208, false)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![]);
	}

	#[test]
	fn id_based_late_nonce_should_be_queued() {
		let pool = pool();
		let ready = || Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);

		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 210, true)).unwrap();
		let pending: Vec<_> = pool.cull_and_get_pending(ready(), |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![]);

		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 209, true)).unwrap();
		let pending: Vec<_> = pool.cull_and_get_pending(ready(), |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![(Some(Alice.to_raw_public().into()), 209), (Some(Alice.to_raw_public().into()), 210)]);
	}

	#[test]
	fn index_based_late_nonce_should_be_queued() {
		let pool = pool();
		let ready = || Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);

		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 210, false)).unwrap();
		let pending: Vec<_> = pool.cull_and_get_pending(ready(), |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![]);

		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 209, false)).unwrap();
		let pending: Vec<_> = pool.cull_and_get_pending(ready(), |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![(Some(Alice.to_raw_public().into()), 209), (Some(Alice.to_raw_public().into()), 210)]);
	}

	#[test]
	fn index_then_id_submission_should_make_progress() {
		let pool = pool();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 209, false)).unwrap();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 210, true)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![
			(Some(Alice.to_raw_public().into()), 209)
		]);
	}

	#[test]
	fn id_then_index_submission_should_make_progress() {
		let pool = pool();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 209, true)).unwrap();
		pool.import_unchecked_extrinsic(BlockId::number(0), uxt(Alice, 210, false)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![
			(Some(Alice.to_raw_public().into()), 209)
		]);
	}

	#[test]
	fn index_change_should_result_in_second_tx_culled_or_future() {
		let pool = pool();
		let block = BlockId::number(0);
		pool.import_unchecked_extrinsic(block, uxt(Alice, 209, false)).unwrap();
		pool.import_unchecked_extrinsic(block, uxt(Alice, 210, false)).unwrap();

		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(0)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![
			(Some(Alice.to_raw_public().into()), 209),
			(Some(Alice.to_raw_public().into()), 210)
		]);

		// first xt is mined, but that has a side-effect of switching index 0 from Alice to Bob.
		// second xt now invalid signature, so it fails.

		// there is no way of reporting this back to the queue right now (TODO). this should cause
		// the queue to flush all information regarding the sender index/account.

		// after this, a re-evaluation of the second's readiness should result in it being thrown
		// out (or maybe placed in future queue).
/*
		// TODO: uncomment once the new queue design is in.
		let ready = Ready::create(TestPolkadotApi.check_id(BlockId::number(1)).unwrap(), &TestPolkadotApi);
		let pending: Vec<_> = pool.cull_and_get_pending(ready, |p| p.map(|a| (a.sender(), a.index())).collect());
		assert_eq!(pending, vec![]);
*/
	}
}
