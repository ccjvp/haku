use crate::{args::Args, io::IO};
use clap::Parser;

mod args;
mod io;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let io = IO::new(&args);
    let url = "https://place.dog/300/200";
    let id = io.get(url).await?;
    let bytes = io.read(id).await?;

    io.write("doggo.jpeg", &bytes).await?;

    dbg!(bytes.len());
    Ok(())
}
