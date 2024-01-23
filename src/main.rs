use std::path::PathBuf;

use argh::FromArgs;
use libp2p::Multiaddr;
use once_cell::sync::Lazy;
use simb::client::Client;
use simb::TRACING;

#[derive(FromArgs)]
/// A Bitcoin light client.
pub struct Options {
    /// seed nodes
    #[argh(option)]
    pub peers: Vec<Multiaddr>,

    /// path to the database
    #[argh(option)]
    pub db_path: Option<PathBuf>,

    /// interval for mining if set auto mine will automatically mine blocks
    #[argh(option)]
    pub auto_mine_interval: Option<u64>,

    /// seed for the random number generator
    #[argh(option)]
    pub seed: Option<u8>,
}

impl Options {
    pub fn from_env() -> Self {
        argh::from_env()
    }
}

#[tokio::main]
async fn main() {
    Lazy::force(&TRACING);
    let opts = Options::from_env();

    let db_path = if opts.seed.is_some() {
        opts.db_path
            .unwrap_or(format!("./db-{}.redb", opts.seed.unwrap()).into())
    } else {
        opts.db_path.unwrap_or("./db.redb".into())
    };

    let client = Client::new(db_path, opts.auto_mine_interval, opts.peers, opts.seed);
    client.execute().await;
}
