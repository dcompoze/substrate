// This file is part of Substrate.

// Copyright (C) 2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The signed phase of the multi-block election system.
//!
//! Signed submissions work on the basis of keeping a queue of submissions from random signed
//! accounts, and sorting them based on the best claimed score to the worse.
//!
//! Once the time to evaluate the signed phase comes, the solutions are checked from best-to-worse
//! claim, and they end up in either of the 3 buckets:
//!
//! 1. If they are the first, correct solution (and consequently the best one, since we start
//!    evaluating from the best claim), they are rewarded.
//! 2. Any solution after the first correct solution is refunded in an unbiased way.
//! 3. Any invalid solution that wasted valuable blockchain time gets slashed for their deposit.
//!
//! # Future Plans:
//!
//! Overall, this pallet can avoid the need to delete any storage item, by:
//! 1. outsource the storage of solution data to some other pallet.
//! 2. keep it here, but make everything be also a map of the round number, so that we can keep old
//!    storage, and it is ONLY EVER removed, when after that round number is over. This can happen
//!    for more or less free by the submitter itself, and by anyone else as well, in which case they
//!    get a share of the the sum deposit. The share increases as times goes on.

use codec::{Decode, Encode, MaxEncodedLen};
use frame_election_provider_support::PageIndex;
use frame_support::{
	traits::{Currency, Defensive, ReservableCurrency},
	BoundedVec, RuntimeDebugNoBound,
};
use scale_info::TypeInfo;
use sp_npos_elections::ElectionScore;
use sp_runtime::traits::{Saturating, Zero};
use sp_std::prelude::*;

/// Exports of this pallet
pub use pallet::*;

use crate::verifier::{AsynchronousVerifier, SolutionDataProvider, Status, VerificationResult};

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[cfg(test)]
mod tests;

type BalanceOf<T> =
	<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

/// All of the (meta) data around a signed submission
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Default, RuntimeDebugNoBound)]
#[codec(mel_bound(T: Config))]
#[scale_info(skip_type_params(T))]
pub struct SubmissionMetadata<T: Config> {
	/// The amount of deposit that has been held in reserve.
	deposit: BalanceOf<T>,
	/// The amount of transaction fee that this submission has cost for its submitter so far.
	fee: BalanceOf<T>,
	/// The amount of rewards that we expect to give to this submission, if deemed worthy.
	reward: BalanceOf<T>,
	/// The score that this submission is claiming to achieve.
	claimed_score: ElectionScore,
	/// A bounded-bit-vec of pages that have been submitted so far.
	pages: BoundedVec<bool, T::Pages>,
}

impl<T: Config> SolutionDataProvider for Pallet<T> {
	type Solution = T::Solution;
	fn get_page(page: PageIndex) -> Option<Self::Solution> {
		// note: a non-existing page will still be treated as merely an empty page. This could be
		// re-considered.
		let current_round = Self::current_round();
		Submissions::<T>::leader(current_round).map(|(who, _score)| {
			sublog!(info, "signed", "returning page {} of {:?}'s submission as leader.", page, who);
			Submissions::<T>::get_page_of(current_round, &who, page).unwrap_or_default()
		})
	}

	fn get_score() -> Option<ElectionScore> {
		Submissions::<T>::leader(Self::current_round()).map(|(_who, score)| score)
	}

