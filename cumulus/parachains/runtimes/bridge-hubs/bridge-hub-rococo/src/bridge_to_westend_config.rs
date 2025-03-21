// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! Bridge definitions used on BridgeHub with the Rococo flavor for bridging to BridgeHubWestend.

use crate::{
	bridge_common_config::{BridgeParachainWestendInstance, DeliveryRewardInBalance},
	weights, AccountId, BridgeWestendMessages, ParachainInfo, Runtime, RuntimeEvent, RuntimeOrigin,
	XcmRouter,
};
use bp_messages::LaneId;
use bridge_runtime_common::{
	messages,
	messages::{
		source::{FromBridgedChainMessagesDeliveryProof, TargetHeaderChainAdapter},
		target::{FromBridgedChainMessagesProof, SourceHeaderChainAdapter},
		MessageBridge, ThisChainWithMessages, UnderlyingChainProvider,
	},
	messages_xcm_extension::{
		SenderAndLane, XcmAsPlainPayload, XcmBlobHauler, XcmBlobHaulerAdapter,
		XcmBlobMessageDispatch,
	},
	refund_relayer_extension::{
		ActualFeeRefund, RefundBridgedParachainMessages, RefundSignedExtensionAdapter,
		RefundableMessagesLane, RefundableParachain,
	},
};

use codec::Encode;
use frame_support::{parameter_types, traits::PalletInfoAccess};
use sp_runtime::RuntimeDebug;
use xcm::{
	latest::prelude::*,
	prelude::{InteriorMultiLocation, NetworkId},
};
use xcm_builder::{BridgeBlobDispatcher, HaulBlobExporter};

parameter_types! {
	pub const MaxUnrewardedRelayerEntriesAtInboundLane: bp_messages::MessageNonce =
		bp_bridge_hub_rococo::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX;
	pub const MaxUnconfirmedMessagesAtInboundLane: bp_messages::MessageNonce =
		bp_bridge_hub_rococo::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX;
	pub const BridgeHubWestendChainId: bp_runtime::ChainId = bp_runtime::BRIDGE_HUB_WESTEND_CHAIN_ID;
	pub BridgeRococoToWestendMessagesPalletInstance: InteriorMultiLocation = X1(PalletInstance(<BridgeWestendMessages as PalletInfoAccess>::index() as u8));
	pub BridgeHubRococoUniversalLocation: InteriorMultiLocation = X2(GlobalConsensus(Rococo), Parachain(ParachainInfo::parachain_id().into()));
	pub WestendGlobalConsensusNetwork: NetworkId = NetworkId::Westend;
	pub ActiveOutboundLanesToBridgeHubWestend: &'static [bp_messages::LaneId] = &[XCM_LANE_FOR_ASSET_HUB_ROCOCO_TO_ASSET_HUB_WESTEND];
	pub const AssetHubRococoToAssetHubWestendMessagesLane: bp_messages::LaneId = XCM_LANE_FOR_ASSET_HUB_ROCOCO_TO_ASSET_HUB_WESTEND;
	// see the `FEE_BOOST_PER_MESSAGE` constant to get the meaning of this value
	pub PriorityBoostPerMessage: u64 = 182_044_444_444_444;

	pub AssetHubRococoParaId: cumulus_primitives_core::ParaId = bp_asset_hub_rococo::ASSET_HUB_ROCOCO_PARACHAIN_ID.into();
	pub AssetHubWestendParaId: cumulus_primitives_core::ParaId = bp_asset_hub_westend::ASSET_HUB_WESTEND_PARACHAIN_ID.into();

	pub FromAssetHubRococoToAssetHubWestendRoute: SenderAndLane = SenderAndLane::new(
		ParentThen(X1(Parachain(AssetHubRococoParaId::get().into()))).into(),
		XCM_LANE_FOR_ASSET_HUB_ROCOCO_TO_ASSET_HUB_WESTEND,
	);

	pub CongestedMessage: Xcm<()> = build_congestion_message(true).into();

	pub UncongestedMessage: Xcm<()> = build_congestion_message(false).into();
}
pub const XCM_LANE_FOR_ASSET_HUB_ROCOCO_TO_ASSET_HUB_WESTEND: LaneId = LaneId([0, 0, 0, 2]);

