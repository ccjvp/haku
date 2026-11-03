use crate::args::Args;
use crate::io::IO;
use hyper::{Body, Request, Response, Server, StatusCode, body, header, service};
use std::{convert::Infallible, net::SocketAddr, sync::Arc};

pub async fn serve(io: Arc<IO>, args: Arc<Args>) -> anyhow::Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], 3000));
    let max_upload_size = args.max_chunk_size;

    let make_service = service::make_service_fn(move |_| {
        let io = io.clone();
        let args = args.clone();

        async move {
            Ok::<_, Infallible>(service::service_fn(move |request| {
                handle_request(io.clone(), args.clone(), max_upload_size, request)
            }))
        }
    });

    tracing::info!(%address, "HTTP server started");
    Server::bind(&address).serve(make_service).await?;

    Ok(())
}

async fn handle_request(
    io: Arc<IO>,
    args: Arc<Args>,
    max_upload_size: usize,
    request: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    if request.method() != hyper::Method::POST {
        return Ok(response(StatusCode::METHOD_NOT_ALLOWED));
    }

    let name = request.uri().path().trim_start_matches('/').to_owned();
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Ok(response(StatusCode::BAD_REQUEST));
    }

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

    let path = args.wal_dir.join(name);
    let path = path.to_str().unwrap();
    Ok(match io.write(path, &body).await {
        Ok(()) => response(StatusCode::CREATED),
        Err(error) => {
            tracing::error!(%error, %path, "failed to save upload");
            response(StatusCode::INTERNAL_SERVER_ERROR)
        }
    })
}

fn response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}