	fn report_result(result: crate::verifier::VerificationResult) {
		// assumption of the trait.
		debug_assert!(matches!(<T::Verifier as AsynchronousVerifier>::status(), Status::Nothing));
		let current_round = Self::current_round();

		match result {
			VerificationResult::Queued => {
				// defensive: if there is a result to be reported, then we must have had some
				// leader.
				if let Some((winner, metadata)) =
					Submissions::<T>::fully_take_leader(Self::current_round()).defensive()
				{
					// first, let's give them their reward.
					let reward = metadata.reward.saturating_add(metadata.fee);
					let imbalance = T::Currency::deposit_creating(&winner, reward);
					Self::deposit_event(Event::<T>::Rewarded(winner.clone(), reward));

					// then, unreserve their deposit
					let _remaining = T::Currency::unreserve(&winner, metadata.deposit);
					debug_assert!(_remaining.is_zero());

					// For now, I will wipe everything on the spot, but in ideal case, we would do
					// it over time.
					// NOTE: what we need a generic "StateGarbageCollector", to which you give a
					// bunch of storage keys, and it will clean them for you on_idle. It should just
					// be able to accept one job at a time, and report back to you if it is done
					// doing what it was doing, or not. UPDATE: no, we do them via a bit and
					// transactions.
					while let Some((discarded, metadata)) =
						Submissions::<T>::fully_take_leader(Self::current_round())
					{
						let _remaining = T::Currency::unreserve(&discarded, metadata.deposit);
						debug_assert!(_remaining.is_zero());
						Self::deposit_event(Event::<T>::Discarded(discarded));
					}

					// everything should have been clean.
					#[cfg(debug_assertions)]
					assert!(Submissions::<T>::ensure_killed(current_round).is_ok());
				}
			},
			VerificationResult::Rejected => {
				// defensive: if there is a result to be reported, then we must have had some
				// leader.
				if let Some((loser, metadata)) =
					Submissions::<T>::fully_take_leader(Self::current_round()).defensive()
				{
					// first, let's slash their deposit.
					let slash = metadata.deposit;
					let (imbalance, _remainder) = T::Currency::slash_reserved(&loser, slash);
					debug_assert!(_remainder.is_zero());
					Self::deposit_event(Event::<T>::Slashed(loser.clone(), slash));

					// inform the verifier that they can now try again, if we're still in the signed
					// validation phase.
					if crate::Pallet::<T>::current_phase().is_signed_validation() &&
						Submissions::<T>::has_leader(current_round)
					{
						// defensive: verifier just reported back a result, it must be in clear
						// state.
						let _ = <T::Verifier as AsynchronousVerifier>::start().defensive();
					}
				}
			},
			VerificationResult::DataUnavailable => {
				unreachable!("TODO")
			},
		}
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use crate::verifier::AsynchronousVerifier;
	use frame_support::{
		dispatch::DispatchResultWithPostInfo,
		pallet_prelude::{StorageDoubleMap, ValueQuery, *},
		traits::{defensive_path, Defensive, EstimateCallFee, TryCollect},
		transactional, Twox64Concat,
	};
	use frame_system::{ensure_signed, pallet_prelude::*};
	use sp_runtime::traits::Zero;

	pub trait WeightInfo {}
	impl WeightInfo for () {}

	#[pallet::config]
	#[pallet::disable_frame_system_supertrait_check]
	pub trait Config: crate::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		type Currency: ReservableCurrency<Self::AccountId>;

		type DepositBase: Get<BalanceOf<Self>>;
		type DepositPerPage: Get<BalanceOf<Self>>;

		type RewardBase: Get<BalanceOf<Self>>;

		type MaxSubmissions: Get<u32>;

		type EstimateCallFee: EstimateCallFee<Call<Self>, BalanceOf<Self>>;

		type WeightInfo: WeightInfo;
	}

	/// Wrapper type for signed submissions.
	///
	/// It handles 3 storage items:
	///
	/// 1. [`SortedScores`]: A flat vector of all submissions' `(submitter_id, claimed_score)`.
	/// 2. [`SubmissionStorage`]: Paginated map of of all submissions, keyed by submitter and page.
	/// 3. [`SubmissionMetadataStorage`]: Map from submitter to the metadata of their submission.
	///
	/// All storage items in this group are mapped, and their first key is the `round` to which they
	/// belong to. In essence, we are storing multiple versions of each group.
	///
	/// ### Invariants:
	///
	/// This storage group is sane, clean, and consistent if the following invariants are held:
	///
	/// Among the submissions of each round:
	/// - `SortedScores` should never contain duplicate account ids.
	/// - For any key in `SubmissionMetadataStorage`, a corresponding value should exist in
	/// `SortedScores` vector.
	///       - And the value of `metadata.score` must be equal to the score stored in
	///         `SortedScores`.
	/// - For any first key existing in `SubmissionStorage`, a key must exist in
	///   `SubmissionMetadataStorage`.
	/// - For any first key in `SubmissionStorage`, the number of second keys existing should be the
	///   same as the `true` count of `pages` in [`SubmissionMetadata`] (this already implies the
	///   former, since it uses the metadata).
	///
	/// All mutating functions are only allowed to transition into states where all of the above
	/// conditions are met.
	///
	/// No particular invariant exists between data that related to different rounds. They are
	/// purely independent.
	pub(crate) struct Submissions<T: Config>(sp_std::marker::PhantomData<T>);