fn build_congestion_message<Call>(is_congested: bool) -> sp_std::vec::Vec<Instruction<Call>> {
	sp_std::vec![
		UnpaidExecution { weight_limit: Unlimited, check_origin: None },
		Transact {
			origin_kind: OriginKind::Xcm,
			require_weight_at_most:
				bp_asset_hub_rococo::XcmBridgeHubRouterTransactCallMaxWeight::get(),
			call: bp_asset_hub_rococo::Call::ToWestendXcmRouter(
				bp_asset_hub_rococo::XcmBridgeHubRouterCall::report_bridge_status {
					bridge_id: Default::default(),
					is_congested,
				}
			)
			.encode()
			.into(),
		}
	]
}

/// Proof of messages, coming from Westend.
pub type FromWestendBridgeHubMessagesProof =
	FromBridgedChainMessagesProof<bp_bridge_hub_westend::Hash>;
/// Messages delivery proof for Rococo Bridge Hub -> Westend Bridge Hub messages.
pub type ToWestendBridgeHubMessagesDeliveryProof =
	FromBridgedChainMessagesDeliveryProof<bp_bridge_hub_westend::Hash>;

/// Dispatches received XCM messages from other bridge
type FromWestendMessageBlobDispatcher = BridgeBlobDispatcher<
	XcmRouter,
	BridgeHubRococoUniversalLocation,
	BridgeRococoToWestendMessagesPalletInstance,
>;

/// Export XCM messages to be relayed to the other side
pub type ToBridgeHubWestendHaulBlobExporter = HaulBlobExporter<
	XcmBlobHaulerAdapter<ToBridgeHubWestendXcmBlobHauler>,
	WestendGlobalConsensusNetwork,
	(),
>;
pub struct ToBridgeHubWestendXcmBlobHauler;
impl XcmBlobHauler for ToBridgeHubWestendXcmBlobHauler {
	type Runtime = Runtime;
	type MessagesInstance = WithBridgeHubWestendMessagesInstance;
	type SenderAndLane = FromAssetHubRococoToAssetHubWestendRoute;

	type ToSourceChainSender = XcmRouter;
	type CongestedMessage = CongestedMessage;
	type UncongestedMessage = UncongestedMessage;
}

/// On messages delivered callback.
type OnMessagesDeliveredFromWestend = XcmBlobHaulerAdapter<ToBridgeHubWestendXcmBlobHauler>;

/// Messaging Bridge configuration for BridgeHubRococo -> BridgeHubWestend
pub struct WithBridgeHubWestendMessageBridge;
impl MessageBridge for WithBridgeHubWestendMessageBridge {
	const BRIDGED_MESSAGES_PALLET_NAME: &'static str =
		bp_bridge_hub_rococo::WITH_BRIDGE_HUB_ROCOCO_MESSAGES_PALLET_NAME;
	type ThisChain = BridgeHubRococo;
	type BridgedChain = BridgeHubWestend;
	type BridgedHeaderChain = pallet_bridge_parachains::ParachainHeaders<
		Runtime,
		BridgeParachainWestendInstance,
		bp_bridge_hub_westend::BridgeHubWestend,
	>;
}

/// Message verifier for BridgeHubWestend messages sent from BridgeHubRococo
pub type ToBridgeHubWestendMessageVerifier =
	messages::source::FromThisChainMessageVerifier<WithBridgeHubWestendMessageBridge>;

/// Maximal outbound payload size of BridgeHubRococo -> BridgeHubWestend messages.
pub type ToBridgeHubWestendMaximalOutboundPayloadSize =
	messages::source::FromThisChainMaximalOutboundPayloadSize<WithBridgeHubWestendMessageBridge>;

