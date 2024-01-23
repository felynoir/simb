use std::task::Poll;

use futures_util::{stream::FuturesUnordered, stream::StreamExt, Future};
use libp2p::PeerId;
use thiserror::Error;
use tracing::debug;

use crate::interface::{Header, HeadersProvider, HeadersRequest, RequestError}; // Assuming you have a mock implementation of HeadersProvider

pub struct SyncHeaderBuilder<H: HeadersProvider> {
    provider: Option<H>,
    peer_id: Option<PeerId>,
    local_head_block: u64,
    target_head: u64,
    request_limit: u64,
    max_concurrent_limit: usize,
}

impl<H: HeadersProvider> Default for SyncHeaderBuilder<H> {
    fn default() -> Self {
        Self {
            provider: None,
            peer_id: None,
            // we alway have genesis block at the beginning
            local_head_block: 0,
            target_head: 0,
            // reference from other client
            request_limit: 1000,
            // this is arbitary value, we can change it later
            max_concurrent_limit: 10,
        }
    }
}

impl<H: HeadersProvider> SyncHeaderBuilder<H> {
    /// Set the provider for the sync header
    ///
    /// Provider is required.
    /// And need to implement HeadersProvider trait which provider way to get headers from the network.
    pub fn provider(mut self, provider: H) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Set the best known local head blcok
    ///
    /// Default is 0 (genesis block). And will be sync from block 1 to the target head
    pub fn local_head_block(mut self, block: u64) -> Self {
        self.local_head_block = block;
        self
    }

    /// Set the target head block
    ///
    /// The target head block is inclusive. which mean (local_head_block, target_head] will be sync.
    /// Default is 0 (genesis block). Which won't be sync any headers.
    pub fn target_head(mut self, block: u64) -> Self {
        self.target_head = block;
        self
    }

    /// Set peer id to sync from
    ///
    /// Peer id is required.
    pub fn peer_id(mut self, peer_id: PeerId) -> Self {
        self.peer_id = Some(peer_id);
        self
    }

    /// Set request batch size.
    ///
    /// This determine `limit` of `GetHeadersRequest` request per batch.
    pub fn request_limit(mut self, limit: u64) -> Self {
        self.request_limit = limit;
        self
    }

    /// Set maximum of concurrent request.
    ///
    /// Default is 10.
    pub fn max_concurrent_limit(mut self, limit: usize) -> Self {
        self.max_concurrent_limit = limit;
        self
    }

