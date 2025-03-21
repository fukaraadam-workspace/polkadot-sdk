// Copyright (C) Parity Technologies (UK) Ltd.
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

use crate::configuration::*;
use frame_benchmarking::{benchmarks, BenchmarkError, BenchmarkResult};
use frame_system::RawOrigin;
use primitives::{ExecutorParam, ExecutorParams, PvfExecTimeoutKind, PvfPrepTimeoutKind};
use sp_runtime::traits::One;

benchmarks! {
	set_config_with_block_number {}: set_code_retention_period(RawOrigin::Root, One::one())

	set_config_with_u32 {}: set_max_code_size(RawOrigin::Root, 100)

	set_config_with_option_u32 {}: set_max_validators(RawOrigin::Root, Some(10))

	set_hrmp_open_request_ttl {}: {
		Err(BenchmarkError::Override(
			BenchmarkResult::from_weight(T::BlockWeights::get().max_block)
		))?;
	}

	set_config_with_balance {}: set_hrmp_sender_deposit(RawOrigin::Root, 100_000_000_000)

	set_config_with_executor_params {}: set_executor_params(RawOrigin::Root, ExecutorParams::from(&[
		ExecutorParam::MaxMemoryPages(2080),
		ExecutorParam::StackLogicalMax(65536),
		ExecutorParam::StackNativeMax(256 * 1024 * 1024),
		ExecutorParam::WasmExtBulkMemory,
		ExecutorParam::PrecheckingMaxMemory(2 * 1024 * 1024 * 1024),
		ExecutorParam::PvfPrepTimeout(PvfPrepTimeoutKind::Precheck, 60_000),
		ExecutorParam::PvfPrepTimeout(PvfPrepTimeoutKind::Lenient, 360_000),
		ExecutorParam::PvfExecTimeout(PvfExecTimeoutKind::Backing, 2_000),
		ExecutorParam::PvfExecTimeout(PvfExecTimeoutKind::Approval, 12_000),
	][..]))

	set_config_with_perbill {}: set_on_demand_fee_variability(RawOrigin::Root, Perbill::from_percent(100))

	set_node_feature{}: set_node_feature(RawOrigin::Root, 255, true)

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::new_test_ext(Default::default()),
		crate::mock::Test
	);
}
