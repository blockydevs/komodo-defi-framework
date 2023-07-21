//! Contains rpc data layer structures that are not ready to become a part of the mm2_rpc::data module
//!
//! *Note: it's expected that the following data types will be moved to mm2_rpc::data when mm2 is refactored to be able to handle them*
//!

mod activation;
mod network;
mod swaps;
mod trade_preimage;

pub(crate) use activation::{ActivationRequest, GetEnabledRequest};
pub(crate) use network::{GetGossipMeshRequest, GetGossipMeshResponse, GetGossipPeerTopicsRequest,
                         GetGossipPeerTopicsResponse, GetGossipTopicPeersRequest, GetGossipTopicPeersResponse,
                         GetMyPeerIdRequest, GetMyPeerIdResponse, GetPeersInfoRequest, GetPeersInfoResponse,
                         GetRelayMeshRequest, GetRelayMeshResponse};
pub(crate) use swaps::*;
pub(crate) use trade_preimage::{MakerPreimage, MaxTakerVolRequest, MaxTakerVolResponse, MinTradingVolRequest,
                                TakerPreimage, TotalTradeFeeResponse, TradeFeeResponse, TradePreimageMethod,
                                TradePreimageRequest, TradePreimageResponse};

//TODO: @rozhkovdmitrii
