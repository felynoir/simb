use libp2p::{identity::Keypair, multiaddr::Protocol, Multiaddr, SwarmBuilder};
use std::{path::PathBuf, time::Duration};

use tokio::{spawn, sync::mpsc};

use crate::{
    blocktree::Chain,
    net::{
        chain::ChainNetworkManager,
        network::{AppBehaviour, NetworkManager},
        provider::NetworkProvider,
        BLOCK_HEADER,
    },
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
}
impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("./db"),
            auto_mine_interval: None,
            listening_addrs: vec![],
            keypair: Keypair::generate_ed25519(),
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

    pub async fn execute_with_seed_nodes(mut self, seed_nodes: Vec<Multiaddr>) {
        let (blockchain, mut provider, h1, h2) = self.start_network();

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
            .with_behaviour(|key| AppBehaviour::new(key.clone()))
            .unwrap()
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&BLOCK_HEADER)
            .unwrap();

        let (command_send, command_recv) = mpsc::unbounded_channel();
        let provider = NetworkProvider::new(command_send);
        let blockchain = Chain::new(
            self.db_path.as_path(),
            self.auto_mine_interval,
            provider.clone(),
        );

        let (network, chain_manager_rx) = NetworkManager::new(swarm, command_recv);
        let chain_manager =
            ChainNetworkManager::new(blockchain.clone(), provider.clone(), chain_manager_rx);

        let handle1 = spawn(network.run());
        let handle2 = spawn(chain_manager);

        (blockchain, provider, handle1, handle2)
    }
}

#[cfg(test)]
mod tests {
    use libp2p::{identity::Keypair, Multiaddr, PeerId};
    use once_cell::sync::Lazy;
    use tempdir::TempDir;
    use tracing::{error, info};

    use crate::{
        blocktree::{BlockTree, Chain},
        net::provider::NetworkProvider,
        TRACING,
    };

    use super::ClientBuilder;

    // test node
    pub struct Node {
        id: PeerId,
        provider: NetworkProvider,
        blockchain: Chain<NetworkProvider>,
        addrs: Vec<Multiaddr>,
    }

    impl Node {
        pub async fn new(idx: usize) -> Self {
            let keypair = Keypair::generate_ed25519();
            let dir = TempDir::new("db").unwrap();
            let db_path = dir.path().join(format!("db_{}.redb", idx));
            let (blockchain, mut provider, _, _) = ClientBuilder::default()
                .with_db_path(db_path)
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
    }

    pub async fn spawn_nodes(num: usize) -> Vec<Node> {
        let mut nodes = Vec::new();
        for i in 0..num {
            let node = Node::new(i).await;
            nodes.push(node);
        }
        nodes
    }

    #[tokio::test]
    async fn test_two_node_sync() {
        Lazy::force(&TRACING);
        let mut nodes = spawn_nodes(2).await;

        nodes[0].dial(nodes[1].id, nodes[1].addrs[0].clone()).await;
        nodes[0].mining().await;

        let head1 = nodes[0].blockchain.tip().unwrap();
        let head2 = nodes[1].blockchain.tip().unwrap();

        assert_eq!(head1, head2);
    }
}
