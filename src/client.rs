use libp2p::{identity::Keypair, multiaddr::Protocol, Multiaddr, SwarmBuilder};
use std::{path::PathBuf, time::Duration};

use tokio::{spawn, sync::mpsc};

use crate::{
    blocktree::Chain,
    net::{
        chain::ChainNetworkManager,
        network::{AppBehaviour, NetworkManager},
        provider::NetworkProvider,
        BLOCK_HEADER, TRANSACTIONS,
    },
    pool::PoolTx,
    rpc::RpcServerBuilder,
};

/// Client is the main entry point for the application
/// Its responsible for initializing the database, network, rpc server
/// Including block sync, block production, block validation (omit for now)
pub struct ClientBuilder {
    db_path: PathBuf,
    auto_mine_interval: Option<u64>,
    keypair: Keypair,
    listening_addrs: Vec<Multiaddr>,
    enabled_mdns: bool,
}
impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("./db"),
            auto_mine_interval: None,
            listening_addrs: vec![],
            keypair: Keypair::generate_ed25519(),
            enabled_mdns: false,
        }
    }
}

impl ClientBuilder {
    pub fn with_db_path(mut self, path: PathBuf) -> Self {
        self.db_path = path;
        self
    }

    pub fn with_auto_mine_interval(mut self, interval: Option<u64>) -> Self {
        self.auto_mine_interval = interval;
        self
    }

    pub fn with_listening_addrs(mut self, listening_addrs: Vec<Multiaddr>) -> Self {
        self.listening_addrs = listening_addrs;
        self
    }

    pub fn with_keypair(mut self, keypair: Keypair) -> Self {
        self.keypair = keypair;
        self
    }

    pub fn with_mdns(mut self, enabled: bool) -> Self {
        self.enabled_mdns = enabled;
        self
    }

    pub async fn execute_with_seed_nodes(mut self, seed_nodes: Vec<Multiaddr>) {
        let (blockchain, _pool, mut provider, h1, h2) = self.start_network();

        if self.listening_addrs.is_empty() {
            provider
                .start_listening("/ip4/0.0.0.0/tcp/0".parse().expect("parse addr to work"))
                .await
                .expect("Listening not to fail.");
        } else {
            for addr in self.listening_addrs {
                provider
                    .start_listening(addr)
                    .await
                    .expect("Listening not to fail.");
            }
        }

        for addr in seed_nodes {
            let peer_id = match addr.iter().last() {
                Some(Protocol::P2p(peer_id)) => peer_id,
                _ => panic!("peer id should exist"),
            };
            provider.dial(peer_id, addr).await.expect("dial peer");
        }

        let mut server = RpcServerBuilder::default().with_chain(blockchain).build();
        let h3 = spawn(async move {
            let handle = server.start_server().await;
            handle.stopped().await;
        });

        let _ = tokio::join!(h1, h2, h3);
    }

