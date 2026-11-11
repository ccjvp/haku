use crate::args::Args;
use hyper::body::HttpBody;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::{path::PathBuf, time::Instant};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

// https://codepointer.dev/p/uv-oncemap-rust-pattern-for-running
enum Chunk {
    Waiting(Arc<tokio::sync::Notify>),
    Filled(Arc<Vec<u8>>),
}

struct Cache {
    data: lru::LruCache<u64, Chunk>,
    pool: Vec<Vec<u8>>,
    bytes: usize,
    capacity: usize,
}

type Client = hyper::Client<hyper_rustls::HttpsConnector<hyper::client::HttpConnector>>;

pub struct IO {
    cache: Mutex<Cache>,
    max_chunk_size: usize,
    cache_dir: PathBuf,
    cache_enabled: bool,
    http_client: Client,
    http_limit: Arc<tokio::sync::Semaphore>,
    http_runtime: Option<tokio::runtime::Runtime>,
}

impl IO {
    pub fn new(args: &Args) -> Self {
        // https://duckdb.org/2026/07/31/asynchronous-io
        let workers = match std::thread::available_parallelism() {
            Ok(n) => n.get().saturating_mul(4).clamp(2, 256),
            Err(_) => 8,
        };

        let http_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .expect("failed to build HTTP runtime");

        let http_client: Client = hyper::Client::builder().build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_webpki_roots()
                .https_or_http()
                .enable_http1()
                .enable_http2()
                .build(),
        );

        let cache = Mutex::new(Cache {
            data: lru::LruCache::unbounded(),
            pool: Vec::new(),
            bytes: 0,
            capacity: args.cache_capacity,
        });

        Self {
            cache,
            max_chunk_size: args.max_chunk_size,
            cache_dir: args.cache_dir.clone(),
            cache_enabled: !args.no_cache,
            http_client,
            http_limit: Arc::new(tokio::sync::Semaphore::new(args.max_http_requests)),
            http_runtime: Some(http_runtime),
        }
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<u64> {
        self.get_range(path, 0, None).await
    }

    pub async fn get_range(
        &self,
        path: &str,
        start: usize,
        limit: Option<usize>,
    ) -> anyhow::Result<u64> {
        if limit.is_some_and(|limit| limit > self.max_chunk_size) {
            return Err(anyhow::anyhow!(
                "requested range exceeds maximum chunk size"
            ));
        }

        let end = match limit {
            Some(limit) => match start.checked_add(limit) {
                Some(end) => Some(end),
                None => return Err(anyhow::anyhow!("requested range end overflows usize")),
            },
            None => None,
        };

        let id = match end {
            Some(end) => range_id(path, start, end),
            None => file_id(path),
        };

        if self.read_memory_cache(id) {
            return Ok(id);
        }

        if !is_http(path) {
            let path = std::path::Path::new(path);
            let result = self.read_file(id, path, start, limit).await;

            return match result {
                Ok(()) => Ok(id),
                Err(e) => {
                    self.cancel_waiting(id);
                    Err(e)
                }
            };
        }

        if self.read_file_cache(id).await? {
            return Ok(id);
        }

        let buffer = self.acquire_buffer(limit.unwrap_or(0));
        let range = end.map(|end| start..end);
        let response = self.fetch(path, range, buffer).await;

        let bytes = match response {
            Ok(bytes) => bytes,
            Err(e) => {
                self.cancel_waiting(id);
                return Err(e);
            }
        };

        self.complete_remote(id, bytes).await
    }

    pub async fn read(&self, id: u64) -> anyhow::Result<Vec<u8>> {
        self.read_range(id, 0, None).await
    }