/// BridgeHubWestend chain from message lane point of view.
#[derive(RuntimeDebug, Clone, Copy)]
pub struct BridgeHubWestend;

impl UnderlyingChainProvider for BridgeHubWestend {
	type Chain = bp_bridge_hub_westend::BridgeHubWestend;
}

impl messages::BridgedChainWithMessages for BridgeHubWestend {}

/// BridgeHubRococo chain from message lane point of view.
#[derive(RuntimeDebug, Clone, Copy)]
pub struct BridgeHubRococo;

impl UnderlyingChainProvider for BridgeHubRococo {
	type Chain = bp_bridge_hub_rococo::BridgeHubRococo;
}

impl ThisChainWithMessages for BridgeHubRococo {
	type RuntimeOrigin = RuntimeOrigin;
}

/// Signed extension that refunds relayers that are delivering messages from the Westend parachain.
pub type OnBridgeHubRococoRefundBridgeHubWestendMessages = RefundSignedExtensionAdapter<
	RefundBridgedParachainMessages<
		Runtime,
		RefundableParachain<
			BridgeParachainWestendInstance,
			bp_bridge_hub_westend::BridgeHubWestend,
		>,
		RefundableMessagesLane<
			WithBridgeHubWestendMessagesInstance,
			AssetHubRococoToAssetHubWestendMessagesLane,
		>,
		ActualFeeRefund<Runtime>,
		PriorityBoostPerMessage,
		StrOnBridgeHubRococoRefundBridgeHubWestendMessages,
	>,
>;
bp_runtime::generate_static_str_provider!(OnBridgeHubRococoRefundBridgeHubWestendMessages);

