// Copyright (C) Parity Technologies (UK) Ltd.
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

//! # OnReapIdentity Tests
//!
//! This file contains the test cases for migrating Identity data away from the Rococo Relay
//! chain and to the PeopleRococo parachain, with zero cost incurred to the user. This migration
//! is part of the broader Minimal Relay effort:
//! https://github.com/polkadot-fellows/RFCs/blob/main/text/0032-minimal-relay.md
//!
//! ## Overview
//!
//! The tests validate the robustness and correctness of the `OnReapIdentityHandler`
//! ensuring that it behaves as expected in various scenarios. Key aspects tested include:
//!
//! - **Identity Creation**: Verifying that identities can be created with the correct
//! information and handling of deposits.
//! - **Sub-Account Management**: Testing the functionality for managing sub-accounts, including
//!   creation and deposit requirements.
//! - **Cross-Chain Functionality**: Ensuring that identities can be managed seamlessly across the
//!   Rococo Relay Chain and the PeopleRococo Parachain, including testing cross-chain interactions
//!   and migrations.
//! - **Deposit Handling**: Confirming that deposits are correctly handled, reserved, and released
//!   in various scenarios, including identity reaping.
//!
//! ### Test Scenarios
//!
//! The tests are categorized into several scenarios, each focusing on different aspects of the
//! Identity Management Module:
//!
//! - Minimal identity information with varying numbers of sub-accounts.
//! - Full identity information with and without additional fields, again with varying numbers of
//!   sub-accounts.
//! - Reaping (removal) of identities and the correct release and transfer of associated deposits.

use crate::*;
use frame_support::BoundedVec;
use pallet_balances::Event as BalancesEvent;
use pallet_identity::{legacy::IdentityInfo, Data, Event as IdentityEvent};
use people_rococo_runtime::people::{
	BasicDeposit as BasicDepositParachain, ByteDeposit as ByteDepositParachain,
	IdentityInfo as IdentityInfoParachain, SubAccountDeposit as SubAccountDepositParachain,
};
use rococo_runtime::{
	BasicDeposit, ByteDeposit, MaxAdditionalFields, MaxSubAccounts, RuntimeOrigin as RococoOrigin,
	SubAccountDeposit,
};
use rococo_runtime_constants::currency::*;
use rococo_system_emulated_network::{
	rococo_emulated_chain::RococoRelayPallet, RococoRelay, RococoRelaySender,
};

type Balance = u128;
type RococoIdentity = <RococoRelay as RococoRelayPallet>::Identity;
type RococoBalances = <RococoRelay as RococoRelayPallet>::Balances;
type RococoIdentityMigrator = <RococoRelay as RococoRelayPallet>::IdentityMigrator;
type PeopleRococoIdentity = <PeopleRococo as PeopleRococoPallet>::Identity;
type PeopleRococoBalances = <PeopleRococo as PeopleRococoPallet>::Balances;

#[derive(Clone, Debug)]
struct Identity {
	relay: IdentityInfo<MaxAdditionalFields>,
	para: IdentityInfoParachain,
	subs: Subs,
}

impl Identity {
	fn new<T: frame_support::traits::Get<u32>>(
		full: bool,
		additional: Option<BoundedVec<(Data, Data), MaxAdditionalFields>>,
		subs: Subs,
	) -> Self {
		let pgp_fingerprint = [
			0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
			0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC,
		];
		let make_data = |data: &[u8], full: bool| -> Data {
			if full {
				Data::Raw(data.to_vec().try_into().unwrap())
			} else {
				Data::None
			}
		};
		let (github, discord) = additional
			.as_ref()
			.and_then(|vec| vec.get(0))
			.map(|(g, d)| (g.clone(), d.clone()))
			.unwrap_or((Data::None, Data::None));
		Self {
			relay: IdentityInfo {
				display: make_data(b"xcm-test", full),
				legal: make_data(b"The Xcm Test, Esq.", full),
				web: make_data(b"https://xcm-test.io", full),
				riot: make_data(b"xcm-riot", full),
				email: make_data(b"xcm-test@gmail.com", full),
				pgp_fingerprint: Some(pgp_fingerprint),
				image: make_data(b"xcm-test.png", full),
				twitter: make_data(b"@xcm-test", full),
				additional: additional.unwrap_or_else(|| BoundedVec::default()),
			},
			para: IdentityInfoParachain {
				display: make_data(b"xcm-test", full),
				legal: make_data(b"The Xcm Test, Esq.", full),
				web: make_data(b"https://xcm-test.io", full),
				matrix: make_data(b"xcm-matrix@server", full),
				email: make_data(b"xcm-test@gmail.com", full),
				pgp_fingerprint: Some(pgp_fingerprint),
				image: make_data(b"xcm-test.png", full),
				twitter: make_data(b"@xcm-test", full),
				github,
				discord,
			},
			subs,
		}
	}
}

