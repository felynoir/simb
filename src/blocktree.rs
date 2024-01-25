use std::{
    path::Path,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
    time::Duration,
};

use alloy_rlp::{Decodable, Encodable};
use chrono::Utc;
use futures_util::{future::poll_fn, ready, Future, FutureExt};
use redb::{
    CommitError, Database, ReadableTable, StorageError, TableDefinition, TableError,
    TransactionError,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    spawn,
    time::{self, Interval},
};
use tracing::{error, info};

use crate::interface::{BroadcastProvider, Header, RequestError, RequestResponse, Transaction};

#[derive(PartialEq, Error, Debug)]
pub enum Error {
    /// header is invalid and not added to the chain
    #[error("Invalid header")]
    Invalid,

    #[error("Block header not found in database")]
    BlockHeaderNotFound,

    #[error("Database error: {0}")]
    DatabaseError(String),
}

impl From<TransactionError> for Error {
    fn from(e: TransactionError) -> Self {
        Error::DatabaseError(e.to_string())
    }
}

impl From<StorageError> for Error {
    fn from(e: StorageError) -> Self {
        Error::DatabaseError(e.to_string())
    }
}

impl From<CommitError> for Error {
    fn from(e: CommitError) -> Self {
        Error::DatabaseError(e.to_string())
    }
}

impl From<TableError> for Error {
    fn from(e: TableError) -> Self {
        Error::DatabaseError(e.to_string())
    }
}

#[derive(PartialEq, Debug)]
pub enum AddedBlockResult {
    /// Block is extend the tip of chain
    ExtendTip,
    /// Block is already in the chain
    AlreadyInChain,
}

#[derive(PartialEq, Debug)]
pub enum MineBlockResult {
    /// Block being mined and added to the chain
    Mined(Header),
    /// Block is being mined but not added to the chain due to falling behind the current tip
    BlockFallingBehind,
}

/// BlockTree is a data structure that holds the chain
/// and responsible for manage the block chain
pub trait BlockTree: Send + Sync + Clone + Unpin + 'static {
    /// Try to add a block to the chain
    /// If the block is valid, it will be added to the chain
    fn try_add_block_header(&mut self, header: Header) -> Result<AddedBlockResult, Error>;

    /// Get a block header by block number
    fn get_header_by_block_number(&self, block_number: u64) -> Result<Header, Error>;

    /// Get a block header by hash
    fn get_header_by_hash(&self, hash: String) -> Result<Header, Error>;

    /// Return the best known local header (validated)
    fn tip(&self) -> Result<Header, Error>;

    /// Get the genesis block header
    fn genesis() -> Header;

    /// Get address balance
    fn get_balance(&self, address: String) -> Result<u64, Error> {
        if address == "0x000000000000000000000000000000000000dead" {
            return Ok(57005);
        }

        Ok(rand::random::<u64>())
    }

    fn mine_block(&mut self) -> MineBlockResult {
        info!("mining block...");
        // mine from current tip
        let prev_block_header = self.tip().unwrap();
        let number = prev_block_header.number + 1;
        let parent_hash = prev_block_header.hash;
        let timestamp = Utc::now().timestamp() as u64;
        let random_bytes: [u8; 32] = rand::random();
        let mut hasher = Sha256::new();
        hasher.update(random_bytes);
        let state_root = hex::encode(hasher.finalize().as_slice());

        let data = "a block data";
        let (nonce, hash) = mine_block(number, timestamp, &parent_hash, &state_root, data);
        info!(
            "mined block {}! nonce: {}, hash: {}",
            number,
            nonce,
            hex::encode(&hash),
        );
        let header = Header {
            hash,
            nonce,
            number,
            parent_hash,
            state_root: "".to_string(),
            timestamp: 0,
        };
        match self.try_add_block_header(header.clone()) {
            Err(e) => {
                panic!("block mining logic failed with: {:?}", e);
            }
            Ok(AddedBlockResult::AlreadyInChain) => MineBlockResult::BlockFallingBehind,
            Ok(AddedBlockResult::ExtendTip) => MineBlockResult::Mined(header),
        }
    }
}

/// (block_number, block_header)
const HEADER: TableDefinition<u64, Vec<u8>> = TableDefinition::new("header");
/// (block_hash, block_number)
const HASH: TableDefinition<&str, u64> = TableDefinition::new("hash");

