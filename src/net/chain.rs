use core::panic;
use std::{
    collections::{hash_map::Entry, HashMap},
    pin::Pin,
    task::{Context, Poll},
};

use futures_util::{ready, stream::FuturesUnordered, Future, FutureExt, StreamExt};
use libp2p::PeerId;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    blocktree::{AddedBlockResult, BlockTree, Error},
    interface::{
        BroadcastProvider, Header, HeadersProvider, HeadersRequest, PeerProvider, RequestResponse,
    },
    tasks::sync_header::{SyncHeaderBuilder, SyncHeaderOutput},
};

use super::{network::GetBlockHeadersResponse, ChainNetworkEvent};

pub type SyncRequestsFut = Pin<Box<dyn Future<Output = SyncHeaderOutput> + Send + 'static>>;

pub trait Provider {}

pub struct PeersRequest<P: PeerProvider> {
    header: Header,
    fut: P::Output,
}

pub struct PeersRequestOutcome {
    header: Header,
    outcome: RequestResponse<Vec<PeerId>>,
}

impl<P> Future for PeersRequest<P>
where
    P: PeerProvider,
{
    type Output = PeersRequestOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let outcome = ready!(this.fut.poll_unpin(cx));
        let header = this.header.clone();

        Poll::Ready(PeersRequestOutcome { header, outcome })
    }
}

/// Manage chain event on the network (apart from peer management)
///
/// [ChainNetworkManager] is communicate with [NetworkManager](crate::net::network::NetworkManager)
/// in both directions.
///   - receive delegated task in the form of [ChainNetworkEvent]
///   - send back result of the task
///
/// This only access to network through [NetworkProvider](crate::net::provider::NetworkProvider) as the same as other components
///
/// Also have access to [Chain](crate::blocktree::Chain) which responsible for handling block and header
pub struct ChainNetworkManager<B, P>
where
    P: PeerProvider + HeadersProvider + BroadcastProvider + Clone + Unpin + Send + Sync + 'static,
    B: BlockTree,
{
    pub chain: B,
    pub provider: P,
    pub event_recv: mpsc::UnboundedReceiver<ChainNetworkEvent>,
    // a list of sync request that is in progress
    pub sync_requests: FuturesUnordered<SyncRequestsFut>,
    pub broadcast_requests: FuturesUnordered<<P as BroadcastProvider>::Output>,
    // a request to get closest peers for sync process
    pub closest_peers_request_for_sync: Option<PeersRequest<P>>,
    // prevent redundant sync request
    pub pending_sync_request: bool,
    // prevent redundant sync of same header
    pub broadcasted_headers: HashMap<String, ()>,
}

