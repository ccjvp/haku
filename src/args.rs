use std::path::PathBuf;

use clap::Parser;

fn positive_usize(value: &str) -> Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| "must be a positive integer".to_owned())?;
    if value == 0 {
        return Err("must be greater than zero".to_owned());
    }
    Ok(value)
}

#[derive(Debug, Parser)]
#[command(about)]
pub struct Args {
    #[arg(long)]
    pub no_cache: bool,
    #[arg(long, default_value = "data/wal")]
    pub wal_dir: PathBuf,
    #[arg(long, default_value = "data/cache")]
    pub cache_dir: PathBuf,
    #[arg(long, default_value_t = 1024 * 1024 * 1024, value_parser = positive_usize, help = "maximum in-memory cache size in bytes")]
    pub cache_capacity: usize,
    #[arg(long, default_value_t = 32, value_parser = positive_usize)]
    pub max_http_requests: usize,
    #[arg(long, default_value_t = 8 * 1024 * 1024, value_parser = positive_usize)]
    pub max_chunk_size: usize,
}
