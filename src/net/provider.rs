use tokio::sync::{
    mpsc::{self, error::SendError},
    oneshot,
};

use crate::interface::{
    BodiesFut, BodyProvider, BodyRequest, BroadcastProvider, ClosestPeersFut, HeadersFut,
    HeadersProvider, HeadersRequest, PeerProvider,
};

use super::*;

/// Provider provide handful way to interact with network manager
/// Including information retrieval
#[derive(Clone)]
pub struct NetworkProvider {
    command_send: mpsc::UnboundedSender<Command>,
}

impl NetworkProvider {
    pub fn new(command_send: mpsc::UnboundedSender<Command>) -> Self {
        Self { command_send }
    }

    fn send_command(&self, cmd: Command) -> Result<(), SendError<Command>> {
        self.command_send.send(cmd)
    }

    pub fn dial(&self, peer_id: PeerId, peer_addr: Multiaddr) {
        self.send_command(Command::Dial { peer_id, peer_addr })
            .expect("sender closed");
    }
}

impl PeerProvider for NetworkProvider {
    type Output = ClosestPeersFut;
    fn get_closest_peers(&self) -> Self::Output {
        let (sender, receiver) = oneshot::channel();
        let _ = self.send_command(Command::GetClosestPeers { sender });

        Box::pin(async move { receiver.await? })
    }
}

impl HeadersProvider for NetworkProvider {
    type Output = HeadersFut;
    fn get_headers(&self, peer_id: PeerId, request: HeadersRequest) -> Self::Output {
        let (sender, receiver) = oneshot::channel();
        let _ = self.send_command(Command::GetHeaders(GetHeadersRequest {
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
    fn broadcast_block_header(&self, header: Header) -> Result<(), SendError<Command>> {
        self.send_command(Command::BroadcastBlockHeader { header })
    }

    fn broadcast_transactions(
        &self,
        _transactions: Vec<Transaction>,
    ) -> Result<(), SendError<Command>> {
        todo!()
    }
}