	#[pallet::storage]
	type SortedScores<T: Config> = StorageMap<
		_,
		Twox64Concat,
		u32,
		BoundedVec<(T::AccountId, ElectionScore), T::MaxSubmissions>,
		ValueQuery,
	>;

	/// Triple map from (round, account, page) to a solution page.
	#[pallet::storage]
	type SubmissionStorage<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, u32>,
			NMapKey<Twox64Concat, T::AccountId>,
			NMapKey<Twox64Concat, PageIndex>,
		),
		T::Solution,
		OptionQuery,
	>;

	/// Map from account to the metadata of their submission.
	///
	/// invariant: for any Key1 of type `AccountId` in [`Submissions`], this storage map also has a
	/// value.
	#[pallet::storage]
	type SubmissionMetadataStorage<T: Config> =
		StorageDoubleMap<_, Twox64Concat, u32, Twox64Concat, T::AccountId, SubmissionMetadata<T>>;

	impl<T: Config> Submissions<T> {
		// -- mutating functions

		/// Generic checked mutation helper.
		///
		/// All mutating functions must be fulled through this bad boy. The round at which the
		/// mutation happens must be provided
		fn mutate_checked<R, F: FnOnce() -> R>(round: u32, mutate: F) -> R {
			let result = mutate();

			#[cfg(debug_assertions)]
			{
				assert!(Self::sanity_check_round(round).is_ok());
				assert!(Self::sanity_check_round(round + 1).is_ok());
				assert!(Self::sanity_check_round(round.saturating_sub(1)).is_ok());
			}

			result
		}

		/// *Fully* **TAKE** (i.e. get and remove) the leader from storage, with all of its
		/// associated data.
		///
		/// This removes all associated data of the leader from storage, discarding the submission
		/// data and score, returning the rest.
		pub(crate) fn fully_take_leader(
			round: u32,
		) -> Option<(T::AccountId, SubmissionMetadata<T>)> {
			Self::mutate_checked(round, || {
				SortedScores::<T>::mutate(round, |sorted| sorted.pop()).and_then(
					|(submitter, _score)| {
						SubmissionStorage::<T>::remove_prefix((round, &submitter), None);
						SubmissionMetadataStorage::<T>::take(round, &submitter)
							.map(|metadata| (submitter, metadata))
					},
				)
			})
		}

		/// Try and register a new solution.
		///
		/// Registration can only happen for the current round.
		///
		/// If a solution from `who` already exists, then it is updated to the new metadata, else it
		/// is potentially inserted. In the later case, submission might fail if the queue is
		/// already full, and the solution is not good enough to eject the weakest.
		fn try_register(
			round: u32,
			who: &T::AccountId,
			metadata: SubmissionMetadata<T>,
		) -> DispatchResultWithPostInfo {
			Self::mutate_checked(round, || Self::try_register_inner(round, who, metadata))
		}

		fn try_register_inner(
			round: u32,
			who: &T::AccountId,
			metadata: SubmissionMetadata<T>,
		) -> DispatchResultWithPostInfo {
			let mut sorted_scores = SortedScores::<T>::get(round);
			if let Some(pos) = sorted_scores.iter().position(|(x, _)| x == who) {
				// must have already existed, just update their claim.
				debug_assert!(SubmissionMetadataStorage::<T>::contains_key(round, who));
				// Note: due to the limited API of BoundedVec, using IndexMut is our only way.
				sorted_scores[pos].1 = metadata.claimed_score;
			} else {
				// must be new.
				debug_assert!(!SubmissionMetadataStorage::<T>::contains_key(round, who));

				let pos = match sorted_scores
					.binary_search_by_key(&metadata.claimed_score, |(_, y)| *y)
				{
					// an equal score exists, unlikely, but could very well happen. We just put them
					// next to each other.
					Ok(pos) => pos,
					// new score, should be inserted in this pos.
					Err(pos) => pos,
				};

				let record = (who.clone(), metadata.claimed_score);
				match sorted_scores.force_insert_keep_right(pos, record) {
					(true, None) => {},
					(true, Some((discarded, _score))) => {
						let metadata = SubmissionMetadataStorage::<T>::take(round, &discarded);
						SubmissionStorage::<T>::remove_prefix((round, &discarded), None);
						let _remaining = T::Currency::unreserve(
							&discarded,
							metadata.map(|m| m.deposit).defensive_unwrap_or_default(),
						);
						debug_assert!(_remaining.is_zero());
						Pallet::<T>::deposit_event(Event::<T>::Discarded(discarded));
					},
					(false, Some(_)) => {
						defensive_path(
							"force_insert_keep_right cannot NOT insert someone, but eject one; qed",
						);
					},
					(false, None) => return Err("too many submissions".into()),
				}
			}

			SortedScores::<T>::insert(round, sorted_scores);
			SubmissionMetadataStorage::<T>::insert(round, who, metadata);
			Ok(().into())
		}

		/// Submit a page of `solution` to the `page` index of `who`'s submission.
		///
		/// Updates the deposit in the metadata accordingly.
		///
		/// - If `maybe_solution` is `None`, then the given page is deleted.
		/// - `who` must have already registered their submission.
		/// - If the page is duplicate, it will replaced.
		pub(crate) fn try_mutate_page(
			round: u32,
			who: &T::AccountId,
			page: PageIndex,
			maybe_solution: Option<T::Solution>,
		) -> DispatchResultWithPostInfo {
			Self::mutate_checked(round, || {
				Self::try_mutate_page_inner(round, who, page, maybe_solution)
			})
		}

		fn try_mutate_page_inner(
			round: u32,
			who: &T::AccountId,
			page: PageIndex,
			maybe_solution: Option<T::Solution>,
		) -> DispatchResultWithPostInfo {
			let mut metadata =
				SubmissionMetadataStorage::<T>::get(round, who).ok_or("not registered")?;
			ensure!(page < T::Pages::get(), "bad page index");

			// defensive only: we resize `meta.pages` once to be `T::Pages` elements once, and never
			// resize it again; `page` is checked here to be in bound; element must exist; qed.
			if let Some(page_bit) = metadata.pages.get_mut(page as usize).defensive() {
				*page_bit = maybe_solution.is_some();
			}

			// update deposit.
			let new_pages: BalanceOf<T> =
				(metadata.pages.iter().filter(|x| **x).count() as u32).into();
			let new_deposit = T::DepositBase::get() + T::DepositPerPage::get() * new_pages;
			let old_deposit = metadata.deposit;
			if new_deposit > old_deposit {
				let to_reserve = new_deposit - old_deposit;
				T::Currency::reserve(who, to_reserve)?;
			} else {
				let to_unreserve = old_deposit - new_deposit;
				let _ = T::Currency::unreserve(who, to_unreserve);
			};
			metadata.deposit = new_deposit;

			SubmissionStorage::<T>::mutate_exists((round, who, page), |maybe_old_solution| {
				*maybe_old_solution = maybe_solution
			});
			SubmissionMetadataStorage::<T>::insert(round, who, metadata);
			Ok(().into())
		}

		// -- getter functions
		pub(crate) fn has_leader(round: u32) -> bool {
			!SortedScores::<T>::get(round).is_empty()
		}

		pub(crate) fn leader(round: u32) -> Option<(T::AccountId, ElectionScore)> {
			SortedScores::<T>::get(round).last().cloned()
		}

		pub(crate) fn sorted_submitters(round: u32) -> BoundedVec<T::AccountId, T::MaxSubmissions> {
			SortedScores::<T>::get(round).into_iter().map(|(x, _)| x).try_collect().unwrap()
		}

		pub(crate) fn get_page_of(
			round: u32,
			who: &T::AccountId,
			page: PageIndex,
		) -> Option<T::Solution> {
			SubmissionStorage::<T>::get((round, who, &page))
		}
	}

	#[cfg(debug_assertions)]
	impl<T: Config> Submissions<T> {
		pub fn submissions_iter(
			round: u32,
		) -> impl Iterator<Item = (T::AccountId, PageIndex, T::Solution)> {
			// TODO: ask keith if there a better way, there should be.
			SubmissionStorage::<T>::iter().filter_map(move |((r, x, y), z)| {
				if r == round {
					Some((x, y, z))
				} else {
					None
				}
			})
		}

		pub fn metadata_iter(
			round: u32,
		) -> impl Iterator<Item = (T::AccountId, SubmissionMetadata<T>)> {
			SubmissionMetadataStorage::<T>::iter_prefix(round)
		}

		/// Ensure that all the storage items associated with the given round are in `killed` state,
		/// meaning that in the expect state after an election is OVER.
		pub(crate) fn ensure_killed(round: u32) -> Result<(), &'static str> {
			ensure!(Self::metadata_iter(round).count() == 0, "metadata_iter not cleared.");
			ensure!(Self::submissions_iter(round).count() == 0, "submissions_iter not cleared.");
			ensure!(Self::sorted_submitters(round).len() == 0, "sorted_submitters not cleared.");

			Ok(())
		}

		/// Perform all the sanity checks of this storage item group at the given round.
		pub(crate) fn sanity_check_round(round: u32) -> Result<(), &'static str> {
			let sorted_scores = SortedScores::<T>::get(round);
			assert_eq!(
				sorted_scores
					.clone()
					.into_iter()
					.map(|(x, _)| x)
					.collect::<sp_std::collections::btree_set::BTreeSet<_>>()
					.len(),
				sorted_scores.len()
			);

			let _ = SubmissionMetadataStorage::<T>::iter_prefix(round)
				.map(|(submitter, meta)| {
					let mut matches = SortedScores::<T>::get(round)
						.into_iter()
						.filter(|(who, _score)| who == &submitter)
						.collect::<Vec<_>>();

					ensure!(
						matches.len() == 1,
						"item existing in metadata but missing in sorted list.",
					);

					let (_, score) = matches.pop().expect("checked; qed");
					ensure!(score == meta.claimed_score, "score mismatch");
					Ok(())
				})
				.collect::<Result<Vec<_>, &'static str>>()?;

			ensure!(
				SubmissionStorage::<T>::iter_keys()
					.filter_map(|(k0, k1, k2)| if k0 == round { Some((k1, k2)) } else { None })
					.map(|(k1, _k2)| k1)
					.all(|submitter| SubmissionMetadataStorage::<T>::contains_key(
						round, submitter
					)),
				"missing metadata of submitter"
			);

			for submitter in SubmissionStorage::<T>::iter_keys()
				.filter_map(|(k0, k1, k2)| if k0 == round { Some((k1, k2)) } else { None })
				.map(|(k1, _k2)| k1)
			{
				let pages_count =
					SubmissionStorage::<T>::iter_key_prefix((round, &submitter)).count();
				let metadata = SubmissionMetadataStorage::<T>::get(round, submitter)
					.expect("metadata checked to exist for all keys; qed");
				let assumed_pages_count = metadata.pages.iter().filter(|x| **x).count();
				ensure!(pages_count == assumed_pages_count, "wrong page count");
			}

			Ok(())
		}
	}

	#[pallet::pallet]
	pub struct Pallet<T>(PhantomData<T>);

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Upcoming submission has been registered for the given account, with the given score.
		Registered(T::AccountId, ElectionScore),
		/// A page of solution solution with the given index has been stored for the given account.
		Stored(T::AccountId, PageIndex),
		Rewarded(T::AccountId, BalanceOf<T>),
		Slashed(T::AccountId, BalanceOf<T>),
		Discarded(T::AccountId),
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Submit an upcoming solution for registration.
		///
		/// If `who` already registered, it updates it. Else, a new a entry is added, if the bound
		/// (`T::MaxSubmissions`) is not met yet.
		#[pallet::weight(0)]
		#[transactional]
		pub fn register(
			origin: OriginFor<T>,
			claimed_score: ElectionScore,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			ensure!(crate::Pallet::<T>::current_phase().is_signed(), "phase not signed");

			let deposit = T::DepositBase::get();
			let reward = T::RewardBase::get();
			let fee = Zero::zero();
			let mut pages = BoundedVec::<_, _>::with_bounded_capacity(T::Pages::get() as usize);
			pages.bounded_resize(T::Pages::get() as usize, false);

			let new_metadata = SubmissionMetadata { claimed_score, deposit, reward, fee, pages };

			T::Currency::reserve(&who, deposit).map_err(|_| "insufficient funds")?;
			let _ = Submissions::<T>::try_register(Self::current_round(), &who, new_metadata)?;
			Self::deposit_event(Event::<T>::Registered(who, claimed_score));
			Ok(().into())
		}

		/// Retract a submission.
		///
		/// Needs to pay for the removal of all associated storage items, but no string attached
		/// henceforth.
		///
		/// This should lessen the grief, but it should still be fairly expensive, because we don't
		/// want users to register empty slots and all retract at the very end.
		///
		/// Useful for when a submitted realized they have made a mistake.
		#[pallet::weight(0)]
		pub fn retract(
			origin: OriginFor<T>,
			claimed_score: ElectionScore,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			ensure!(crate::Pallet::<T>::current_phase().is_signed(), "phase not signed");
			todo!()
		}

		#[pallet::weight(0)]
		pub fn submit_page(
			origin: OriginFor<T>,
			page: PageIndex,
			maybe_solution: Option<T::Solution>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			ensure!(crate::Pallet::<T>::current_phase().is_signed(), "phase not signed");

			Submissions::<T>::try_mutate_page(Self::current_round(), &who, page, maybe_solution)?;
			Self::deposit_event(Event::<T>::Stored(who, page));

			Ok(().into())
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(now: T::BlockNumber) -> Weight {
			// TODO: we could rely on an explicit notification system instead of this.. but anyways.
			if crate::Pallet::<T>::current_phase().is_signed_validation_open_at(now) {
				let maybe_leader = Submissions::<T>::leader(Self::current_round());
				sublog!(
					info,
					"signed",
					"signed validation started, sending validation start signal? {:?}",
					maybe_leader.is_some()
				);
				// start an attempt to verify our best thing.
				if maybe_leader.is_some() {
					// defensive: signed phase has just began, verifier should be in a clear state
					// and ready to accept a solution.
					<T::Verifier as AsynchronousVerifier>::start().defensive();
				}
			}

			if crate::Pallet::<T>::current_phase().is_unsigned_open_at(now) {
				// signed validation phase just ended, make sure you stop any ongoing operation.
				sublog!(info, "signed", "signed validation ended, sending validation stop signal",);
				<T::Verifier as AsynchronousVerifier>::stop();
			}

			0
		}
	}
}

impl<T: Config> Pallet<T> {
	#[cfg(any(debug_assertions, test))]
	pub(crate) fn sanity_check() -> Result<(), &'static str> {
		Submissions::<T>::sanity_check_round(Self::current_round())
	}

	fn current_round() -> u32 {
		crate::Pallet::<T>::round()
	}
}