/// Add XCM messages support for BridgeHubRococo to support Rococo->Westend XCM messages
pub type WithBridgeHubWestendMessagesInstance = pallet_bridge_messages::Instance3;
impl pallet_bridge_messages::Config<WithBridgeHubWestendMessagesInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = weights::pallet_bridge_messages::WeightInfo<Runtime>;
	type BridgedChainId = BridgeHubWestendChainId;
	type ActiveOutboundLanes = ActiveOutboundLanesToBridgeHubWestend;
	type MaxUnrewardedRelayerEntriesAtInboundLane = MaxUnrewardedRelayerEntriesAtInboundLane;
	type MaxUnconfirmedMessagesAtInboundLane = MaxUnconfirmedMessagesAtInboundLane;

	type MaximalOutboundPayloadSize = ToBridgeHubWestendMaximalOutboundPayloadSize;
	type OutboundPayload = XcmAsPlainPayload;

	type InboundPayload = XcmAsPlainPayload;
	type InboundRelayer = AccountId;
	type DeliveryPayments = ();

	type TargetHeaderChain = TargetHeaderChainAdapter<WithBridgeHubWestendMessageBridge>;
	type LaneMessageVerifier = ToBridgeHubWestendMessageVerifier;
	type DeliveryConfirmationPayments = pallet_bridge_relayers::DeliveryConfirmationPaymentsAdapter<
		Runtime,
		WithBridgeHubWestendMessagesInstance,
		DeliveryRewardInBalance,
	>;

	type SourceHeaderChain = SourceHeaderChainAdapter<WithBridgeHubWestendMessageBridge>;
	type MessageDispatch = XcmBlobMessageDispatch<
		FromWestendMessageBlobDispatcher,
		Self::WeightInfo,
		cumulus_pallet_xcmp_queue::bridging::OutXcmpChannelStatusProvider<
			AssetHubRococoParaId,
			Runtime,
		>,
	>;
	type OnMessagesDelivered = OnMessagesDeliveredFromWestend;
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::bridge_common_config::BridgeGrandpaWestendInstance;
	use bridge_runtime_common::{
		assert_complete_bridge_types,
		integrity::{
			assert_complete_bridge_constants, check_message_lane_weights,
			AssertBridgeMessagesPalletConstants, AssertBridgePalletNames, AssertChainConstants,
			AssertCompleteBridgeConstants,
		},
	};
	use parachains_common::{rococo, Balance};

	/// Every additional message in the message delivery transaction boosts its priority.
	/// So the priority of transaction with `N+1` messages is larger than priority of
	/// transaction with `N` messages by the `PriorityBoostPerMessage`.
	///
	/// Economically, it is an equivalent of adding tip to the transaction with `N` messages.
	/// The `FEE_BOOST_PER_MESSAGE` constant is the value of this tip.
	///
	/// We want this tip to be large enough (delivery transactions with more messages = less
	/// operational costs and a faster bridge), so this value should be significant.
	const FEE_BOOST_PER_MESSAGE: Balance = 2 * rococo::currency::UNITS;

	#[test]
	fn ensure_bridge_hub_rococo_message_lane_weights_are_correct() {
		check_message_lane_weights::<
			bp_bridge_hub_rococo::BridgeHubRococo,
			Runtime,
			WithBridgeHubWestendMessagesInstance,
		>(
			bp_bridge_hub_westend::EXTRA_STORAGE_PROOF_SIZE,
			bp_bridge_hub_rococo::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX,
			bp_bridge_hub_rococo::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX,
			true,
		);
	}

	#[test]
	fn ensure_bridge_integrity() {
		assert_complete_bridge_types!(
			runtime: Runtime,
			with_bridged_chain_grandpa_instance: BridgeGrandpaWestendInstance,
			with_bridged_chain_messages_instance: WithBridgeHubWestendMessagesInstance,
			bridge: WithBridgeHubWestendMessageBridge,
			this_chain: bp_rococo::Rococo,
			bridged_chain: bp_westend::Westend,
		);

		assert_complete_bridge_constants::<
			Runtime,
			BridgeGrandpaWestendInstance,
			WithBridgeHubWestendMessagesInstance,
			WithBridgeHubWestendMessageBridge,
		>(AssertCompleteBridgeConstants {
			this_chain_constants: AssertChainConstants {
				block_length: bp_bridge_hub_rococo::BlockLength::get(),
				block_weights: bp_bridge_hub_rococo::BlockWeights::get(),
			},
			messages_pallet_constants: AssertBridgeMessagesPalletConstants {
				max_unrewarded_relayers_in_bridged_confirmation_tx:
					bp_bridge_hub_westend::MAX_UNREWARDED_RELAYERS_IN_CONFIRMATION_TX,
				max_unconfirmed_messages_in_bridged_confirmation_tx:
					bp_bridge_hub_westend::MAX_UNCONFIRMED_MESSAGES_IN_CONFIRMATION_TX,
				bridged_chain_id: bp_runtime::BRIDGE_HUB_WESTEND_CHAIN_ID,
			},
			pallet_names: AssertBridgePalletNames {
				with_this_chain_messages_pallet_name:
					bp_bridge_hub_rococo::WITH_BRIDGE_HUB_ROCOCO_MESSAGES_PALLET_NAME,
				with_bridged_chain_grandpa_pallet_name:
					bp_westend::WITH_WESTEND_GRANDPA_PALLET_NAME,
				with_bridged_chain_messages_pallet_name:
					bp_bridge_hub_westend::WITH_BRIDGE_HUB_WESTEND_MESSAGES_PALLET_NAME,
			},
		});

		bridge_runtime_common::priority_calculator::ensure_priority_boost_is_sane::<
			Runtime,
			WithBridgeHubWestendMessagesInstance,
			PriorityBoostPerMessage,
		>(FEE_BOOST_PER_MESSAGE);

		assert_eq!(
			BridgeRococoToWestendMessagesPalletInstance::get(),
			X1(PalletInstance(
				bp_bridge_hub_rococo::WITH_BRIDGE_ROCOCO_TO_WESTEND_MESSAGES_PALLET_INDEX
			))
		);
	}
}