#[derive(Clone, Debug)]
enum Subs {
	Zero,
	Many(u32),
}

enum IdentityOn<'a> {
	Relay(&'a IdentityInfo<MaxAdditionalFields>),
	Para(&'a IdentityInfoParachain),
}

impl IdentityOn<'_> {
	fn calculate_deposit(self) -> Balance {
		match self {
			IdentityOn::Relay(id) => {
				let base_deposit = BasicDeposit::get();
				let byte_deposit =
					ByteDeposit::get() * TryInto::<Balance>::try_into(id.encoded_size()).unwrap();
				base_deposit + byte_deposit
			},
			IdentityOn::Para(id) => {
				let base_deposit = BasicDepositParachain::get();
				let byte_deposit = ByteDepositParachain::get() *
					TryInto::<Balance>::try_into(id.encoded_size()).unwrap();
				base_deposit + byte_deposit
			},
		}
	}
}

/// Generate an `AccountId32` from a `u32`.
/// This creates a 32-byte array, initially filled with `255`, and then repeatedly fills it
/// with the 4-byte little-endian representation of the `u32` value, until the array is full.
///
/// **Example**:
///
/// `account_from_u32(5)` will return an `AccountId32` with the bytes
/// `[0, 5, 0, 0, 0, 0, 0, 0, 0, 5 ... ]` (repeated 8 times)
///
/// **Note**
///
/// This is only used for testing and is not intended for production use.
fn account_from_u32(id: u32) -> AccountId32 {
	let mut buffer = [255u8; 32];
	let id_bytes = id.to_le_bytes();
	let id_size = id_bytes.len();
	for ii in 0..buffer.len() / id_size {
		let start = ii * id_size;
		let end = start + id_size;
		buffer[start..end].clone_from_slice(&id_bytes[..]);
	}
	AccountId32::new(buffer)
}

// Set up the Relay Chain with an identity.
fn set_id_relay(id: &Identity) -> Balance {
	let mut total_deposit: Balance = 0;

	// Set identity and Subs on Relay Chain
	RococoRelay::execute_with(|| {
		type RuntimeEvent = <RococoRelay as Chain>::RuntimeEvent;

		assert_ok!(RococoIdentity::set_identity(
			RococoOrigin::signed(RococoRelaySender::get()),
			Box::new(id.relay.clone())
		));

		match id.subs {
			Subs::Zero => {},
			Subs::Many(n) => {
				let mut subs = Vec::with_capacity(n.try_into().unwrap());
				for ii in 0..n {
					subs.push((
						account_from_u32(ii),
						Data::Raw(b"name".to_vec().try_into().unwrap()),
					));
				}
				assert_ok!(RococoIdentity::set_subs(
					RococoOrigin::signed(RococoRelaySender::get()),
					subs,
				));
			},
		}

		let reserved_balance = RococoBalances::reserved_balance(RococoRelaySender::get());
		let id_deposit = IdentityOn::Relay(&id.relay).calculate_deposit();

		match id.subs {
			Subs::Zero => {
				total_deposit = id_deposit; // No subs
				assert_expected_events!(
					RococoRelay,
					vec![
						RuntimeEvent::Identity(IdentityEvent::IdentitySet { .. }) => {},
						RuntimeEvent::Balances(BalancesEvent::Reserved { who, amount }) => {
							who: *who == RococoRelaySender::get(),
							amount: *amount == id_deposit,
						},
					]
				);
			},
			Subs::Many(n) => {
				let sub_account_deposit = n as Balance * SubAccountDeposit::get();
				total_deposit =
					sub_account_deposit + IdentityOn::Relay(&id.relay).calculate_deposit();
				assert_expected_events!(
					RococoRelay,
					vec![
						RuntimeEvent::Identity(IdentityEvent::IdentitySet { .. }) => {},
						RuntimeEvent::Balances(BalancesEvent::Reserved { who, amount }) => {
							who: *who == RococoRelaySender::get(),
							amount: *amount == id_deposit,
						},
						RuntimeEvent::Balances(BalancesEvent::Reserved { who, amount }) => {
							who: *who == RococoRelaySender::get(),
							amount: *amount == sub_account_deposit,
						},
					]
				);
			},
		}

		assert_eq!(reserved_balance, total_deposit);
	});
	total_deposit
}

