use std::pin::Pin;

use alloy_rlp::{RlpDecodable, RlpEncodable};
use futures_util::{future::BoxFuture, Future};
use libp2p::{request_response::OutboundFailure, PeerId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{
    mpsc::{self, error::SendError},
    oneshot,
};

use crate::net::Command;

#[derive(
    Serialize, Deserialize, PartialEq, Eq, RlpDecodable, RlpEncodable, Debug, Clone, Default,
)]
pub struct Header {
    pub number: u64,
    pub parent_hash: String,
    pub state_root: String,
    pub timestamp: u64,
    pub nonce: u64,
    pub hash: String,
}

#[derive(Serialize, Deserialize, RlpDecodable, RlpEncodable, Debug, Clone)]
pub struct Transaction {
    pub block_number: u64,
    pub from: String,
    pub to: String,
    pub value: u64,
    pub nonce: u64,
}

#[derive(Serialize, Deserialize, RlpDecodable, RlpEncodable, Debug, Clone)]
pub struct Block {
    pub header: Header,
    pub transactions: Vec<Transaction>,
}

/// Request for block header
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HeadersRequest {
    /// limit number of header in single request
    pub limit: u64,
    /// a block which we want to start syncing from
    pub start_block: u64,
}

pub type RequestResponse<T> = Result<T, RequestError>;

pub type HeadersResponse = Vec<Header>;

/// Furure that will return headers
pub type HeadersFut = Pin<Box<dyn Future<Output = RequestResponse<HeadersResponse>> + Sync + Send>>;

pub type ClosestPeersFut =
    Pin<Box<dyn Future<Output = RequestResponse<Vec<PeerId>>> + Sync + Send>>;

pub trait PeerProvider {
    type Output: Future<Output = RequestResponse<Vec<PeerId>>> + Sync + Send + Unpin;
    fn get_closest_peers(&self) -> Self::Output;
}

/// Provider that can provide headers
pub trait HeadersProvider {
    type Output: Future<Output = RequestResponse<HeadersResponse>> + Sync + Send + Unpin;
    fn get_headers(&self, peer: PeerId, request: HeadersRequest) -> Self::Output;
}

/// Request for block body
pub struct BodyRequest {
    /// Represent block hash to request
    pub hashes: Vec<String>,
}

/// Furure that will return block bodies
pub type BodiesFut = BoxFuture<'static, Vec<Block>>;

/// Provider that can provide bodies
pub trait BodyProvider {
    type Output: Future<Output = Vec<Block>>;
    fn get_bodies(&self, request: BodyRequest) -> Self::Output;
}

pub trait BroadcastProvider {
    fn broadcast_block_header(&self, header: Header) -> Result<(), SendError<Command>>;

    fn broadcast_transactions(
        &self,
        transactions: Vec<Transaction>,
    ) -> Result<(), SendError<Command>>;
}

/// Error variants that can happen when sending requests to network manager.
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum RequestError {
    #[error("Channel closed.")]
    ChannelClosed,
    #[error("Timeout while waiting for response.")]
    Timeout,
    #[error("Outbound network failure.")]
    OutboundFailure(#[from] OutboundFailure),
}

impl<T> From<mpsc::error::SendError<T>> for RequestError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        RequestError::ChannelClosed
    }
}

impl From<oneshot::error::RecvError> for RequestError {
    fn from(_: oneshot::error::RecvError) -> Self {
        RequestError::ChannelClosed
    }
}