    /// Our network is based on libp2p
    /// And working around [NetworkManager] which responsible for handling peers and networking
    /// [NetworkManager] will delegate task relate to chain network to [ChainNetworkManager]
    /// All interaction to [NetworkManager] is done through [NetworkProvider]
    fn start_network(
        &mut self,
    ) -> (
        Chain<NetworkProvider>,
        PoolTx<NetworkProvider>,
        NetworkProvider,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let mut swarm = SwarmBuilder::with_existing_identity(self.keypair.clone())
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )
            .unwrap()
            .with_behaviour(|key| AppBehaviour::new(key.clone(), self.enabled_mdns))
            .unwrap()
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&BLOCK_HEADER)
            .unwrap();
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&TRANSACTIONS)
            .unwrap();

        let (command_send, command_recv) = mpsc::unbounded_channel();
        let provider = NetworkProvider::new(command_send);
        let blockchain = Chain::new(
            self.db_path.as_path(),
            self.auto_mine_interval,
            provider.clone(),
        );

        let (network, chain_manager_rx) = NetworkManager::new(swarm, command_recv);
        let pool = PoolTx::new(provider.clone());
        let chain_manager = ChainNetworkManager::new(
            blockchain.clone(),
            pool.clone(),
            provider.clone(),
            chain_manager_rx,
        );

        let handle1 = spawn(network.run());
        let handle2 = spawn(chain_manager);

        (blockchain, pool, provider, handle1, handle2)
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt::Debug, time::Duration};

    use futures_util::Future;
    use libp2p::{identity::Keypair, Multiaddr, PeerId};
    use once_cell::sync::Lazy;
    use pretty_assertions::assert_eq;
    use tempdir::TempDir;
    use tokio::time::sleep;
    use tracing::{debug, error, info};

    use crate::{
        blocktree::{BlockTree, Chain},
        interface::Transaction,
        net::provider::NetworkProvider,
        pool::PoolTx,
        TRACING,
    };

    use super::ClientBuilder;

    // test node
    pub struct Node {
        id: PeerId,
        provider: NetworkProvider,
        blockchain: Chain<NetworkProvider>,
        pool: PoolTx<NetworkProvider>,
        addrs: Vec<Multiaddr>,
    }

    impl Node {
        pub async fn new(idx: usize) -> Self {
            let keypair = Keypair::generate_ed25519();
            let dir = TempDir::new("db").unwrap();
            let db_path = dir.path().join(format!("db_{}.redb", idx));
            let (blockchain, pool, mut provider, _, _) = ClientBuilder::default()
                .with_db_path(db_path)
                .with_mdns(false)
                .with_keypair(keypair.clone())
                .start_network();

            provider
                .start_listening("/ip4/0.0.0.0/tcp/0".parse().expect("parse addr to work"))
                .await
                .expect("Listening not to fail.");
            let id = keypair.public().to_peer_id();
            let addrs = provider.listen_addrs().await.unwrap();
            info!("addrs: {:?}", addrs);

            Self {
                id,
                pool,
                addrs,
                provider,
                blockchain,
            }
        }

        pub async fn dial(&self, peer_id: PeerId, peer_addr: Multiaddr) {
            self.provider.dial(peer_id, peer_addr).await.unwrap();
        }

        pub async fn mining(&mut self) {
            match self.blockchain.mine_and_broadcast_block().await {
                Err(e) => error!("Failed to mine block: {:?}", e),
                Ok(res) => info!("Mining result: {:?}", res),
            }
        }

        pub async fn add_external_transaction(&mut self, tx: Transaction) {
            match self.pool.add_transactions_and_broadcast(vec![tx]).await {
                Err(e) => error!("Failed to broadcast transactions: {:?}", e),
                Ok(hashes) => info!(
                    "Add and broadcast transaction: {:?}",
                    hashes.first().unwrap()
                ),
            }
        }
    }

    pub async fn spawn_nodes(num: usize) -> Vec<Node> {
        let mut nodes = Vec::new();
        for i in 0..num {
            let node = Node::new(i).await;
            nodes.push(node);
        }
        nodes
    }
    async fn assert_with_retry<F, Fut>(mut assertion: F, attempts: u32, delay: Duration)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let mut attempt_count = 0;
        loop {
            if attempt_count >= attempts {
                panic!("Assertion failed after {} attempts", attempts);
            }
            if assertion().await {
                return; // Assertion passed
            }
            attempt_count += 1;
            sleep(delay).await;
        }
    }

    async fn assert_eq_with_retry<F, Fut, T>(
        mut assertion: F,
        attempts: u32,
        delay: Duration,
        expected: T,
    ) where
        F: FnMut() -> Fut,
        Fut: Future<Output = T>,
        T: PartialEq + Debug,
    {
        let mut attempt_count = 0;
        loop {
            if attempt_count >= attempts {
                assert_eq!(assertion().await, expected);
            }
            let actual = assertion().await;
            if actual == expected {
                return; // Assertion passed
            }
            attempt_count += 1;
            sleep(delay).await;
        }
    }

    #[tokio::test]
    async fn test_two_node_sync() {
        Lazy::force(&TRACING);
        let mut nodes = spawn_nodes(2).await;

        nodes[0].dial(nodes[1].id, nodes[1].addrs[0].clone()).await;

        // wait for gossipsub to do the task.
        sleep(Duration::from_secs(2)).await;
        nodes[0].mining().await;

        assert_with_retry(
            || async {
                let head1 = nodes[0].blockchain.tip().unwrap();
                let head2 = nodes[1].blockchain.tip().unwrap();
                head1 == head2
            },
            100,
            Duration::from_millis(100),
        )
        .await;
    }

    #[tokio::test]
    async fn test_line_connected_sync() {
        Lazy::force(&TRACING);
        let num = 10;
        let mut nodes = spawn_nodes(num).await;

        // node0 -> node1 -> node2 -> node3 -> node4 -> node5 -> node6 -> node7 -> node8 -> node9
        for i in 0..num - 1 {
            nodes[i]
                .dial(nodes[i + 1].id, nodes[i + 1].addrs[0].clone())
                .await;
        }

        for _ in 0..10 {
            // currently we need to have one node mining since we have no consensus to select best chain
            nodes[0].mining().await;
            // this should make it more stable syncing.
            sleep(Duration::from_millis(200)).await;
        }

        let first = nodes[0].blockchain.tip().unwrap();
        let expected = vec![first.clone(); num];

        assert_eq_with_retry(
            || async {
                nodes
                    .iter()
                    .map(|node| node.blockchain.tip().unwrap())
                    .collect::<Vec<_>>()
            },
            100,
            Duration::from_millis(100),
            expected,
        )
        .await;
    }

    #[tokio::test]
    async fn test_unconnected_group_sync() {
        Lazy::force(&TRACING);
        let num = 10;
        let mut nodes = spawn_nodes(num).await;

        // node0 -> node1 -> node2 -> node3 -> node4
        for i in 0..4 {
            nodes[i]
                .dial(nodes[i + 1].id, nodes[i + 1].addrs[0].clone())
                .await;
        }

        // node5 -> node6 -> node7 -> node8 -> node9
        for i in 5..num - 1 {
            nodes[i]
                .dial(nodes[i + 1].id, nodes[i + 1].addrs[0].clone())
                .await;
        }

        // this shoud not too fast, otherwise some node may not even establish connection
        // or maybe have more granular control on node connection
        sleep(Duration::from_millis(300)).await;

        for _ in 0..10 {
            // currently we need to have one node mining since we have no consensus to select best chain
            nodes[4].mining().await;
            nodes[7].mining().await;
        }

        let first_group = nodes[4].blockchain.tip().unwrap();
        let second_group = nodes[7].blockchain.tip().unwrap();

        debug!("first_group: {:?}", first_group);
        debug!("second_group: {:?}", second_group);
        let mut expected = vec![first_group.clone(); 5];
        expected.extend(vec![second_group.clone(); 5]);

        assert_eq_with_retry(
            || async {
                nodes[0..num]
                    .iter()
                    .map(|node| node.blockchain.tip().unwrap())
                    .collect::<Vec<_>>()
            },
            100,
            Duration::from_millis(100),
            expected,
        )
        .await;
    }

    #[tokio::test]
    async fn test_two_node_sync_transaction() {
        let mut nodes = spawn_nodes(2).await;

        nodes[0].dial(nodes[1].id, nodes[1].addrs[0].clone()).await;
        // wait for gossipsub to do the task.
        sleep(Duration::from_secs(2)).await;

        let tx = PoolTx::<NetworkProvider>::tx_rng();
        nodes[0].add_external_transaction(tx).await;

        let expected = nodes[0].pool.get_all();

        assert_eq_with_retry(
            || async { nodes[1].pool.get_all() },
            10,
            Duration::from_millis(100),
            expected,
        )
        .await;
    }
}