// Set up the parachain with an identity and (maybe) sub accounts, but with zero deposits.
fn assert_set_id_parachain(id: &Identity) {
	// Set identity and Subs on Parachain with zero deposit
	PeopleRococo::execute_with(|| {
		let free_bal = PeopleRococoBalances::free_balance(PeopleRococoSender::get());
		let reserved_balance = PeopleRococoBalances::reserved_balance(PeopleRococoSender::get());

		//total balance at Genesis should be zero
		assert_eq!(reserved_balance + free_bal, 0);

		assert_ok!(PeopleRococoIdentity::set_identity_no_deposit(
			&PeopleRococoSender::get(),
			id.para.clone(),
		));

		match id.subs {
			Subs::Zero => {},
			Subs::Many(n) => {
				let mut subs = Vec::with_capacity(n.try_into().unwrap());
				for ii in 0..n {
					subs.push((
						account_from_u32(ii),
						Data::Raw(b"name".to_vec().try_into().unwrap()),
					));
				}
				assert_ok!(PeopleRococoIdentity::set_subs_no_deposit(
					&PeopleRococoSender::get(),
					subs,
				));
			},
		}

		// No amount should be reserved as deposit amounts are set to 0.
		let reserved_balance = PeopleRococoBalances::reserved_balance(PeopleRococoSender::get());
		assert_eq!(reserved_balance, 0);
		assert!(PeopleRococoIdentity::identity(&PeopleRococoSender::get()).is_some());

		let (_, sub_accounts) = PeopleRococoIdentity::subs_of(&PeopleRococoSender::get());

		match id.subs {
			Subs::Zero => assert_eq!(sub_accounts.len(), 0),
			Subs::Many(n) => assert_eq!(sub_accounts.len(), n as usize),
		}
	});
}

// Reap the identity on the Relay Chain and assert that the correct things happen there.
fn assert_reap_id_relay(total_deposit: Balance, id: &Identity) {
	RococoRelay::execute_with(|| {
		type RuntimeEvent = <RococoRelay as Chain>::RuntimeEvent;
		let free_bal_before_reap = RococoBalances::free_balance(RococoRelaySender::get());
		let reserved_balance = RococoBalances::reserved_balance(RococoRelaySender::get());

		assert_eq!(reserved_balance, total_deposit);

		assert_ok!(RococoIdentityMigrator::reap_identity(
			RococoOrigin::root(),
			RococoRelaySender::get()
		));

		let remote_deposit = match id.subs {
			Subs::Zero => calculate_remote_deposit(id.relay.encoded_size() as u32, 0),
			Subs::Many(n) => calculate_remote_deposit(id.relay.encoded_size() as u32, n),
		};

		assert_expected_events!(
			RococoRelay,
			vec![
				// `reap_identity` sums the identity and subs deposits and unreserves them in one
				// call. Therefore, we only expect one `Unreserved` event.
				RuntimeEvent::Balances(BalancesEvent::Unreserved { who, amount }) => {
					who: *who == RococoRelaySender::get(),
					amount: *amount == total_deposit,
				},
				RuntimeEvent::IdentityMigrator(
					polkadot_runtime_common::identity_migrator::Event::IdentityReaped {
						who,
					}) => {
					who: *who == PeopleRococoSender::get(),
				},
			]
		);
		// Identity should be gone.
		assert!(PeopleRococoIdentity::identity(&RococoRelaySender::get()).is_none());

		// Subs should be gone.
		let (_, sub_accounts) = RococoIdentity::subs_of(&RococoRelaySender::get());
		assert_eq!(sub_accounts.len(), 0);

		let reserved_balance = RococoBalances::reserved_balance(RococoRelaySender::get());
		assert_eq!(reserved_balance, 0);

		// Free balance should be greater (i.e. the teleport should work even if 100% of an
		// account's balance is reserved for Identity).
		let free_bal_after_reap = RococoBalances::free_balance(RococoRelaySender::get());
		assert!(free_bal_after_reap > free_bal_before_reap);

		// Implicit: total_deposit > remote_deposit. As in, accounts should always have enough
		// reserved for the parachain deposit.
		assert_eq!(free_bal_after_reap, free_bal_before_reap + total_deposit - remote_deposit);
	});
}

