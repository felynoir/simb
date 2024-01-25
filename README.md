## Repo documentation

> This project is not serve for any production or references.

This is a implementation of simplified blockchain node client which has similar interface of Ethereum client.
Most of functionality is in the mock for example:

- Block mining, generate
- Transaction generation
- Implementation of consensus; current implementation use an assumption that header, transaction are valid

As for the network layer we use libp2p and all the network management is locate at `net/network`. But `network manager` will delegate the task related to chain, i.e. incoming header and transaction, to `chain network manager` which locate at `net/chain`.

all the interaction toward network/chain manager will proceed through `network provider` (including future implementation of RPC).

the entry of event comming from two subscribe messages:

- /lightlayer/block_header
- /lightlayer/transactions

Each of those trigger the `network manager` which handle all incoming network event to delegate to `chain network manager` which will try to add block to local chain or including the transactions in local pool.
Then will broadcast to the network.

### Broadcasting Block header

This design not to broadcast entire block come from the efficient of reducing the data where we exchange among peers. Regarding the fact that peer who not manage to sync up to current known tip wouldn't be able to proceed the validation of the block.
We stil let peer broadcasting without validation of the block. As for the draw back of this decision it prone to block header that being spam to the
chain but this could resolve futhur by adjust peer reputation or self validating block header using difficulty or the like.

By limit to only broadcast the header which ahead of local chain we won't have redundant old block being published.

Which make the sync process easier to catch up with the known tip.
And If someone with fall behind local tip broadcasting, it will not broadcasting by majority synced peers.

### Block synchronization process

Initially we not start sync the block out of no where but rather wait for incoming block to start the sync process. To not go complext with the implementation we choose to sync with the closest peers in the kademlia concept and hope that these peer capable to let us sync.
And we choose to sync from the current tip to the best known tip only one request per peers.

When there are two incoming headers we will sync with the current tip to that header and discard the following headers. This way make the process simple and efficient to not redundant sync with the same headers twice. And after all we will have longer tip coming up after finish the previous sync.

### Broadcasting transaction

All the transaction in the local pool is a pending transaction in a valid state (assumption), and we will always broadcast the whole incoming transaction.

This could be more efficient if we track known transaction per peer and try to update them what we have or exchange the transaction periodically. And making a way to request transaction.

### Request Block header

Using libp2p request/response crate for this work. And we breakdown the whole request to a batch where we optimistic request for each peer.
If some of batch invalid or empty we would only since up to the available headers (which validable) and disregard the rest. This way we can remove the retry process which add more complext to the system and leave the sync task to the upcoming header to kick start sync process again. And this could be improve by peer reputation and we won't sit around the bad peer anymore.