    pub fn build(self) -> SyncHeader<H> {
        SyncHeader {
            provider: self.provider.expect("Provider is required"),
            local_head_block: self.local_head_block,
            peer_id: self.peer_id.expect("Peer id is required"),
            request_limit: self.request_limit,
            target_head: self.target_head,
            max_concurrent_limit: self.max_concurrent_limit,
            queued_headers: Default::default(),
            in_progress_queue: Default::default(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SyncHeaderError {
    #[error("Header is invalid.")]
    HeaderInvalid,
    #[error("Response is empty.")]
    EmptyResponse,
    #[error("Request erorr.")]
    RequestError(#[from] RequestError),
}

/// SyncHeader is responsible for syncing headers from the network
/// This will sync from the target tip of the chain to the local head (for simplicity it's genesis block)
pub struct SyncHeader<H: HeadersProvider> {
    provider: H,
    in_progress_queue: FuturesUnordered<H::Output>,
    queued_headers: Vec<Header>,
    max_concurrent_limit: usize,
    local_head_block: u64,
    request_limit: u64,
    target_head: u64,
    peer_id: PeerId,
}

impl<H> SyncHeader<H>
where
    H: HeadersProvider,
{
    fn on_header_response(&mut self, headers: Vec<Header>) -> Result<(), SyncHeaderError> {
        if headers.is_empty() {
            return Err(SyncHeaderError::EmptyResponse);
        }

        debug!(
            "Received {} headers from {} to {} ",
            headers.len(),
            headers.first().unwrap().number,
            headers.last().unwrap().number
        );

        // Skip some check for now.
        // Such as number of headers, invalid headers, ordering , etc.
        self.queued_headers.extend(headers);
        Ok(())
    }
}

pub type SyncHeaderOutput = Result<Vec<Header>, SyncHeaderError>;

impl<H> Future for SyncHeader<H>
where
    H: HeadersProvider + Unpin,
{
    type Output = SyncHeaderOutput;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            while let Poll::Ready(Some(headers)) = this.in_progress_queue.poll_next_unpin(cx) {
                match headers {
                    Ok(headers) => this.on_header_response(headers)?,
                    Err(e) => {
                        return Poll::Ready(Err(e.into()));
                    }
                }
            }
            let mut progress = false;

            // This can prevent task from switching (not sure tokio's cooperative scheduling is balanced this out)
            // consider adding min concurrent limit to have a range for switching in case of starvation
            // this may happen when progress is steady ready and not leave any room for SyncHeader yield pending
            while this.in_progress_queue.len() < this.max_concurrent_limit {
                // Only sync if we are behind the head of the chain
                if this.target_head > this.local_head_block {
                    let peer_id = this.peer_id;
                    let diff = this.target_head - this.local_head_block;
                    let limit = diff.min(this.request_limit);

                    let request = HeadersRequest {
                        start_block: this.local_head_block + 1,
                        limit,
                    };

                    debug!(
                        "Request headers block number from {} to {} from {:?}",
                        request.start_block,
                        request.start_block + request.limit - 1,
                        peer_id
                    );

                    this.local_head_block += request.limit;
                    this.in_progress_queue
                        .push(this.provider.get_headers(peer_id, request));
                    progress = true;
                } else {
                    break;
                }
            }

            if !progress {
                break;
            }
        }

        if this.in_progress_queue.is_empty() {
            debug!("Sync done with {} headers", this.queued_headers.len());
            return Poll::Ready(Ok(this.queued_headers.clone()));
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use libp2p::PeerId;

    use crate::interface::HeadersFut;

    use super::*;

    struct MockHeadersProvider {}

    impl MockHeadersProvider {
        pub fn new() -> Self {
            Self {}
        }
    }

    impl HeadersProvider for MockHeadersProvider {
        type Output = HeadersFut;
        fn get_headers(&self, _peer_id: PeerId, request: HeadersRequest) -> Self::Output {
            debug!(
                "Request headers from {} to {}",
                request.start_block,
                request.start_block + request.limit - 1
            );
            // Generate mock data for testing purposes
            let mut headers = vec![];
            for i in 0..request.limit {
                headers.push(Header {
                    number: i + request.start_block,
                    ..Default::default()
                });
            }
            Box::pin(async { Ok(headers) })
        }
    }

    struct EmptyProvider {}

    impl EmptyProvider {
        pub fn new() -> Self {
            Self {}
        }
    }

    impl HeadersProvider for EmptyProvider {
        type Output = HeadersFut;
        fn get_headers(&self, _peer_id: PeerId, _request: HeadersRequest) -> Self::Output {
            Box::pin(async { Ok(vec![]) })
        }
    }

    #[tokio::test]
    async fn test_calculation_correct() {
        let provider = MockHeadersProvider::new();

        let local_head_block = 0;
        let request_limit = 7;
        let target_head = 1000;
        let sync_header = SyncHeaderBuilder::default()
            .provider(provider)
            .local_head_block(local_head_block)
            .target_head(target_head)
            .peer_id(PeerId::random())
            .request_limit(request_limit)
            .build();

        let result = sync_header.await.unwrap();

        assert_eq!(result.len(), 1000);

        // preserved order
        for i in local_head_block + 1..target_head + 1 {
            assert_eq!(result[i as usize - 1].number, i);
        }
    }

    #[tokio::test]
    async fn test_empty_response() {
        let provider = EmptyProvider::new();

        let local_head_block = 0;
        let request_limit = 7;
        let target_head = 1000;
        let sync_header = SyncHeaderBuilder::default()
            .provider(provider)
            .local_head_block(local_head_block)
            .target_head(target_head)
            .peer_id(PeerId::random())
            .request_limit(request_limit)
            .build();

        let err = sync_header.await.unwrap_err();
        match err {
            SyncHeaderError::EmptyResponse => {}
            _ => panic!("Expect empty response error"),
        }
    }
}