// Reaping the identity on the Relay Chain will have sent an XCM program to the parachain. Ensure
// that everything happens as expected.
fn assert_reap_parachain(id: &Identity) {
	PeopleRococo::execute_with(|| {
		let reserved_balance = PeopleRococoBalances::reserved_balance(PeopleRococoSender::get());
		let id_deposit = IdentityOn::Para(&id.para).calculate_deposit();
		let total_deposit = match id.subs {
			Subs::Zero => id_deposit,
			Subs::Many(n) => id_deposit + n as Balance * SubAccountDepositParachain::get(),
		};
		assert_reap_events(id_deposit, id);
		assert_eq!(reserved_balance, total_deposit);

		// Should have at least one ED after in free balance after the reap.
		assert!(PeopleRococoBalances::free_balance(PeopleRococoSender::get()) >= PEOPLE_ROCOCO_ED);
	});
}

// Assert the events that should happen on the parachain upon reaping an identity on the Relay
// Chain.
fn assert_reap_events(id_deposit: Balance, id: &Identity) {
	type RuntimeEvent = <PeopleRococo as Chain>::RuntimeEvent;
	match id.subs {
		Subs::Zero => {
			assert_expected_events!(
				PeopleRococo,
				vec![
					// Deposit and Endowed from teleport
					RuntimeEvent::Balances(BalancesEvent::Deposit { .. }) => {},
					RuntimeEvent::Balances(BalancesEvent::Endowed { .. }) => {},
					// Amount reserved for identity info
					RuntimeEvent::Balances(BalancesEvent::Reserved { who, amount }) => {
						who: *who == PeopleRococoSender::get(),
						amount: *amount == id_deposit,
					},
					// Confirmation from Migrator with individual identity and subs deposits
					RuntimeEvent::IdentityMigrator(
						polkadot_runtime_common::identity_migrator::Event::DepositUpdated {
							who, identity, subs
						}) => {
						who: *who == PeopleRococoSender::get(),
						identity: *identity == id_deposit,
						subs: *subs == 0,
					},
					RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { ..}) => {},
				]
			);
		},
		Subs::Many(n) => {
			let subs_deposit = n as Balance * SubAccountDepositParachain::get();
			assert_expected_events!(
				PeopleRococo,
				vec![
					// Deposit and Endowed from teleport
					RuntimeEvent::Balances(BalancesEvent::Deposit { .. }) => {},
					RuntimeEvent::Balances(BalancesEvent::Endowed { .. }) => {},
					// Amount reserved for identity info
					RuntimeEvent::Balances(BalancesEvent::Reserved { who, amount }) => {
						who: *who == PeopleRococoSender::get(),
						amount: *amount == id_deposit,
					},
					// Amount reserved for subs
					RuntimeEvent::Balances(BalancesEvent::Reserved { who, amount }) => {
						who: *who == PeopleRococoSender::get(),
						amount: *amount == subs_deposit,
					},
					// Confirmation from Migrator with individual identity and subs deposits
					RuntimeEvent::IdentityMigrator(
						polkadot_runtime_common::identity_migrator::Event::DepositUpdated {
							who, identity, subs
						}) => {
						who: *who == PeopleRococoSender::get(),
						identity: *identity == id_deposit,
						subs: *subs == subs_deposit,
					},
					RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { ..}) => {},
				]
			);
		},
	};
}

