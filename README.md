## Repo documentation

This project, a simplified blockchain node client, is purely experimental and not intended for production use or as a reference. It offers a user experience similar to an Ethereum client, but with most functionalities being mocked, including:

Most of functionality is in the mock for example:

- Block Mining and Generation: Simplified mock-up of the actual mining process.
- Transaction Generation: Creation of mock transactions.
- Consensus Implementation: The current version operates under the assumption that headers and transactions are valid, simplifying consensus mechanisms.

The network layer is built on libp2p, with all network management located in `net/`. The `Network Manager` (`net/network`) delegates blockchain-specific tasks (like incoming headers and transactions) to the `Chain Network Manager`, found in `net/chain`.

Interaction with the network and chain managers is facilitated by the `Network Provider`, which will also be integral to future RPC implementation.

Entry of event come from two subscribe messages in Gossipsub:

- /lightlayer/block_header
- /lightlayer/transactions

### Broadcasting Block Header

To enhance efficiency and reduce data exchange among peers, the system is designed to broadcast only block headers, not entire blocks. This approach is based on the rationale that peers not up-to-date with the current tip can't validate blocks effectively. While this does expose the system to potential spamming of invalid block headers, such risks can be mitigated through peer reputation adjustments or self-validating headers based on difficulty parameters.

By restricting broadcasts to headers ahead of the local chain, the system avoids circulating outdated blocks, simplifying the synchronization process and ensuring that only the majority of synced peers broadcast new headers.

### Block Synchronization Process

Initially we not start sync the block out of no where but rather wait for incoming block to start the sync process. To keep the implementation straightforward, synchronization occurs with the closest peers based on Kademlia's principles. This approach assumes these peers can facilitate effective syncing.

When dealing with multiple incoming headers, the system syncs from the current tip to the new header, ignoring subsequent headers. This method avoids redundant synchronization efforts and ensures that longer tips are eventually reached as previous syncs complete.

### Broadcasting Transactions

All transactions in the local pool are pending and assumed valid. The system broadcasts all incoming transactions. Future enhancements could include tracking known transactions per peer, exchanging transactions periodically, and enabling transaction requests.

### Requesting Block Headers

Block header requests are handled using libp2p's request/response mechanism. Requests are broken down into batches for each peer. If a batch is invalid or incomplete, the system syncs up to the last valid header, ignoring the rest. This eliminates the need for retry mechanisms, streamlining the system and relying on subsequent headers to restart the sync process. Future improvements may involve enhancing peer reputation mechanisms to avoid reliance on unreliable peers.