#[derive(Clone)]
pub struct Chain<P>
where
    P: BroadcastProvider + Clone + Sync + Send + Unpin + 'static,
{
    db: Arc<RwLock<Database>>,
    provider: P,
}

impl<P> Chain<P>
where
    P: BroadcastProvider + Clone + Sync + Send + Unpin + 'static,
{
    pub fn new(path: &Path, auto_mine: Option<u64>, provider: P) -> Self {
        info!("Create db at {}", path.to_str().unwrap());
        let db = Database::create(path).expect("create db");

        info!("Init genesis block");
        let genesis = Self::genesis();
        let write_txn = db.begin_write().expect("writable");
        {
            info!("Haha");

            let mut table = write_txn.open_table(HEADER).expect("open table");
            let mut buf = Vec::new();
            genesis.encode(&mut buf);
            table.insert(genesis.number, buf).expect("insert genesis");
            info!("Haha");

            let mut table = write_txn.open_table(HASH).expect("open table");
            table
                .insert(genesis.hash.as_str(), genesis.number)
                .expect("insert genesis");
        }
        write_txn.commit().expect("commit genesis");
        info!("Haha");

        let chain = Self {
            db: Arc::new(RwLock::new(db)),
            provider,
        };

        info!("Haha");

        if let Some(itv) = auto_mine {
            info!("Start auto mining with interval {}s", itv);
            let miner = FixedBlockTimeMiner::new(chain.clone(), itv, chain.provider.clone());
            spawn(miner.execute());
        }

        info!("Haha");

        chain
    }

    // Skip consensus validation and just validate the parent hash and block number
    // for proper chain ordering
    pub fn validate_adjacent_block_header(&mut self, header: &Header) -> Result<(), Error> {
        // skip validation if it is genesis block
        if *header == Self::genesis() {
            return Ok(());
        }

        let parent_header = self.get_header_by_block_number(header.number - 1)?;

        if parent_header.hash != header.parent_hash {
            return Err(Error::Invalid);
        }

        if parent_header.number + 1 != header.number {
            return Err(Error::Invalid);
        }

        Ok(())
    }

    pub async fn mine_and_broadcast_block(&mut self) -> Result<MineBlockResult, RequestError> {
        match self.mine_block() {
            MineBlockResult::Mined(header) => {
                self.provider.broadcast_block_header(header.clone()).await?;
                Ok(MineBlockResult::Mined(header))
            }
            MineBlockResult::BlockFallingBehind => Ok(MineBlockResult::BlockFallingBehind),
        }
    }
}
impl<P> BlockTree for Chain<P>
where
    P: BroadcastProvider + Clone + Sync + Send + Unpin + 'static,
{
    fn try_add_block_header(&mut self, header: Header) -> Result<AddedBlockResult, Error> {
        // this should check if we can validate or not.
        self.validate_adjacent_block_header(&header)?;

        let old_tip = self.tip()?;

        {
            let binding = self.db.write().expect("db available");
            let write_txn = binding.begin_write()?;
            {
                let mut table = write_txn.open_table(HEADER)?;
                let mut buf = Vec::new();
                header.encode(&mut buf);
                table.insert(header.number.clone(), buf)?;

                let mut table = write_txn.open_table(HASH)?;
                table.insert(header.hash.as_str(), header.number)?;
            }
            write_txn.commit()?;
        }

        if old_tip.number < self.tip()?.number {
            Ok(AddedBlockResult::ExtendTip)
        } else {
            Ok(AddedBlockResult::AlreadyInChain)
        }
    }

    fn get_header_by_block_number(&self, block_number: u64) -> Result<Header, Error> {
        let binding = self.db.read().expect("db available");
        let read_txn = binding.begin_read()?;
        let table = read_txn.open_table(HEADER)?;
        if let Some(header) = table.get(&block_number)? {
            let buf = header.value();
            let header = Header::decode(&mut buf.as_slice()).expect("decode header");
            return Ok(header);
        }
        Err(Error::BlockHeaderNotFound)
    }

    fn get_header_by_hash(&self, hash: String) -> Result<Header, Error> {
        let binding = self.db.read().expect("db available");
        let read_txn = binding.begin_read()?;
        let table = read_txn.open_table(HASH)?;
        if let Some(block_number) = table.get(hash.as_str())? {
            return self.get_header_by_block_number(block_number.value());
        }
        Err(Error::BlockHeaderNotFound)
    }

    // NOTE: this is not support chain re-org
    fn tip(&self) -> Result<Header, Error> {
        let binding = self.db.read().expect("db available");

        let read_txn = binding.begin_read()?;
        let table = read_txn.open_table(HEADER)?;
        let buf = table
            .last()?
            .expect("at least one block in database")
            .1
            .value();
        let header = Header::decode(&mut buf.as_slice()).expect("decode header");

        Ok(header)
    }

    fn genesis() -> Header {
        let timestamp: u64 = 1705551177;
        let parent_hash = "genesis".to_string();
        let state_root = "0x".to_string();
        let data = "0x";
        let nonce = 17012;

        let hash = hex::encode(calculate_hash(
            0,
            timestamp,
            &parent_hash,
            &state_root,
            data,
            nonce,
        ));
        Header {
            hash,
            number: 0,
            nonce,
            parent_hash,
            state_root,
            timestamp,
        }
    }
}
#[cfg(test)]
mod tests {