// Duplicate of the impl of `ToParachainIdentityReaper` in the Rococo runtime.
fn calculate_remote_deposit(bytes: u32, subs: u32) -> Balance {
	// Note: These `deposit` functions and `EXISTENTIAL_DEPOSIT` correspond to the Relay Chain's.
	// Pulled in: use rococo_runtime_constants::currency::*;
	let para_basic_deposit = deposit(1, 17) / 100;
	let para_byte_deposit = deposit(0, 1) / 100;
	let para_sub_account_deposit = deposit(1, 53) / 100;
	let para_existential_deposit = EXISTENTIAL_DEPOSIT / 10;

	// pallet deposits
	let id_deposit =
		para_basic_deposit.saturating_add(para_byte_deposit.saturating_mul(bytes as Balance));
	let subs_deposit = para_sub_account_deposit.saturating_mul(subs as Balance);

	id_deposit
		.saturating_add(subs_deposit)
		.saturating_add(para_existential_deposit.saturating_mul(2))
}

// Represent some `additional` data that would not be migrated to the parachain. The encoded size,
// and thus the byte deposit, should decrease.
fn nonsensical_additional() -> BoundedVec<(Data, Data), MaxAdditionalFields> {
	BoundedVec::try_from(vec![(
		Data::Raw(b"fOo".to_vec().try_into().unwrap()),
		Data::Raw(b"baR".to_vec().try_into().unwrap()),
	)])
	.unwrap()
}

// Represent some `additional` data that will be migrated to the parachain as first-class fields.
fn meaningful_additional() -> BoundedVec<(Data, Data), MaxAdditionalFields> {
	BoundedVec::try_from(vec![
		(
			Data::Raw(b"github".to_vec().try_into().unwrap()),
			Data::Raw(b"niels-username".to_vec().try_into().unwrap()),
		),
		(
			Data::Raw(b"discord".to_vec().try_into().unwrap()),
			Data::Raw(b"bohr-username".to_vec().try_into().unwrap()),
		),
	])
	.unwrap()
}

// Execute a single test case.
fn assert_relay_para_flow(id: &Identity) {
	let total_deposit = set_id_relay(id);
	assert_set_id_parachain(id);
	assert_reap_id_relay(total_deposit, id);
	assert_reap_parachain(id);
}

// Tests with empty `IdentityInfo`.

#[test]
fn on_reap_identity_works_for_minimal_identity_with_zero_subs() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(false, None, Subs::Zero));
}

#[test]
fn on_reap_identity_works_for_minimal_identity() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(false, None, Subs::Many(1)));
}

#[test]
fn on_reap_identity_works_for_minimal_identity_with_max_subs() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(
		false,
		None,
		Subs::Many(MaxSubAccounts::get()),
	));
}

// Tests with full `IdentityInfo`.

#[test]
fn on_reap_identity_works_for_full_identity_no_additional_zero_subs() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(true, None, Subs::Zero));
}

#[test]
fn on_reap_identity_works_for_full_identity_no_additional() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(true, None, Subs::Many(1)));
}

#[test]
fn on_reap_identity_works_for_full_identity_no_additional_max_subs() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(
		true,
		None,
		Subs::Many(MaxSubAccounts::get()),
	));
}

// Tests with full `IdentityInfo` and `additional` fields that will _not_ be migrated.

#[test]
fn on_reap_identity_works_for_full_identity_nonsense_additional_zero_subs() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(
		true,
		Some(nonsensical_additional()),
		Subs::Zero,
	));
}

#[test]
fn on_reap_identity_works_for_full_identity_nonsense_additional() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(
		true,
		Some(nonsensical_additional()),
		Subs::Many(1),
	));
}

#[test]
fn on_reap_identity_works_for_full_identity_nonsense_additional_max_subs() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(
		true,
		Some(nonsensical_additional()),
		Subs::Many(MaxSubAccounts::get()),
	));
}

// Tests with full `IdentityInfo` and `additional` fields that will be migrated.

#[test]
fn on_reap_identity_works_for_full_identity_meaningful_additional_zero_subs() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(
		true,
		Some(meaningful_additional()),
		Subs::Zero,
	));
}

#[test]
fn on_reap_identity_works_for_full_identity_meaningful_additional() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(
		true,
		Some(meaningful_additional()),
		Subs::Many(1),
	));
}

#[test]
fn on_reap_identity_works_for_full_identity_meaningful_additional_max_subs() {
	assert_relay_para_flow(&Identity::new::<MaxAdditionalFields>(
		true,
		Some(meaningful_additional()),
		Subs::Many(MaxSubAccounts::get()),
	));
}