impl<B, P> ChainNetworkManager<B, P>
where
    B: BlockTree,
    P: PeerProvider + HeadersProvider + BroadcastProvider + Clone + Unpin + Send + Sync + 'static,
{
    pub fn new(
        chain: B,
        provider: P,
        event_recv: mpsc::UnboundedReceiver<ChainNetworkEvent>,
    ) -> Self {
        Self {
            chain,
            provider,
            event_recv,
            sync_requests: Default::default(),
            broadcast_requests: Default::default(),
            closest_peers_request_for_sync: Default::default(),
            pending_sync_request: false,
            broadcasted_headers: Default::default(),
        }
    }

    // We got closest peers from network and ready to sync header
    fn send_sync_requests(&mut self, header: Header, peers: Vec<PeerId>) {
        let local_header = self
            .chain
            .tip()
            .expect("Be able to get tip if not it is fatal error.");

        for peer in peers {
            info!("Send request sync headers request to peer : {:?} Block headers start from {} to {}", peer, local_header.number+1, header.number);
            let provider = self.provider.clone();

            let syncer = SyncHeaderBuilder::default()
                .provider(provider)
                .local_head_block(local_header.number)
                .target_head(header.number)
                .peer_id(peer)
                .build();

            self.sync_requests.push(Box::pin(syncer));
        }
    }

    fn on_event(&mut self, event: ChainNetworkEvent) {
        match event {
            ChainNetworkEvent::IncomingNewBlockHeader { header } => {
                info!("Got new block header{:?}", header);
                // Incoming header will trigger sync headers and broadcast it.

                // We might in synced state and can add block header without sync process or just validate it.
                match self.chain.try_add_block_header(header.clone()) {
                    Ok(result) => {
                        match result {
                            // only broadcast if this block extend tip.
                            AddedBlockResult::ExtendTip => {
                                info!("Successfully added new block header: {:?}", header);
                                self.broadcast_requests
                                    .push(self.provider.broadcast_block_header(header.clone()));
                            }
                            AddedBlockResult::AlreadyInChain => {
                                debug!("Block header already in chain");
                            }
                        }
                    }
                    // we can't add block header cause we lack information to validate it then sync process is required
                    Err(Error::BlockHeaderNotFound) => {
                        let local_head_block = self
                            .chain
                            .tip()
                            .expect("Be able to get tip if not it is fatal error.");

                        debug!("Current tip block number: {:?}", local_head_block.number);
                        // continue to broadcast new block header even we can't validate it.
                        // base on assumption that network is truthworthy so we can leave the majority validate this broadcasted header
                        // NOTE: This will never broadcast the block that we mined twice since it will not be ahead of us.
                        match self.broadcasted_headers.entry(header.hash.clone()) {
                            Entry::Occupied(_) => {}
                            Entry::Vacant(entry) => {
                                debug!("Send broadcast request for header: {:?}", header);
                                self.broadcast_requests
                                    .push(self.provider.broadcast_block_header(header.clone()));
                                entry.insert(());
                            }
                        }

                        // consider manually track peers for more optimization i.e. we can request sync for each peer individually
                        // and discard synced request.

                        // NOTE: closest peer will be requested only one at a time, that mean we never sync header more than one at a time
                        // so later incoming header will be ignored before the one before is done (added to chain).
                        // this assume that we have trustworthy network and we can trust peer to validate header for us.

                        if self.closest_peers_request_for_sync.is_none()
                            && !self.pending_sync_request
                        {
                            debug!("Send closest peers request for header: {:?}", header);
                            self.pending_sync_request = true;
                            self.closest_peers_request_for_sync = Some(PeersRequest {
                                fut: self.provider.get_closest_peers(),
                                header,
                            })
                        }
                    }
                    // this peer is malicious and we should ban it somehow
                    Err(Error::Invalid) => {
                        warn!("Received invalid header: {:?}", header)
                    }
                    Err(e) => {
                        // we treat this as fatal error since we can't recover from this
                        panic!("Error from add block header: {e:?}")
                    }
                }
            }
            ChainNetworkEvent::IncomingTransactions { transactions: _tx } => {
                info!("new transaction received");
                //self.chain.try_add_transaction(transactions);
            }
            ChainNetworkEvent::GetBlockHeaders {
                respond,
                channel,
                request,
            } => {
                // Receiver can be dropped or channel closed and we do not try to handle it.
                let headers = self.handle_get_block_headers_request(request);
                let _ = respond.send(Ok(GetBlockHeadersResponse { headers, channel }));
            }
        }
    }

    // getting block header from chain and return in validable order (preserve order from early to latest block)
    fn handle_get_block_headers_request(&self, request: HeadersRequest) -> Vec<Header> {
        let mut headers = vec![];
        let mut number = request.start_block;

        for _ in 0..request.limit {
            match self.chain.get_header_by_block_number(number) {
                Ok(header) => {
                    number = header.number + 1;
                    headers.push(header);
                }
                Err(Error::BlockHeaderNotFound) => {
                    // request limit might exceed chain tip, we just return what we have.
                    // and wouldn't be treat as bad reputation since it not invalid header after all.
                    // more important we won't try to send missed header that lead to incorrect validation
                    break;
                }
                Err(_) => {
                    // this seem to have fatal error with database
                    panic!("error from get block header")
                }
            }
        }

        headers
    }
}

