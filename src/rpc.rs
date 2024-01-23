use hyper::body::Bytes;
use jsonrpsee::server::ServerHandle;
use jsonrpsee::types::error::{INTERNAL_ERROR_CODE, INVALID_PARAMS_CODE};
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::{server::Server, RpcModule};
use serde_json::json;
use std::iter::once;
use std::net::Ipv4Addr;
use std::{net::SocketAddr, time::Duration};
use tower_http::validate_request::ValidateRequestHeaderLayer;
use tower_http::{
    sensitive_headers::SetSensitiveRequestHeadersLayer,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::info;

use crate::blocktree::BlockTree;
use crate::interface::Transaction;

pub const MOCK_AUTH_TOKEN: &str = "mock_auth_token";
const DEFAULT_RPC_PORT: u16 = 8545;

pub struct RpcServerBuilder<B: BlockTree> {
    addr: SocketAddr,
    chain: Option<B>,
}

impl<B> Default for RpcServerBuilder<B>
where
    B: BlockTree,
{
    fn default() -> Self {
        Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_RPC_PORT)),
            chain: None,
        }
    }
}

impl<B> RpcServerBuilder<B>
where
    B: BlockTree,
{
    pub fn with_addr(mut self, addr: SocketAddr) -> Self {
        self.addr = addr;
        self
    }

    pub fn with_chain(mut self, chain: B) -> Self {
        self.chain = Some(chain);
        self
    }

    pub fn build(self) -> RpcServer<B> {
        RpcServer {
            addr: self.addr,
            chain: self.chain.expect("Chain implement BlockTree is required."),
        }
    }
}

pub struct RpcServer<B: BlockTree> {
    pub addr: SocketAddr,
    pub chain: B,
}

impl<B> RpcServer<B>
where
    B: BlockTree,
{
    pub async fn start_server(&mut self) -> ServerHandle {
        // Custom tower service to handle the RPC requests
        let service_builder = tower::ServiceBuilder::new()
            // Add high level tracing/logging to all requests
            .layer(
                TraceLayer::new_for_http()
                    .on_request(
                        |request: &hyper::Request<hyper::Body>, _span: &tracing::Span| tracing::info!(request = ?request, "on_request"),
                    )
                    .on_body_chunk(|chunk: &Bytes, latency: Duration, _: &tracing::Span| {
                        tracing::info!(size_bytes = chunk.len(), latency = ?latency, "sending body chunk")
                    })
                    .make_span_with(DefaultMakeSpan::new().include_headers(true))
                    .on_response(DefaultOnResponse::new().include_headers(true).latency_unit(tower_http::LatencyUnit::Micros)),
            )
            // Mark the `Authorization` request header as sensitive so it doesn't show in logs
            .layer(SetSensitiveRequestHeadersLayer::new(once(hyper::header::AUTHORIZATION)))
            // This could be a custom authorization middleware that checks the `Authorization` header i.e. JWT
            .layer(ValidateRequestHeaderLayer::bearer(MOCK_AUTH_TOKEN))
            .timeout(Duration::from_secs(2));

        let server = Server::builder()
            .set_http_middleware(service_builder)
            .build("0.0.0.0:0")
            .await
            .unwrap();

        self.addr = server.local_addr().unwrap();
        info!("RPC server listening on {}", self.addr);

        let chain = self.chain.clone();

        let mut module = RpcModule::new(());
        module
            .register_method("eth_getBlockByNumber", move |params, _| {
                let block_number: String = params.one()?;

                let number = match u64::from_str_radix(&block_number[2..], 16) {
                    Ok(n) => n,
                    Err(_) => {
                        return Err(ErrorObjectOwned::owned(
                            INVALID_PARAMS_CODE,
                            "Number should be in hex string format",
                            None::<()>,
                        ));
                    }
                };

                match chain.get_header_by_block_number(number) {
                    Ok(header) => {
                        let block_hash = header.hash;
                        let block_number = header.number;
                        let state_root = header.state_root;
                        let parent_root = header.parent_hash;
                        let transactions: Vec<Transaction> = vec![];

                        return Ok::<_, ErrorObjectOwned>(json!({
                            "block_hash": block_hash,
                            "block_number": format!("0x{:x}",block_number),
                            "state_root": state_root,
                            "parent_root": parent_root,
                            "transactions": transactions
                        }));
                    }
                    Err(_) => Err(ErrorObjectOwned::owned(
                        INTERNAL_ERROR_CODE,
                        format!("Block not found at {}", block_number),
                        None::<()>,
                    )),
                }
            })
            .unwrap();

        let chain = self.chain.clone();
        module
            .register_method("eth_getBlockByHash", move |params, _| {
                let hash: String = params.one()?;

                match chain.get_header_by_hash(hash.clone()) {
                    Ok(header) => {
                        let block_hash = header.hash;
                        let block_number = header.number;
                        let state_root = header.state_root;
                        let parent_root = header.parent_hash;
                        let transactions: Vec<Transaction> = vec![];

                        return Ok::<_, ErrorObjectOwned>(json!({
                            "block_hash": block_hash,
                            "block_number": format!("0x{:x}",block_number),
                            "state_root": state_root,
                            "parent_root": parent_root,
                            "transactions": transactions
                        }));
                    }
                    Err(_) => Err(ErrorObjectOwned::owned(
                        INTERNAL_ERROR_CODE,
                        format!("Block not found with hash: {}", hash),
                        None::<()>,
                    )),
                }
            })
            .unwrap();

        let chain = self.chain.clone();
        module
            .register_method("eth_getBalance", move |params, _| {
                let address: String = params.one()?;

                match chain.get_balance(address) {
                    Err(e) => Err(ErrorObjectOwned::owned(
                        INTERNAL_ERROR_CODE,
                        format!("Internal error: {}", e),
                        None::<()>,
                    )),
                    Ok(balance) => Ok::<_, ErrorObjectOwned>(format!("0x{:x}", balance)),
                }
            })
            .unwrap();

        let handle = server.start(module);

        handle
    }
}

