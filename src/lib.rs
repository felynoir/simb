use once_cell::sync::Lazy;

pub mod blocktree;
pub mod client;
pub mod interface;
pub mod net;
pub mod rpc;
pub mod tasks;

pub static TRACING: Lazy<()> = Lazy::new(|| {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
});
