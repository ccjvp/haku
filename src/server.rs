use haku::Args;
use haku::BM25;
use haku::IO;
use hyper::Method;
use hyper::{Body, Request, Response, Server, StatusCode, body, header, service};
use std::{convert::Infallible, net::SocketAddr, sync::Arc};

pub async fn serve(io: Arc<IO>, args: Arc<Args>, bm25: Arc<BM25>) -> anyhow::Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], 3000));
    let max_upload_size = args.max_chunk_size;

    let make_service = service::make_service_fn(move |_| {
        let io = io.clone();
        let args = args.clone();
        let bm25 = bm25.clone();

        async move {
            Ok::<_, Infallible>(service::service_fn(move |request| {
                handle_request(
                    io.clone(),
                    args.clone(),
                    bm25.clone(),
                    max_upload_size,
                    request,
                )
            }))
        }
    });

    tracing::info!(%address, "HTTP server started");
    Server::bind(&address).serve(make_service).await?;

    Ok(())
}

fn get_name(request: &Request<Body>) -> Option<String> {
    let name = request.uri().path().trim_start_matches('/').to_owned();

    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return None;
    }

    Some(name)
}

fn get_search_query(request: &Request<Body>) -> Option<String> {
    if request.uri().path() != "/search" {
        return None;
    }

    let query = request.uri().query()?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "q")
        .map(|(_, value)| value.into_owned())
}

async fn handle_post(
    io: Arc<IO>,
    args: Arc<Args>,
    max_upload_size: usize,
    request: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    let Some(name) = get_name(&request) else {
        return Ok(response(StatusCode::BAD_REQUEST));
    };

    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().ok())
        .flatten();

    if content_type.is_none_or(|v| !v.starts_with("text/")) {
        return Ok(response(StatusCode::UNSUPPORTED_MEDIA_TYPE));
    }

    let body = match body::to_bytes(request.into_body()).await {
        Ok(body) if body.len() <= max_upload_size => body,
        Ok(_) => {
            return Ok(response(StatusCode::PAYLOAD_TOO_LARGE));
        }
        Err(error) => {
            tracing::warn!(%error, "failed to read upload body");
            return Ok(response(StatusCode::BAD_REQUEST));
        }
    };

    let wal_path = args.wal_dir.join(name);
    let wal_path = wal_path.to_str().unwrap();
    Ok(match io.write(wal_path, &body).await {
        Ok(()) => response(StatusCode::CREATED),
        Err(error) => {
            tracing::error!(%error, %wal_path, "failed to save upload");
            response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    })
}

async fn handle_get(bm25: Arc<BM25>, request: Request<Body>) -> Result<Response<Body>, Infallible> {
    let Some(query) = get_search_query(&request) else {
        return Ok(response(StatusCode::BAD_REQUEST));
    };

    match bm25.search(&query) {
        Ok(docs) => match serde_json::to_vec(&docs) {
            Ok(body) => Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap()),

            Err(error) => {
                tracing::error!(%error, "failed to serialize search results");
                Ok(response(StatusCode::INTERNAL_SERVER_ERROR))
            }
        },
        Err(_) => Ok(response(StatusCode::INTERNAL_SERVER_ERROR)),
    }
}

async fn handle_request(
    io: Arc<IO>,
    args: Arc<Args>,
    bm25: Arc<BM25>,
    max_upload_size: usize,
    request: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    match request.method() {
        &Method::POST => handle_post(io, args, max_upload_size, request).await,
        &Method::GET => handle_get(bm25, request).await,
        _ => Ok(response(StatusCode::METHOD_NOT_ALLOWED)),
    }
}

fn response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}
