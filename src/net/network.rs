use std::{
    collections::{
        hash_map::{DefaultHasher, Entry},
        HashMap,
    },
    hash::{Hash, Hasher},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use alloy_rlp::{Decodable, Encodable};
use futures_util::{ready, stream::FuturesUnordered, Future, FutureExt};
use libp2p::{
    core::transport::ListenerId,
    futures::StreamExt,
    gossipsub::{self, ValidationMode},
    identity::Keypair,
    kad::{self, BootstrapOk, GetClosestPeersError, GetClosestPeersOk, Mode, QueryId, QueryResult},
    mdns,
    multiaddr::Protocol,
    request_response::{self, Config, OutboundRequestId, ProtocolSupport},
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
    StreamProtocol, Swarm,
};

use tokio::{
    select,
    sync::mpsc,
    time::{self, Interval},
};
use tracing::{debug, error, info, warn};

/// App behaviour implement libp2p basic component
#[derive(NetworkBehaviour)]
pub struct AppBehaviour {
    pub request_response: request_response::json::Behaviour<HeadersRequest, HeadersResponse>,
    pub mdns: Toggle<mdns::tokio::Behaviour>,
    pub gossipsub: gossipsub::Behaviour,
    pub kad: kad::Behaviour<kad::store::MemoryStore>,
}

impl AppBehaviour {
    pub fn new(keypair: Keypair, enable_mdns: bool) -> AppBehaviour {
        let peer_id = keypair.public().to_peer_id();
        let mdns = if enable_mdns {
            Some(mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id).unwrap())
        } else {
            None
        };
        let privacy = gossipsub::MessageAuthenticity::Signed(keypair);
        let message_id_fn = |message: &gossipsub::Message| {
            let mut s = DefaultHasher::new();
            message.data.hash(&mut s);
            gossipsub::MessageId::from(s.finish().to_string())
        };
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .message_id_fn(message_id_fn)
            .validation_mode(ValidationMode::Strict)
            .heartbeat_interval(std::time::Duration::from_secs(10))
            .build()
            .unwrap();

        let mut kad = kad::Behaviour::new(peer_id, kad::store::MemoryStore::new(peer_id));
        kad.set_mode(Some(Mode::Server));

        let gossipsub = gossipsub::Behaviour::new(privacy, gossipsub_config).unwrap();
        AppBehaviour {
            request_response: request_response::json::Behaviour::new(
                [(StreamProtocol::new("/chain"), ProtocolSupport::Full)],
                Config::default(),
            ),
            mdns: mdns.into(),
            gossipsub,
            kad,
        }
    }
}

#[derive(Debug)]
pub struct GetBlockHeadersResponse {
    pub headers: Vec<Header>,
    pub channel: ResponseChannel<Vec<Header>>,
}

pub struct GetBlockHeaders {
    rx: oneshot::Receiver<RequestResponse<GetBlockHeadersResponse>>,
}

impl Future for GetBlockHeaders {
    type Output = RequestResponse<GetBlockHeadersResponse>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let res = ready!(this.rx.poll_unpin(cx))?;
        Poll::Ready(res)
    }
}

/// Manage entire network and peer related event
/// being a entry point for network related event and advancing network state:
///  - Incoming header
///  - Incoming transaction (todo)
///
/// This should be treat as [Future] and spawn in separate task and run endlessly
///
/// Some task will be delate to [ChainNetworkManager](crate::net::chain::ChainNetworkManager)
///
/// The interaction mainly will be done through [NetworkProviderMessage] sent by Unbounded channel
pub struct NetworkManager {
    swarm: Swarm<AppBehaviour>,
    command_recv: mpsc::UnboundedReceiver<NetworkProviderMessage>,
    event_sender: mpsc::UnboundedSender<ChainNetworkEvent>,
    pending_request_headers: HashMap<OutboundRequestId, GetHeadersRequest>,
    pending_get_closest_peers: HashMap<QueryId, oneshot::Sender<RequestResponse<Vec<PeerId>>>>,
    pending_dial: HashMap<PeerId, oneshot::Sender<RequestResponse<()>>>,
    pending_listening: HashMap<ListenerId, oneshot::Sender<RequestResponse<()>>>,
    // might be good idea to force some number of request per peer or other rate limiting
    incoming_peer_requests: FuturesUnordered<GetBlockHeaders>,
    // this should help network fresh and avoid network partition
    bootstrap_interval: Interval,
}

