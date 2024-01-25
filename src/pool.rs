use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use ethereum_types::{H160, H256};

use crate::interface::{BroadcastProvider, RequestResponse, Transaction};

/// The pending transaction pool that already to include in the block.
/// For the simple sake we assume that all the transactions are valid.
/// and can't be in invalid state.
#[derive(Clone)]
pub struct PoolTx<P>
where
    P: BroadcastProvider + Clone + Sync + Send + Unpin + 'static,
{
    pub pool: Arc<RwLock<BTreeMap<String, Transaction>>>,
    pub provider: P,
}

impl<P> PoolTx<P>
where
    P: BroadcastProvider + Clone + Sync + Send + Unpin + 'static,
{
    pub fn new(provider: P) -> Self {
        Self {
            pool: Arc::new(RwLock::new(BTreeMap::new())),
            provider,
        }
    }

    pub fn add_transactions(&mut self, txs: Vec<Transaction>) -> Vec<String> {
        let mut added = Vec::new();
        let mut pool = self.pool.write().expect("pool write lock");
        for tx in txs {
            let hash = tx.hash.clone();
            if pool.insert(hash.clone(), tx).is_none() {
                added.push(hash);
            }
        }
        added
    }

    pub fn add_transaction(&mut self, tx: Transaction) -> Option<String> {
        let mut pool = self.pool.write().expect("pool write lock");
        let hash = tx.hash.clone();
        if pool.insert(hash.clone(), tx).is_none() {
            Some(hash)
        } else {
            None
        }
    }

    /// Add external transactions to the pool. And broadcast to the network.
    ///
    /// This intentionally expose as API for RPC or Client.
    pub async fn add_transactions_and_broadcast(
        &mut self,
        txs: Vec<Transaction>,
    ) -> RequestResponse<Vec<String>> {
        let added_hashes = self.add_transactions(txs.clone());

        let txs = txs
            .into_iter()
            .filter(|tx| added_hashes.contains(&tx.hash))
            .collect::<Vec<_>>();
        // would broadcast only transaction that new to the local pool.
        self.provider.broadcast_transactions(txs).await?;

        Ok(added_hashes)
    }

    pub fn get_all(&self) -> Vec<Transaction> {
        self.pool
            .read()
            .expect("pool read lock")
            .values()
            .cloned()
            .collect()
    }

    pub fn tx_rng() -> Transaction {
        Transaction {
            hash: ethereum_types::H256::random().to_string(),
            from: H160::random().to_string(),
            to: H256::random().to_string(),
            value: rand::random(),
            nonce: 0,
        }
    }
}

impl<P> Iterator for PoolTx<P>
where
    P: BroadcastProvider + Clone + Sync + Send + Unpin + 'static,
{
    type Item = Transaction;

    fn next(&mut self) -> Option<Self::Item> {
        let mut pool = self.pool.write().expect("pool write lock");
        pool.pop_first().map(|(_, tx)| tx)
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        interface::{BroadcastFut, BroadcastProvider},
        net::provider::NetworkProvider,
        pool::PoolTx,
    };

    #[derive(Clone)]
    pub struct MockProvider {}
    impl MockProvider {
        pub fn new() -> Self {
            Self {}
        }
    }
    impl BroadcastProvider for MockProvider {
        type Output = BroadcastFut;
        fn broadcast_block_header(&self, _header: crate::interface::Header) -> Self::Output {
            unimplemented!()
        }

        fn broadcast_transactions(
            &self,
            _transactions: Vec<crate::interface::Transaction>,
        ) -> Self::Output {
            unimplemented!()
        }
    }
    #[test]
    fn test_pool() {
        let mut pool = PoolTx::new(MockProvider::new());
        let tx = PoolTx::<NetworkProvider>::tx_rng();
        let res = pool.add_transaction(tx.clone());

        assert_eq!(tx.hash, res.unwrap());

        let res = pool.add_transaction(tx.clone());
        assert_eq!(None, res);

        assert_eq!(pool.next(), Some(tx));
        assert_eq!(pool.next(), None);

        let tx = PoolTx::<NetworkProvider>::tx_rng();
        pool.add_transaction(tx.clone());
        assert_eq!(pool.next(), Some(tx));
        assert_eq!(pool.next(), None);
    }
}