    pub async fn read_range(
        &self,
        id: u64,
        start: usize,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<u8>> {
        let end = match limit {
            Some(limit) => match start.checked_add(limit) {
                Some(end) => Some(end),
                None => return Err(anyhow::anyhow!("requested range end overflows usize")),
            },
            None => None,
        };

        let mut cache = self.cache.lock().unwrap();
        match cache.data.get(&id) {
            Some(Chunk::Waiting(notify)) => {
                let notify = notify.clone();
                let notified = notify.notified();
                drop(cache);
                notified.await;
            }
            _ => drop(cache),
        }

        let cached = {
            let mut cache = self.cache.lock().unwrap();

            match cache.data.get(&id) {
                Some(Chunk::Filled(data)) => data.clone(),
                _ => return Err(anyhow::anyhow!("file was evicted")),
            }
        };

        match end {
            Some(end) => match cached.get(start..end) {
                Some(bytes) => Ok(bytes.to_vec()),
                None => Err(anyhow::anyhow!("range exceeds cached data length")),
            },
            None => Ok(cached.as_ref().clone()),
        }
    }

    async fn read_file(
        &self,
        id: u64,
        path: &std::path::Path,
        start: usize,
        limit: Option<usize>,
    ) -> anyhow::Result<()> {
        let mut file = fs::File::open(path).await?;
        let starts_after_beginning = start != 0;
        if starts_after_beginning {
            file.seek(tokio::io::SeekFrom::Start(start as u64)).await?;
        }

        let expected_size = limit.unwrap_or(0);
        let mut bytes = self.acquire_buffer(expected_size);

        let read_result = match limit {
            Some(limit) => {
                let result = file.take(limit as u64).read_to_end(&mut bytes).await;
                if let Err(error) = result {
                    self.recycle_buffer(bytes);
                    return Err(error.into());
                }

                if bytes.len() != limit {
                    self.recycle_buffer(bytes);
                    return Err(anyhow::anyhow!("range exceeds file length"));
                }

                Ok(())
            }
            None => file.read_to_end(&mut bytes).await.map(|_| ()),
        };

        if let Err(error) = read_result {
            self.recycle_buffer(bytes);
            return Err(error.into());
        }

        self.insert_memory_cache(id, bytes)
    }

    async fn read_file_cache(&self, id: u64) -> anyhow::Result<bool> {
        if !self.cache_enabled {
            return Ok(false);
        }

        let path = self.cache_path(id);
        if !fs::try_exists(&path).await? {
            return Ok(false);
        }

        match self.read_file(id, &path, 0, None).await {
            Ok(()) => Ok(true),
            Err(error) => {
                self.cancel_waiting(id);
                Err(error)
            }
        }
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<std::ops::Range<usize>>,
        mut bytes: Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        if range.as_ref().is_some_and(|range| range.is_empty()) {
            return Ok(bytes);
        }

        let started_at = Instant::now();
        let request_url = url.to_owned();
        let log_url = request_url.clone();
        let client = self.http_client.clone();
        let http_limit = self.http_limit.clone();
        let max_chunk_size = self.max_chunk_size;

        let request = self.http_runtime.as_ref().unwrap().spawn(async move {
            let _permit = http_limit.acquire_owned().await?;
            let url = request_url.parse::<hyper::Uri>()?;

            let mut builder = hyper::Request::get(url);
            if let Some(range) = range.as_ref() {
                let end = range.end.saturating_sub(1);
                let range = format!("bytes={}-{}", range.start, end);

                builder = builder.header("Range", range);
            }

            let request = builder.body(hyper::Body::empty())?;
            let response = client.request(request).await?;
            if !response.status().is_success() {
                return Err(anyhow::anyhow!(
                    "HTTP request failed: {}",
                    response.status()
                ));
            }

            if let Some(expected_range) = range.as_ref() {
                if response.status() != hyper::StatusCode::PARTIAL_CONTENT {
                    return Err(anyhow::anyhow!(
                        "HTTP range request returned status {}, expected 206 Partial Content",
                        response.status()
                    ));
                }

                let content_range = response
                    .headers()
                    .get(hyper::header::CONTENT_RANGE)
                    .ok_or_else(|| anyhow::anyhow!("HTTP range response is missing Content-Range"))?
                    .to_str()?;

                validate_content_range(content_range, expected_range)?;
            }

            if let Some(length) = response.headers().get(hyper::header::CONTENT_LENGTH) {
                if let Ok(length) = length.to_str().unwrap_or_default().parse::<usize>() {
                    if length > max_chunk_size {
                        return Err(anyhow::anyhow!(
                            "HTTP response exceeds maximum chunk size of {max_chunk_size} bytes"
                        ));
                    }

                    bytes.try_reserve(length)?;
                }
            }

            let mut body = response.into_body();
            while let Some(chunk) = body.data().await {
                let chunk = chunk?;
                let new_length = bytes
                    .len()
                    .checked_add(chunk.len())
                    .ok_or_else(|| anyhow::anyhow!("HTTP response length overflow"))?;
                if new_length > max_chunk_size {
                    return Err(anyhow::anyhow!(
                        "HTTP response exceeds maximum chunk size of {max_chunk_size} bytes"
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }

            if let Some(expected_range) = range.as_ref() {
                let expected_length = expected_range.end - expected_range.start;
                if bytes.len() != expected_length {
                    return Err(anyhow::anyhow!(
                        "HTTP range response body has {} bytes, expected {}",
                        bytes.len(),
                        expected_length
                    ));
                }
            }

            Ok::<_, anyhow::Error>(bytes)
        });

        let bytes = request.await??;
        let elapsed_ms = started_at.elapsed().as_millis() as u64;
        tracing::info!(url = %log_url, bytes = bytes.len(), elapsed_ms, "single remote request completed");
        Ok(bytes)
    }

    async fn complete_remote(&self, id: u64, b: Vec<u8>) -> anyhow::Result<u64> {
        if b.len() > self.max_chunk_size {
            self.cancel_waiting(id);
            return Err(anyhow::anyhow!("buffer exceeds maximum chunk size"));
        }

        if self.cache_enabled {
            if let Err(e) = self.write_file_cache(id, &b).await {
                self.cancel_waiting(id);
                return Err(e);
            }
        }

        match self.insert_memory_cache(id, b) {
            Ok(_) => Ok(id),
            Err(e) => {
                self.cancel_waiting(id);
                Err(e)
            }
        }
    }

    fn insert_memory_cache(&self, id: u64, b: Vec<u8>) -> anyhow::Result<()> {
        if b.len() > self.max_chunk_size {
            return Err(anyhow::anyhow!("buffer exceeds maximum chunk size"));
        }

        let n = b.len();

        let waiting_notify = {
            let mut cache = self.cache.lock().unwrap();

            let waiting_notify = match cache.data.put(id, Chunk::Filled(Arc::new(b))) {
                Some(Chunk::Waiting(notify)) => Some(notify),
                Some(Chunk::Filled(previous)) => {
                    cache.bytes -= previous.len();
                    None
                }
                None => None,
            };

            cache.bytes += n;
            'evict: while cache.bytes > cache.capacity {
                let entries = cache.data.len();

                for _ in 0..entries {
                    let Some((oldest_id, chunk)) = cache.data.pop_lru() else {
                        break 'evict;
                    };

                    match chunk {
                        Chunk::Filled(data) => {
                            cache.bytes -= data.len();
                            if let Ok(data) = Arc::try_unwrap(data) {
                                cache.pool.push(data);
                            }

                            continue 'evict;
                        }
                        Chunk::Waiting(notify) => {
                            let chunk = Chunk::Waiting(notify);
                            cache.data.put(oldest_id, chunk);
                        }
                    }
                }
            }

            waiting_notify
        };

        if let Some(notify) = waiting_notify {
            notify.notify_waiters();
        }

        Ok(())
    }

    async fn write_file_cache(&self, id: u64, b: &[u8]) -> anyhow::Result<()> {
        fs::write(self.cache_path(id), b).await?;

        Ok(())
    }

    fn acquire_buffer(&self, expected: usize) -> Vec<u8> {
        let mut cache = self.cache.lock().unwrap();
        let mut buffer = cache.pool.pop().unwrap_or_default();
        drop(cache);
        buffer.clear();

        if expected != 0 {
            buffer.reserve(expected.min(self.max_chunk_size));
        }

        buffer
    }

    fn recycle_buffer(&self, mut buffer: Vec<u8>) {
        buffer.clear();
        self.cache.lock().unwrap().pool.push(buffer);
    }

    fn read_memory_cache(&self, id: u64) -> bool {
        if !self.cache_enabled {
            return false;
        }

        let mut cache = self.cache.lock().unwrap();
        match cache.data.get(&id) {
            Some(Chunk::Filled(_)) | Some(Chunk::Waiting(_)) => true,
            None => {
                let notify = Arc::new(tokio::sync::Notify::new());
                cache.data.put(id, Chunk::Waiting(notify));

                false
            }
        }
    }

    fn cancel_waiting(&self, id: u64) {
        let waiting_notify = {
            let mut cache = self.cache.lock().unwrap();

            match cache.data.pop(&id) {
                Some(Chunk::Waiting(notify)) => Some(notify),
                Some(chunk) => {
                    cache.data.put(id, chunk);
                    None
                }
                None => None,
            }
        };

        if let Some(notify) = waiting_notify {
            notify.notify_waiters();
        }
    }

    fn cache_path(&self, id: u64) -> PathBuf {
        self.cache_dir.join(format!("{id:016x}"))
    }

    pub async fn write(&self, path: &str, b: &[u8]) -> anyhow::Result<()> {
        if is_http(path) {
            let client = self.http_client.clone();
            let http_limit = self.http_limit.clone();
            let url = path.parse::<hyper::Uri>()?;
            let body = b.to_vec();

            self.http_runtime
                .as_ref()
                .unwrap()
                .spawn(async move {
                    let _permit = http_limit.acquire_owned().await?;
                    let body = hyper::Body::from(body);
                    let request = hyper::Request::put(url).body(body)?;
                    let response = client.request(request).await?;
                    if !response.status().is_success() {
                        return Err(anyhow::anyhow!(
                            "HTTP request failed: {}",
                            response.status()
                        ));
                    }

                    Ok::<_, anyhow::Error>(())
                })
                .await??;

            Ok(())
        } else {
            if let Some(parent) = Path::new(path).parent() {
                fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, b).await?;

            Ok(())
        }
    }
}

impl Drop for IO {
    fn drop(&mut self) {
        if let Some(runtime) = self.http_runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn is_http(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn validate_content_range(value: &str, expected: &std::ops::Range<usize>) -> anyhow::Result<()> {
    let (unit, value) = value
        .split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("invalid Content-Range header"))?;

    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(anyhow::anyhow!("unsupported Content-Range unit: {unit}"));
    }

    let (bounds, total) = value
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid Content-Range header"))?;

    let (start, end) = bounds
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("invalid Content-Range header"))?;

    let start = start.parse::<usize>()?;
    let end = end.parse::<usize>()?;

    if total != "*" {
        let total = total.parse::<usize>()?;
        if total <= end {
            return Err(anyhow::anyhow!("invalid Content-Range total length"));
        }
    }

    let expected_end = expected.end - 1;
    if start != expected.start || end != expected_end {
        return Err(anyhow::anyhow!(
            "HTTP Content-Range is bytes {}-{}, expected bytes {}-{}",
            start,
            end,
            expected.start,
            expected_end
        ));
    }

    Ok(())
}

fn file_id(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn range_id(s: &str, a: usize, b: usize) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    a.hash(&mut h);
    b.hash(&mut h);
    h.finish()
}
