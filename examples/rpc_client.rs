use hyper::header::AUTHORIZATION;
use hyper::HeaderMap;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;
use serde_json::Value;
use simb::rpc::MOCK_AUTH_TOKEN;
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let addr = "0.0.0.0:55858";
    let uri = format!("http://{}", addr);

    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        format!("Bearer {}", MOCK_AUTH_TOKEN).parse().unwrap(),
    );
    let client = HttpClientBuilder::default()
        .set_headers(headers)
        .build(uri)
        .unwrap();
    let response: Value = client
        .request("eth_getBlockByNumber", rpc_params!["0x0"])
        .await
        .unwrap();

    info!("response: {:#?}", response);

    let response: Value = client
        .request(
            "eth_getBlockByHash",
            rpc_params!["0136a0fb5d86d2175d74062cb38324f88cc4437df6145d84140b906e092d94a51"],
        )
        .await
        .unwrap();

    info!("response: {:#?}", response);

    let response: Value = client
        .request(
            "eth_getBalances",
            rpc_params!["0x000000000000000000000000000000000000dead"],
        )
        .await
        .unwrap();
    info!("response: {:#?}", response);
}