use crate::interface::{Header, HeadersResponse, RequestError};

use super::*;

impl NetworkManager {
    pub fn new(
        swarm: Swarm<AppBehaviour>,
        command_recv: mpsc::UnboundedReceiver<NetworkProviderMessage>,
    ) -> (Self, mpsc::UnboundedReceiver<ChainNetworkEvent>) {
        let (event_sender, event_recv) = mpsc::unbounded_channel();

        (
            Self {
                swarm,
                command_recv,
                event_sender,
                pending_get_closest_peers: Default::default(),
                pending_request_headers: Default::default(),
                pending_dial: Default::default(),
                pending_listening: Default::default(),
                incoming_peer_requests: Default::default(),
                // this is arbitary value
                bootstrap_interval: time::interval(Duration::from_secs(60 * 5)),
            },
            event_recv,
        )
    }

    fn handle_response_block_headers(
        &mut self,
        response: RequestResponse<GetBlockHeadersResponse>,
    ) {
        match response {
            Err(e) => {
                error!("Internal failed to get block headers: {:?}", e);
            }
            Ok(GetBlockHeadersResponse { headers, channel }) => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .request_response
                    .send_response(channel, headers);
            }
        }
    }

    pub async fn run(mut self) {
        loop {
            select! {
                event = self.swarm.select_next_some() => self.handle_event(event),
                command = self.command_recv.recv() => match command {
                    Some(c) => self.handle_command(c),
                    // NetworkProviderMessage channel closed, thus shutting down the network event loop.
                    None=>  return,
                },
                response = self.incoming_peer_requests.select_next_some(), if !self.incoming_peer_requests.is_empty() => {
                    self.handle_response_block_headers(response);
                }
                _ = self.bootstrap_interval.tick() => {
                    let _ = self.swarm.behaviour_mut().kad.bootstrap();
                }
            }
        }
    }

    fn handle_command(&mut self, command: NetworkProviderMessage) {
        match command {
            NetworkProviderMessage::StartListening { addr, tx } => {
                match self.swarm.listen_on(addr.clone()) {
                    Err(e) => tx.send(Err(e.into())).expect("Reciever not dropped"),
                    Ok(listener_id) => {
                        info!("Start listening on: {:?}", addr);
                        self.pending_listening.insert(listener_id, tx);
                    }
                };
            }
            NetworkProviderMessage::ListenAddrs { tx } => {
                let mut addrs = Vec::new();
                addrs.extend(
                    self.swarm
                        .listeners()
                        .map(|addr| addr.to_owned())
                        .collect::<Vec<_>>(),
                );
                addrs.extend(self.swarm.external_addresses().map(|addr| addr.to_owned()));

                tx.send(Ok(addrs)).expect("Reciever not dropped");
            }
            NetworkProviderMessage::Dial {
                peer_id,
                peer_addr,
                tx,
            } => {
                info!("Dialing peer: {:?} @{:?}", peer_id, peer_addr);
                match self.pending_dial.entry(peer_id) {
                    Entry::Vacant(entry) => {
                        if let Err(e) = self
                            .swarm
                            .dial(peer_addr.clone().with(Protocol::P2p(peer_id)))
                        {
                            tx.send(Err(e.into())).expect("Reciever not dropped");
                        } else {
                            self.swarm
                                .behaviour_mut()
                                .kad
                                .add_address(&peer_id, peer_addr);
                            entry.insert(tx);
                        }
                    }
                    Entry::Occupied(_) => {
                        debug!("Already dial")
                    }
                }
            }
            NetworkProviderMessage::BroadcastTransactions { transactions: _ } => {
                todo!()
            }
            // Broadcast block header to the network
            // this should be called after block is validate and added to local chain
            NetworkProviderMessage::BroadcastBlockHeader { tx, header } => {
                info!("Broadcast block header: {:?}", header);
                let mut buf = Vec::new();
                header.encode(&mut buf);

                if let Err(e) = self
                    .swarm
                    .behaviour_mut()
                    .gossipsub
                    .publish(BLOCK_HEADER.clone(), buf)
                {
                    tx.send(Err(e.into())).expect("Receiver not to be dropped");
                } else {
                    tx.send(Ok(())).expect("Reciever not to be dropped");
                }
            }
            NetworkProviderMessage::GetClosestPeers { sender } => {
                debug!("cmd::GetClosestPeers");
                let local_peer_id = *self.swarm.local_peer_id();
                let query_id = self
                    .swarm
                    .behaviour_mut()
                    .kad
                    .get_closest_peers(local_peer_id);

                self.pending_get_closest_peers.insert(query_id, sender);
            }
            NetworkProviderMessage::GetHeaders(GetHeadersRequest {
                peer,
                request,
                sender,
            }) => {
                let query_id = self
                    .swarm
                    .behaviour_mut()
                    .request_response
                    .send_request(&peer, request.clone());

                self.pending_request_headers.insert(
                    query_id,
                    GetHeadersRequest {
                        peer,
                        request,
                        sender,
                    },
                );
            }
        }
    }

    fn handle_event(&mut self, event: SwarmEvent<AppBehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(AppBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                for (peer_id, multiaddr) in list {
                    info!("mDNS discovered a new peer: {peer_id}");
                    self.swarm
                        .behaviour_mut()
                        .gossipsub
                        .add_explicit_peer(&peer_id);

                    self.swarm
                        .behaviour_mut()
                        .kad
                        .add_address(&peer_id, multiaddr);
                }
            }
            SwarmEvent::Behaviour(AppBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                for (peer_id, multiaddr) in list {
                    info!("mDNS discover peer has expired: {peer_id}");
                    self.swarm
                        .behaviour_mut()
                        .gossipsub
                        .remove_explicit_peer(&peer_id);

                    self.swarm
                        .behaviour_mut()
                        .kad
                        .remove_address(&peer_id, &multiaddr);
                }
            }
            SwarmEvent::Behaviour(AppBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                propagation_source,
                message,
                ..
            })) => {
                info!("Got gossipsub message from: {}", propagation_source);
                self.handle_gossipsub_message(message);
            }
            SwarmEvent::Behaviour(AppBehaviourEvent::Kad(kad::Event::RoutingUpdated {
                peer,
                addresses,
                ..
            })) => {
                info!("Routing table updated: {:?} {:?}", peer, addresses);
            }
            SwarmEvent::Behaviour(AppBehaviourEvent::Kad(
                kad::Event::OutboundQueryProgressed {
                    id,
                    result: QueryResult::GetClosestPeers(result),
                    ..
                },
            )) => match result {
                Ok(GetClosestPeersOk { peers, .. }) => {
                    info!("Got closest peers ({}): {:?}", peers.len(), peers);
                    if let Some(sender) = self.pending_get_closest_peers.remove(&id) {
                        if sender.send(Ok(peers)).is_err() {
                            // This means the receiver has been dropped
                            // which should never happen
                            panic!("receiver dropped when request for get closest peers")
                        }
                    } else {
                        warn!("No sender for closest peers query id: {}", id);
                    }
                }
                Err(GetClosestPeersError::Timeout { key, peers, .. }) => {
                    error!("Failed to get closest peers: {:?} {:?}", key, peers);

                    if let Some(sender) = self.pending_get_closest_peers.remove(&id) {
                        if sender.send(Err(RequestError::Timeout)).is_err() {
                            // This means the receiver has been dropped
                            panic!("receiver dropped when request for get closest peers and got time out")
                        }
                    } else {
                        warn!("No sender for closest peers query id: {}", id);
                    }
                }
            },
            SwarmEvent::Behaviour(AppBehaviourEvent::Kad(
                kad::Event::OutboundQueryProgressed {
                    result: QueryResult::Bootstrap(result),
                    ..
                },
            )) => {
                info!("Bootstrap result: {:?}", result);
                if let Ok(BootstrapOk { peer, .. }) = result {
                    info!("Successfully bootstrapped with {:?}", peer);
                }
            }
            SwarmEvent::Behaviour(AppBehaviourEvent::RequestResponse(
                request_response::Event::OutboundFailure {
                    request_id, error, ..
                },
            )) => {
                if let Some(GetHeadersRequest { sender, .. }) =
                    self.pending_request_headers.remove(&request_id)
                {
                    sender.send(Err(error.into())).expect("Receiver dropped");
                } else {
                    warn!("Pending request not found for request id: {}", request_id)
                }
            }
            SwarmEvent::Behaviour(AppBehaviourEvent::RequestResponse(
                request_response::Event::ResponseSent { .. },
            )) => {}
            SwarmEvent::Behaviour(AppBehaviourEvent::RequestResponse(
                request_response::Event::Message { message, .. },
            )) => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let (respond, rx) = oneshot::channel();
                    self.event_sender
                        .send(ChainNetworkEvent::GetBlockHeaders {
                            respond,
                            request,
                            channel,
                        })
                        .expect("send event");

                    self.incoming_peer_requests.push(GetBlockHeaders { rx });
                }
                request_response::Message::Response {
                    request_id,
                    response,
                } => {
                    if let Some(GetHeadersRequest { sender, .. }) =
                        self.pending_request_headers.remove(&request_id)
                    {
                        sender.send(Ok(response)).expect("send request");
                    }
                }
            },
            SwarmEvent::IncomingConnection { .. } => {}
            SwarmEvent::IncomingConnectionError { .. } => {}
            SwarmEvent::ConnectionClosed { .. } => {}
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                info!(
                    "Connection established: {:?} endpoint: {:?}",
                    peer_id, endpoint
                );
                if endpoint.is_dialer() {
                    if let Some(sender) = self.pending_dial.remove(&peer_id) {
                        sender.send(Ok(())).expect("Reciever not dropped")
                    }
                }
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                if let Some(peer_id) = peer_id {
                    if let Some(sender) = self.pending_dial.remove(&peer_id) {
                        sender
                            .send(Err(error.into()))
                            .expect("Reciever not dropped");
                    }
                }
            }
            SwarmEvent::NewListenAddr {
                address,
                listener_id,
                ..
            } => {
                info!("New listen address: {:?}", address);
                if let Some(sender) = self.pending_listening.remove(&listener_id) {
                    sender.send(Ok(())).expect("Reciever not dropped")
                }
            }
            _ => {}
        }
    }

    fn handle_gossipsub_message(&mut self, message: gossipsub::Message) {
        if message.topic == BLOCK_HEADER.hash() {
            let header = Header::decode(&mut message.data.as_slice()).expect("decode header");
            info!("Got block header: {:?}", header);

            if self
                .event_sender
                .send(ChainNetworkEvent::IncomingNewBlockHeader { header })
                .is_err()
            {
                // This means the receiver has been dropped
                panic!("receiver dropped when sending new block header")
            }
        }

        if message.topic == TRANSACTIONS.hash() {
            let transactions = Vec::<Transaction>::decode(&mut message.data.as_slice())
                .expect("decode transactions");
            info!("Got transactions: {:?}", transactions);

            if self
                .event_sender
                .send(ChainNetworkEvent::IncomingTransactions { transactions })
                .is_err()
            {
                // This means the receiver has been dropped
                panic!("receiver dropped when sending new transactions")
            }
        }
    }
}