    use once_cell::sync::Lazy;
    use tempdir::TempDir;
    use tokio::time::sleep;

    use crate::{interface::BroadcastFut, TRACING};

    use super::*;

    #[derive(Clone)]
    struct Provider {}

    impl Provider {
        fn new() -> Self {
            Provider {}
        }
    }

    impl BroadcastProvider for Provider {
        type Output = BroadcastFut;
        fn broadcast_block_header(&self, _header: Header) -> Self::Output {
            Box::pin(async { Ok(()) })
        }

        fn broadcast_transactions(&self, _transactions: Vec<Transaction>) -> Self::Output {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn test_auto_mine() {
        Lazy::force(&TRACING);
        let tmp_dir = TempDir::new("tmp").unwrap();
        let file_path = tmp_dir.path().join("db.redb");
        let provider = Provider::new();
        let chain = Chain::new(&file_path, Some(3), provider);
        sleep(Duration::from_secs(7)).await;
        assert!(chain.tip().unwrap().number > 2);
    }

    #[tokio::test]
    async fn test_non_auto_mine() {
        Lazy::force(&TRACING);
        let tmp_dir = TempDir::new("tmp").unwrap();
        let file_path = tmp_dir.path().join("db.redb");
        let provider = Provider::new();
        let chain = Chain::new(&file_path, None, provider);
        sleep(Duration::from_secs(3)).await;
        assert!(chain.tip().unwrap().number == 0);
    }

    #[test]
    fn test_add_block() {
        Lazy::force(&TRACING);
        let tmp_dir = TempDir::new("tmp").unwrap();
        let file_path = tmp_dir.path().join("db.redb");
        let provider = Provider::new();
        let mut chain = Chain::new(&file_path, None, provider);

        let res = chain.try_add_block_header(Header {
            number: 1,
            ..Default::default()
        });

        assert_eq!(res, Err(Error::Invalid));

        let mut header = Header {
            number: 1,
            hash: "0x".to_string(),
            nonce: 0,
            parent_hash: Chain::<Provider>::genesis().hash,
            timestamp: Utc::now().timestamp() as u64,
            state_root: "0x".to_string(),
        };
        let (nonce, hash) = mine_block(
            1,
            header.timestamp,
            &header.parent_hash,
            &header.state_root,
            "0x",
        );

        header.nonce = nonce;
        header.hash = hex::encode(hash);

        let res = chain.try_add_block_header(header.clone());
        assert_eq!(res, Ok(AddedBlockResult::ExtendTip));

        let res = chain.try_add_block_header(header);
        assert_eq!(res, Ok(AddedBlockResult::AlreadyInChain));
    }

    #[test]
    fn test_add_block_ahead_of_tip() {
        Lazy::force(&TRACING);
        let tmp_dir = TempDir::new("tmp").unwrap();
        let file_path = tmp_dir.path().join("db.redb");
        let provider = Provider::new();
        let mut chain = Chain::new(&file_path, None, provider);

        let res = chain.try_add_block_header(Header {
            number: 100,
            ..Default::default()
        });

        assert_eq!(res, Err(Error::BlockHeaderNotFound));
    }

    #[test]
    fn test_add_genesis_block() {
        Lazy::force(&TRACING);
        let tmp_dir = TempDir::new("tmp").unwrap();
        let file_path = tmp_dir.path().join("db.redb");
        let provider = Provider::new();
        let mut chain = Chain::new(&file_path, None, provider);

        let res = chain.try_add_block_header(Chain::<Provider>::genesis());

        assert_eq!(res, Ok(AddedBlockResult::AlreadyInChain));
    }

    #[test]
    fn test_validation_logic() {
        Lazy::force(&TRACING);
        let tmp_dir = TempDir::new("tmp").unwrap();
        let file_path = tmp_dir.path().join("db.redb");
        let provider = Provider::new();
        let mut chain = Chain::new(&file_path, None, provider);

        chain
            .validate_adjacent_block_header(&Chain::<Provider>::genesis())
            .unwrap();

        let err = chain
            .validate_adjacent_block_header(&Header {
                number: 10,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err, Error::BlockHeaderNotFound);

        chain
            .validate_adjacent_block_header(&Header {
                number: 1,
                parent_hash: Chain::<Provider>::genesis().hash,
                ..Default::default()
            })
            .unwrap();

        let err = chain
            .validate_adjacent_block_header(&Header {
                number: 1,
                parent_hash: "0x".to_string(),
                ..Default::default()
            })
            .unwrap_err();

        assert_eq!(err, Error::Invalid);
    }
}
pub struct BroadcastRequest<P: BroadcastProvider> {
    header: Header,
    fut: P::Output,
}

pub struct BroadcastOutcome {
    header: Header,
    outcome: RequestResponse<()>,
}

impl<P> Future for BroadcastRequest<P>
where
    P: BroadcastProvider,
{
    type Output = BroadcastOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let outcome = ready!(this.fut.poll_unpin(cx));
        let header = this.header.clone();

        Poll::Ready(BroadcastOutcome { header, outcome })
    }
}
/// A miner that mines blocks with a fixed block time
pub struct FixedBlockTimeMiner<B: BlockTree, P: BroadcastProvider> {
    interval: Interval,
    chain: B,
    provider: P,
    pending_broadcast: Option<BroadcastRequest<P>>,
}

impl<B, P> FixedBlockTimeMiner<B, P>
where
    B: BlockTree,
    P: BroadcastProvider,
{
    pub fn new(chain: B, itv: u64, provider: P) -> Self {
        FixedBlockTimeMiner {
            interval: time::interval(Duration::from_secs(itv)),
            chain,
            provider,
            pending_broadcast: None,
        }
    }

    pub async fn execute(mut self) {
        poll_fn(move |cx| {
            if let Poll::Ready(_transactions) = self.poll(cx) {
                match self.chain.mine_block() {
                    MineBlockResult::Mined(header) => {
                        self.pending_broadcast = Some(BroadcastRequest {
                            fut: self.provider.broadcast_block_header(header.clone()),
                            header,
                        });
                    }
                    MineBlockResult::BlockFallingBehind => {}
                }
                let _ = self.poll(cx);
            }

            if let Some(mut request) = self.pending_broadcast.take() {
                if let Poll::Ready(BroadcastOutcome { header, outcome }) = request.poll_unpin(cx) {
                    match outcome {
                        Ok(_) => {
                            info!("Broadcast success: {:?}", header);
                        }
                        Err(e) => {
                            error!("Broadcast block header failed {:?}", e);
                        }
                    }
                    self.pending_broadcast = None;
                } else {
                    self.pending_broadcast = Some(request);
                }
            }

            Poll::Pending
        })
        .await
    }

    /// This will poll the mining task every fixed interval
    /// with mock transactions
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Vec<Transaction>> {
        if self.interval.poll_tick(cx).is_ready() {
            // TODO: not empty block
            return Poll::Ready(vec![]);
        }
        Poll::Pending
    }
}

pub fn calculate_hash(
    block_number: u64,
    timestamp: u64,
    parent_hash: &str,
    state_root: &str,
    data: &str,
    nonce: u64,
) -> Vec<u8> {
    let data = serde_json::json!({
        "block_number": block_number,
        "parent_hash": parent_hash,
        "data": data,
        "state_root": state_root,
        "timestamp": timestamp,
        "nonce": nonce
    });

    let mut hasher = Sha256::new();
    hasher.update(data.to_string().as_bytes());
    hasher.finalize().as_slice().to_owned()
}

pub fn mine_block(
    number: u64,
    timestamp: u64,
    previous_hash: &str,
    state_root: &str,
    data: &str,
) -> (u64, String) {
    let nonce = rand::random::<u64>();
    let hash = calculate_hash(number, timestamp, previous_hash, state_root, data, nonce);

    (nonce, hex::encode(hash))
}
