Experimental BM25 full-text indexing inspired by DuckDB’s adaptive radix trees and Lucene’s BlockTree term dictionary.

## Quickstart

Requires Rust, Python 3, and curl.

Start the server:

```sh
# Listens on http://127.0.0.1:3000
cargo run --release
```

In another terminal, download and index some Wikipedia articles, then run a query:

```sh
# Install Python dependencies
python3 -m pip install datasets

# Download sample data
python3 scripts/download_wikipedia.py --limit 1000 --output wikipedia.jsonl

# Index documents, check server logs for progress
python3 scripts/index_wikipedia.py wikipedia.jsonl

# Run a query
curl 'http://127.0.0.1:3000/search?q=something+cool'
```
