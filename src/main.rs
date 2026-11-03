use crate::server::serve;
use haku::{Args, BM25, IO};
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

mod server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_tracing();

    let args = Arc::new(Args::parse());
    let io = Arc::new(IO::new(&args));
    let bm25 = BM25::open(io.clone(), &args).await?;
    let bm25 = Arc::new(bm25);

    run_index_update(args.clone(), bm25.clone());
    serve(io.clone(), args.clone()).await
}

fn setup_tracing() {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::new("info"),
    };

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn run_index_update(args: Arc<Args>, bm25: Arc<BM25>) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = bm25.update(args.clone()).await {
                tracing::error!(%error, "index update failed");
            }

            let sleep = std::time::Duration::from_secs(1);
            tokio::time::sleep(sleep).await;
        }
    });
}