/// Intent to be spawn in a separate thread to handle network events.
impl<B, P> Future for ChainNetworkManager<B, P>
where
    B: BlockTree,
    P: PeerProvider + HeadersProvider + BroadcastProvider + Clone + Unpin + Send + Sync + 'static,
{
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();

        // We poll the event first since it is the one who drive the state here.
        while let Poll::Ready(Some(event)) = this.event_recv.poll_recv(cx) {
            this.on_event(event);
        }

        // peers request is the one who drive the sync process, so we since before sync request
        if let Some(mut request) = this.closest_peers_request_for_sync.take() {
            if let Poll::Ready(PeersRequestOutcome { header, outcome }) = request.poll_unpin(cx) {
                match outcome {
                    Ok(peers) => {
                        this.send_sync_requests(header, peers);
                    }
                    Err(e) => {
                        error!("Error while getting closest peers: {:?}", e);
                    }
                }
            } else {
                // put req back
                this.closest_peers_request_for_sync = Some(request);
            }
        }

        while let Poll::Ready(Some(sync_request)) = this.sync_requests.poll_next_unpin(cx) {
            match sync_request {
                Ok(headers) => {
                    info!("Download total {} headers", headers.len());
                    // This response is in preserve order from early to latest block.
                    // Current add block header is only accept single header at a time,
                    // so we need to properly add it to chain in correct order otherwise validation will fail.
                    let mut cnt = 0;
                    for header in headers {
                        match this.chain.try_add_block_header(header.clone()) {
                            Err(Error::Invalid) => {
                                warn!("Received invalid header: {:?}", header);
                                // we should ban this peer somehow
                                break;
                            }
                            Err(Error::BlockHeaderNotFound) => {
                                // this shouldn't be happen
                                warn!("Block header not found when sync header");
                                break;
                            }
                            Err(Error::DatabaseError(e)) => {
                                // this is fatal error
                                panic!("Database error when sync header: {:?}", e);
                            }
                            _ => {}
                        }
                        cnt += 1;
                    }
                    info!("Total valid {} headers has been added to chain.", cnt);

                    // we are done with this sync request, so we can send new sync request
                    this.pending_sync_request = false;
                }
                Err(e) => {
                    info!("Error while sync header: {:?}", e);
                }
            }
        }

        while let Poll::Ready(Some(res)) = this.broadcast_requests.poll_next_unpin(cx) {
            match res {
                Ok(_) => {}
                Err(e) => {
                    info!("Error while broadcast header: {:?}", e);
                }
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
pub mod tests {
    use once_cell::sync::Lazy;
    use pretty_assertions::assert_eq;
    use std::{
        collections::BTreeMap,
        panic,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::{
        spawn,
        sync::{mpsc::error::TryRecvError, oneshot},
        time::sleep,
    };
    use tracing::debug;

    use crate::{
        blocktree::{calculate_hash, Error},
        interface::{BroadcastFut, ClosestPeersFut, HeadersFut, HeadersRequest},
        net::{GetHeadersRequest, NetworkProviderMessage},
        TRACING,
    };

    use super::*;

    #[derive(Clone)]
    pub struct MockChain {
        pub headers: Arc<Mutex<BTreeMap<u64, Header>>>,
        pub block_add_emitter: mpsc::UnboundedSender<Header>,
    }

    impl MockChain {
        pub fn new(latest_block: u64, block_add_emitter: mpsc::UnboundedSender<Header>) -> Self {
            let initial_headers = generate_headers_from_genesis(latest_block);
            let headers = Arc::new(Mutex::new(initial_headers));
            Self {
                headers,
                block_add_emitter,
            }
        }

        pub fn validate(&mut self, parent_header: &Header, header: &Header) -> Result<(), Error> {
            if parent_header.hash != header.parent_hash {
                return Err(Error::Invalid);
            }

            if parent_header.number + 1 != header.number {
                return Err(Error::Invalid);
            }

            Ok(())
        }
    }

    impl BlockTree for MockChain {
        fn try_add_block_header(&mut self, header: Header) -> Result<AddedBlockResult, Error> {
            if header != Self::genesis() {
                let parent_header = self.get_header_by_block_number(header.number - 1)?;
                self.validate(&parent_header, &header)?;
            }

            let old_tip = self.tip()?;

            self.headers
                .lock()
                .unwrap()
                .insert(header.number, header.clone());

            if old_tip.number < self.tip()?.number {
                self.block_add_emitter.send(header.clone()).unwrap();
                return Ok(AddedBlockResult::ExtendTip);
            }
            return Ok(AddedBlockResult::AlreadyInChain);
        }

        fn get_header_by_hash(&self, _hash: String) -> Result<Header, Error> {
            Ok(Header::default())
        }

        fn get_header_by_block_number(&self, number: u64) -> Result<Header, Error> {
            if let Some(header) = self.headers.lock().unwrap().get(&number) {
                return Ok(header.clone());
            }
            Err(Error::BlockHeaderNotFound)
        }

        fn tip(&self) -> Result<Header, Error> {
            Ok(self
                .headers
                .lock()
                .unwrap()
                .last_key_value()
                .unwrap()
                .1
                .clone())
        }

        fn genesis() -> Header {
            Header {
                hash: "0x".to_string(),
                number: 0,
                nonce: 17012,
                parent_hash: "genesis".to_string(),
                state_root: "0x".to_string(),
                timestamp: 1705551177,
            }
        }
    }

    // peer client
    #[derive(Clone)]
    pub struct PeerClient {
        pub headers: BTreeMap<u64, Header>,
    }

    impl PeerClient {
        pub fn new(headers: BTreeMap<u64, Header>) -> Self {
            Self { headers }
        }
    }

    #[derive(Clone)]
    pub struct MockProvider {
        pub sender: mpsc::UnboundedSender<NetworkProviderMessage>,
        pub client_sender: mpsc::UnboundedSender<GetHeadersRequest>,
    }

    impl MockProvider {
        fn new(
            sender: mpsc::UnboundedSender<NetworkProviderMessage>,
            client_sender: mpsc::UnboundedSender<GetHeadersRequest>,
        ) -> Self {
            Self {
                sender,
                client_sender,
            }
        }
    }
    impl HeadersProvider for MockProvider {
        type Output = HeadersFut;
        fn get_headers(&self, _peer_id: PeerId, request: HeadersRequest) -> Self::Output {
            debug!(
                "request headers from [{},{}]",
                request.start_block,
                request.start_block + request.limit - 1
            );
            let (tx, rx) = oneshot::channel();
            self.client_sender
                .send(GetHeadersRequest {
                    request,
                    sender: tx,
                    peer: PeerId::random(),
                })
                .unwrap();

            Box::pin(async move { rx.await? })
        }
    }

    impl BroadcastProvider for MockProvider {
        type Output = BroadcastFut;
        fn broadcast_block_header(&self, header: Header) -> Self::Output {
            debug!("Received broadcast request for header: {:?}", header);
            let (tx, _rx) = oneshot::channel();

            // no need to wait
            self.sender
                .send(NetworkProviderMessage::BroadcastBlockHeader { tx, header })
                .expect("Receiver should be alive");
            Box::pin(async { Ok(()) })
        }

        fn broadcast_transactions(
            &self,
            _transactions: Vec<crate::interface::Transaction>,
        ) -> RequestResponse<()> {
            todo!()
        }
    }

    impl PeerProvider for MockProvider {
        type Output = ClosestPeersFut;
        fn get_closest_peers(&self) -> Self::Output {
            Box::pin(async move { Ok(vec![PeerId::random().to_owned()]) })
        }
    }

    pub fn generate_headers_from_genesis(to: u64) -> BTreeMap<u64, Header> {
        let mut headers = BTreeMap::new();
        headers.insert(0, MockChain::genesis());

        for number in 1..to + 1 {
            let state_root = "0x".to_string();
            let parent_hash = headers.get(&(number - 1)).unwrap().hash.clone();
            // deterministic timestamp
            let timestamp = 1705551177 + number * 10;
            let nonce = 17012;
            let hash = calculate_hash(number, timestamp, &parent_hash, &state_root, "0x", nonce);

            headers.insert(
                number,
                Header {
                    number,
                    hash: hex::encode(hash),
                    nonce,
                    parent_hash,
                    state_root,
                    timestamp,
                },
            );
        }
        headers
    }

    /// [from,to]
    pub fn get_header_in_interval(from: u64, to: u64) -> Vec<Header> {
        let to = to + 1;
        let initial_headers = generate_headers_from_genesis(to)
            .into_iter()
            .map(|(_, v)| v)
            .collect::<Vec<_>>();

        initial_headers[from as usize..to as usize].to_vec()
    }

    #[tokio::test]
    async fn test_handle_incoming_header() {
        Lazy::force(&TRACING);
        let external_headers = generate_headers_from_genesis(10);

        let (chain_tx, chain_rx) = mpsc::unbounded_channel();
        let (broadcast_tx, mut broadcast_rx) = mpsc::unbounded_channel();
        let (emitter, mut new_block_added_rx) = mpsc::unbounded_channel();
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();

        let chain = MockChain::new(0, emitter);
        let provider = MockProvider::new(broadcast_tx, client_tx);

        spawn(async move {
            loop {
                match client_rx.recv().await {
                    Some(GetHeadersRequest {
                        request, sender, ..
                    }) => {
                        let headers = get_header_in_interval(
                            request.start_block,
                            request.start_block + request.limit - 1,
                        );
                        sender.send(Ok(headers)).unwrap();
                    }
                    _ => {}
                }
            }
        });

        let manager = ChainNetworkManager::new(chain.clone(), provider, chain_rx);

        // advance polling for sync header process.
        spawn(async move { manager.await });

        chain_tx
            .send(ChainNetworkEvent::IncomingNewBlockHeader {
                header: external_headers.last_key_value().unwrap().1.clone(),
            })
            .unwrap();

        // check broadcast event is triggered
        match broadcast_rx.recv().await {
            Some(NetworkProviderMessage::BroadcastBlockHeader { header, .. }) => {
                assert_eq!(header, external_headers.last_key_value().unwrap().1.clone());
            }
            _ => panic!("broadcast should be ignored"),
        }

        // wait for block add event
        new_block_added_rx.recv().await;
        assert_eq!(*chain.headers.lock().unwrap(), external_headers);
    }

    #[tokio::test]
    async fn test_handle_multiple_incoming_headers() {
        Lazy::force(&TRACING);

        // this test is to make sure that we don't sync same header twice
        // cause it will consume a lot of resource
        // and waste of time
        // also make sure that broadcast is not redundant
        let external_headers = generate_headers_from_genesis(100);

        let (chain_tx, chain_rx) = mpsc::unbounded_channel();
        let (broadcast_tx, mut broadcast_rx) = mpsc::unbounded_channel();
        let (emitter, mut new_block_added_rx) = mpsc::unbounded_channel();

        let (client_tx, mut client_rx) = mpsc::unbounded_channel();

        let chain = MockChain::new(0, emitter);
        let provider = MockProvider::new(broadcast_tx, client_tx);

        let manager = ChainNetworkManager::new(chain.clone(), provider, chain_rx);

        chain_tx
            .send(ChainNetworkEvent::IncomingNewBlockHeader {
                header: external_headers.get(&50).unwrap().clone(),
            })
            .unwrap();
        chain_tx
            .send(ChainNetworkEvent::IncomingNewBlockHeader {
                header: external_headers.get(&50).unwrap().clone(),
            })
            .unwrap();
        chain_tx
            .send(ChainNetworkEvent::IncomingNewBlockHeader {
                header: external_headers.get(&52).unwrap().clone(),
            })
            .unwrap();

        // advance polling
        spawn(manager);

        sleep(Duration::from_millis(1000)).await;

        let mut req = client_rx.recv().await;
        while let Some(GetHeadersRequest {
            request, sender, ..
        }) = req.take()
        {
            let headers = get_header_in_interval(
                request.start_block,
                request.start_block + request.limit - 1,
            );
            debug!(
                "send headers from [{},{}]",
                request.start_block,
                request.start_block + request.limit - 1
            );
            sender.send(Ok(headers)).unwrap();
            req = match client_rx.try_recv() {
                Ok(req) => Some(req),
                Err(TryRecvError::Empty) => None,
                Err(e) => panic!("error from client_rx: {:?}", e),
            }
        }

        debug!("wait for block added");
        // wait for block add
        new_block_added_rx.recv().await;
        // drain block add event
        while let Ok(_) = new_block_added_rx.try_recv() {}

        // block should be sync with only one sync request, and it is the first one that we seen
        assert_eq!(
            *chain.headers.lock().unwrap(),
            generate_headers_from_genesis(50)
        );

        debug!("wait for broadcast event");

        // broadcast process should be triggered only once per hash.
        // even if we not sync it.
        match broadcast_rx.recv().await {
            Some(NetworkProviderMessage::BroadcastBlockHeader { header, .. }) => {
                assert_eq!(header, external_headers.get(&50).unwrap().clone());
            }
            _ => panic!("no broadcast event found."),
        }
        match broadcast_rx.recv().await {
            Some(NetworkProviderMessage::BroadcastBlockHeader { header, .. }) => {
                assert_eq!(header, external_headers.get(&52).unwrap().clone());
            }
            _ => panic!("no broadcast event found."),
        }
        assert_eq!(broadcast_rx.try_recv().unwrap_err(), TryRecvError::Empty);

        debug!("New incoming block");

        // Incoming block header that already in the chain should be ignored
        chain_tx
            .send(ChainNetworkEvent::IncomingNewBlockHeader {
                header: external_headers.get(&50).unwrap().clone(),
            })
            .unwrap();

        chain_tx
            .send(ChainNetworkEvent::IncomingNewBlockHeader {
                header: external_headers.last_key_value().unwrap().1.clone(),
            })
            .unwrap();

        let mut req = client_rx.recv().await;
        while let Some(GetHeadersRequest {
            request, sender, ..
        }) = req.take()
        {
            let headers = get_header_in_interval(
                request.start_block,
                request.start_block + request.limit - 1,
            );
            debug!(
                "send headers from [{},{}]",
                request.start_block,
                request.start_block + request.limit - 1
            );
            sender.send(Ok(headers)).unwrap();
            req = match client_rx.try_recv() {
                Ok(req) => Some(req),
                Err(TryRecvError::Empty) => None,
                Err(e) => panic!("error from client_rx: {:?}", e),
            }
        }

        new_block_added_rx.recv().await;
        // block should be synced with external headers now
        assert_eq!(*chain.headers.lock().unwrap(), external_headers);

        // block headers that already in the chain should be ignored
        match broadcast_rx.recv().await {
            Some(NetworkProviderMessage::BroadcastBlockHeader { header, .. }) => {
                assert_eq!(header, external_headers.last_key_value().unwrap().1.clone());
            }
            _ => panic!("no broadcast event found."),
        }

        assert_eq!(broadcast_rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[tokio::test]
    async fn test_handle_get_headers_request() {
        let chain = MockChain::new(100, mpsc::unbounded_channel().0);
        let provider = MockProvider::new(mpsc::unbounded_channel().0, mpsc::unbounded_channel().0);
        let manager = ChainNetworkManager::new(chain, provider, mpsc::unbounded_channel().1);

        let headers = manager.handle_get_block_headers_request(HeadersRequest {
            limit: 50,
            start_block: 3,
        });

        // only check on number since we heavily rely on block number not hash or other field.
        let target_headers = get_header_in_interval(3, 52);

        assert_eq!(headers.len(), 50);
        assert_eq!(headers, target_headers);

        // headers request limit exceed chain tip
        let headers = manager.handle_get_block_headers_request(HeadersRequest {
            limit: 1000,
            start_block: 1,
        });

        // get what we have
        assert_eq!(headers.len(), 100);
        assert_eq!(headers, get_header_in_interval(1, 100));

        // headers request start block exceed chain tip
        let headers = manager.handle_get_block_headers_request(HeadersRequest {
            limit: 100,
            start_block: 101,
        });
        // should get empty result
        assert_eq!(headers.len(), 0);
    }
}