#[cfg(test)]
mod tests {
    use hyper::{body, header, Body, Method, Request};
    use tokio::sync::mpsc;

    use crate::net::chain::tests::MockChain;

    use super::*;

    async fn http_request(body: Body) -> (hyper::StatusCode, String) {
        let chain = MockChain::new(100, mpsc::unbounded_channel().0);
        let mut server = RpcServerBuilder::default()
            .with_chain(chain)
            .with_addr(SocketAddr::from(([0, 0, 0, 0], 0)))
            .build();

        let client = hyper::Client::new();
        let handle = server.start_server().await;
        let addr = server.addr;
        let uri = format!("http://{}", addr);
        let req = Request::builder()
            .method(Method::POST)
            .header(header::AUTHORIZATION, format!("Bearer {}", MOCK_AUTH_TOKEN))
            .header(header::CONTENT_TYPE, "application/json")
            .uri(uri)
            .body(body)
            .unwrap();

        let res = client.request(req).await.unwrap();
        let status = res.status();
        let body_bytes = body::to_bytes(res.into_body()).await.unwrap();
        let body = String::from_utf8(body_bytes.to_vec()).expect("response was not valid utf-8");

        handle.stop().unwrap();
        handle.stopped().await;

        (status, body)
    }

    #[tokio::test]
    async fn test_eth_get_block_by_number() {
        let body = r#"{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params": ["0x12"],"id":1}"#;
        let expected =  "{\"jsonrpc\":\"2.0\",\"result\":{\"block_hash\":\"c99ccfb1e0bc73b59383910492437f1615f5daed4bf55bcf2fa6092b5c158e6a\",\"block_number\":\"0x12\",\"parent_root\":\"09893180eaa1b4f1c4c87d34458f3c785a0d8fbeeaa2bea2f2506b5bb4007c1d\",\"state_root\":\"0x\",\"transactions\":[]},\"id\":1}";

        let (status, res) = http_request(body.into()).await;
        assert_eq!(status, hyper::StatusCode::OK);
        assert_eq!(res, expected.to_string());
    }

    #[tokio::test]
    async fn test_block_not_found() {
        let body =
            r#"{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params": ["0x99999999"],"id":1}"#;
        let expected = r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"Block not found at 0x99999999"},"id":1}"#;

        let (status, res) = http_request(body.into()).await;
        assert_eq!(status, hyper::StatusCode::OK);
        assert_eq!(res, expected.to_string());
    }
    #[tokio::test]
    async fn test_eth_get_block_by_hash() {
        let body = r#"{"jsonrpc":"2.0","method":"eth_getBlockByHash","params": ["0x"],"id":1}"#;
        let expected = "{\"jsonrpc\":\"2.0\",\"result\":{\"block_hash\":\"\",\"block_number\":\"0x0\",\"parent_root\":\"\",\"state_root\":\"\",\"transactions\":[]},\"id\":1}";
        let (status, res) = http_request(body.into()).await;
        assert_eq!(status, hyper::StatusCode::OK);
        assert_eq!(res, expected.to_string());
    }

    #[tokio::test]
    async fn test_eth_get_balance() {
        let body = r#"{"jsonrpc":"2.0","method":"eth_getBalance","params": ["0x000000000000000000000000000000000000dead"],"id":1}"#;
        let expected = "{\"jsonrpc\":\"2.0\",\"result\":\"0xdead\",\"id\":1}";

        let (status, res) = http_request(body.into()).await;
        assert_eq!(status, hyper::StatusCode::OK);
        assert_eq!(res, expected.to_string());
    }
}
