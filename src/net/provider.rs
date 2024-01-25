use tokio::sync::{
    mpsc::{self, error::SendError},
    oneshot,
};

use crate::interface::{
    BodiesFut, BodyProvider, BodyRequest, BroadcastFut, BroadcastProvider, ClosestPeersFut,
    HeadersFut, HeadersProvider, HeadersRequest, PeerProvider, RequestError,
};

use super::*;

/// Provider provide handful way to interact with network manager
/// Including information retrieval
#[derive(Clone)]
pub struct NetworkProvider {
    command_send: mpsc::UnboundedSender<NetworkProviderMessage>,
}

impl NetworkProvider {
    pub fn new(command_send: mpsc::UnboundedSender<NetworkProviderMessage>) -> Self {
        Self { command_send }
    }

    fn send_command(
        &self,
        cmd: NetworkProviderMessage,
    ) -> Result<(), SendError<NetworkProviderMessage>> {
        self.command_send.send(cmd)
    }

    pub async fn start_listening(&mut self, addr: Multiaddr) -> RequestResponse<()> {
        let (tx, rx) = oneshot::channel();
        self.send_command(NetworkProviderMessage::StartListening { addr, tx })
            .expect("Receiver not to be dropped.");

        rx.await.expect("Sender not to be dropped.")
    }

    /// Dial to another peer
    pub async fn dial(&self, peer_id: PeerId, peer_addr: Multiaddr) -> Result<(), RequestError> {
        let (tx, rx) = oneshot::channel();
        self.send_command(NetworkProviderMessage::Dial {
            peer_id,
            peer_addr,
            tx,
        })?;
        rx.await?
    }
    pub async fn listen_addrs(&self) -> Result<Vec<Multiaddr>, RequestError> {
        let (tx, rx) = oneshot::channel();
        self.send_command(NetworkProviderMessage::ListenAddrs { tx })?;
        rx.await?
    }
}

impl PeerProvider for NetworkProvider {
    type Output = ClosestPeersFut;
    fn get_closest_peers(&self) -> Self::Output {
        let (sender, receiver) = oneshot::channel();
        let _ = self.send_command(NetworkProviderMessage::GetClosestPeers { sender });

        Box::pin(async move { receiver.await? })
    }
}

impl HeadersProvider for NetworkProvider {
    type Output = HeadersFut;
    fn get_headers(&self, peer_id: PeerId, request: HeadersRequest) -> Self::Output {
        let (sender, receiver) = oneshot::channel();
        let _ = self.send_command(NetworkProviderMessage::GetHeaders(GetHeadersRequest {
            peer: peer_id,
            request,
            sender,
        }));

        Box::pin(async move { receiver.await? })
    }
}

impl BodyProvider for NetworkProvider {
    type Output = BodiesFut;
    fn get_bodies(&self, _request: BodyRequest) -> Self::Output {
        todo!()
    }
}

impl BroadcastProvider for NetworkProvider {
    type Output = BroadcastFut;

    fn broadcast_block_header(&self, header: Header) -> Self::Output {
        let (tx, rx) = oneshot::channel();

        self.send_command(NetworkProviderMessage::BroadcastBlockHeader { header, tx })
            .expect("Receiver not to be dropped.");

        Box::pin(async { rx.await.expect("Sender not to be dropped") })
    }

    fn broadcast_transactions(&self, transactions: Vec<Transaction>) -> Self::Output {
        let (tx, rx) = oneshot::channel();
        self.send_command(NetworkProviderMessage::BroadcastTransactions { tx, transactions })
            .expect("Receiver not to be dropped.");

        Box::pin(async { rx.await.expect("Sender not to be dropped") })
    }
}
