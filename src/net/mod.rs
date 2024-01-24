use libp2p::{gossipsub::IdentTopic, request_response::ResponseChannel, Multiaddr, PeerId};
use once_cell::sync::Lazy;
use tokio::sync::oneshot;

use crate::interface::{Header, HeadersRequest, HeadersResponse, RequestResponse, Transaction};

use self::network::GetBlockHeadersResponse;

pub mod chain;
pub mod network;
pub mod provider;

/// Events that can be produced by the network.
/// And delegate to chain network management.
pub enum ChainNetworkEvent {
    /// A new block header has been received from the network.
    IncomingNewBlockHeader { header: Header },
    /// Receving a new transactions from the network.
    IncomingTransactions { transactions: Vec<Transaction> },
    /// Block Request
    GetBlockHeaders {
        respond: oneshot::Sender<RequestResponse<GetBlockHeadersResponse>>,
        channel: ResponseChannel<Vec<Header>>,
        request: HeadersRequest,
    },
}

#[derive(Debug)]
pub struct GetHeadersRequest {
    peer: PeerId,
    request: HeadersRequest,
    sender: oneshot::Sender<RequestResponse<HeadersResponse>>,
}

/// Message send to [`NetworkProvider`]
#[derive(Debug)]
pub enum NetworkProviderMessage {
    /// Request for closest peers
    GetClosestPeers {
        sender: oneshot::Sender<RequestResponse<Vec<PeerId>>>,
    },
    BroadcastBlockHeader {
        header: Header,
        tx: oneshot::Sender<RequestResponse<()>>,
    },
    BroadcastTransactions {
        transactions: Vec<Transaction>,
    },
    GetHeaders(GetHeadersRequest),
    Dial {
        peer_id: PeerId,
        peer_addr: Multiaddr,
        tx: oneshot::Sender<RequestResponse<()>>,
    },
    ListenAddrs {
        tx: oneshot::Sender<RequestResponse<Vec<Multiaddr>>>,
    },
    StartListening {
        addr: Multiaddr,
        tx: oneshot::Sender<RequestResponse<()>>,
    },
}

pub static BLOCK_HEADER: Lazy<IdentTopic> =
    Lazy::new(|| IdentTopic::new("/lightlayer/block_header"));
pub static TRANSACTIONS: Lazy<IdentTopic> =
    Lazy::new(|| IdentTopic::new("/lightlayer/transactions"));
