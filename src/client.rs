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
pub struct Client {
    db_path: PathBuf,
    auto_mine_interval: Option<u64>,
    seed_nodes: Vec<Multiaddr>,
    seed: Option<u8>,
}

impl Client {
    pub fn new(
        db_path: PathBuf,
        auto_mine_interval: Option<u64>,
        seed_nodes: Vec<Multiaddr>,
        seed: Option<u8>,
    ) -> Self {
        Self {
            db_path: db_path.to_path_buf(),
            auto_mine_interval,
            seed_nodes,
            seed,
        }
    }
    pub async fn execute(mut self) {
        let (blockchain, provider, h1, h2) = self.start_network(self.seed);

        // start dial seed nodes
        for addr in self.seed_nodes {
            let peer_id = match addr.iter().last() {
                Some(Protocol::P2p(peer_id)) => peer_id,
                _ => panic!("peer id should exist"),
            };
            let _ = provider.dial(peer_id, addr);
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
        seed: Option<u8>,
    ) -> (
        Chain<NetworkProvider>,
        NetworkProvider,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let keypair = match seed {
            Some(seed) => {
                let mut bytes = [0u8; 32];
                bytes[0] = seed;
                Keypair::ed25519_from_bytes(bytes).unwrap()
            }
            None => Keypair::generate_ed25519(),
        };
        let mut swarm = SwarmBuilder::with_existing_identity(keypair)
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

        swarm
            .listen_on("/ip4/0.0.0.0/tcp/0".parse().unwrap())
            .expect("work");
        //swarm
        //    .listen_on(listen_addr)
        //    .expect("to be able to start listening to the specified address");

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
