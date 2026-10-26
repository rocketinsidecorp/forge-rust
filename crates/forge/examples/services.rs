//! Rate limiting, caching and file storage in one Forge application
//! (ADR 0027, 0028, 0029).
//!
//! ```text
//! export FORGE_STORAGE_ROOT=/tmp/forge-services-storage
//! export FORGE_STORAGE_SIGNING_KEY=$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')
//! FORGE_HTTP_ADDR=127.0.0.1:3000 \
//!   cargo run -p forge --example services --features "ratelimit cache storage"
//! ```
//!
//! The README walks through the `curl` commands for each route.

use std::{
    convert::Infallible,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use forge::{
    App, ComposeError,
    cache::{MemoryCache, MemoryOptions as CacheOptions, fetch, key},
    http::{Method, Request, Response, StatusCode},
    ratelimit::{Limit, MemoryLimiter, MemoryOptions as LimiterOptions, RateLimit, by_client_ip},
    storage::{
        DiskStorage, HandlerOptions, SignOptions, Signer, Storage, StorageHandler, UploadOptions,
        receive,
    },
};

fn main() -> ExitCode {
    App::new("services-example")
        .version(env!("CARGO_PKG_VERSION"))
        .compose(|composition| {
            let config = composition.config().clone();

            // Rate limiting: three comments per minute per client address.
            let limiter = Arc::new(
                MemoryLimiter::new(LimiterOptions::from_config(&config.rate_limit))
                    .map_err(ComposeError::other)?,
            );
            let comments = RateLimit::new(
                limiter,
                "comments.create",
                Limit::new(3, Duration::from_secs(60)).map_err(ComposeError::other)?,
                by_client_ip,
            )
            .map_err(ComposeError::other)?;

            // Caching: a price computed once per ten minutes, however many requests ask.
            let cache = Arc::new(
                MemoryCache::new(CacheOptions::from_config(&config.cache))
                    .map_err(ComposeError::other)?,
            );
            let computations = Arc::new(AtomicUsize::new(0));

            // Storage: avatars on disk, served through signed links.
            let store: Arc<dyn Storage> =
                Arc::new(DiskStorage::from_config(&config.storage).map_err(ComposeError::other)?);
            let signer =
                Arc::new(Signer::from_config(&config.storage).map_err(ComposeError::other)?);
            let link_ttl = config.storage.link_ttl;
            let max_upload = config.storage.max_upload_bytes;

            let router = composition.router();
            router.layer(
                StorageHandler::new(store.clone(), signer.clone(), HandlerOptions::default())
                    .map_err(ComposeError::other)?,
            );
            router.route(
                Method::POST,
                "/comments",
                comments.wrap(|_request: Request| async {
                    Response::new(StatusCode::CREATED, "comment created\n")
                }),
            )?;
            router.route(Method::GET, "/price/{sku}", move |request: Request| {
                let (cache, computations) = (Arc::clone(&cache), Arc::clone(&computations));
                async move {
                    let Ok(sku) = request.path_param_as::<String>("sku") else {
                        return Response::new(StatusCode::BAD_REQUEST, "bad sku\n");
                    };
                    let value = fetch(
                        cache.as_ref(),
                        &key("price", &[&sku]),
                        Some(Duration::from_secs(600)),
                        || async {
                            let n = computations.fetch_add(1, Ordering::SeqCst) + 1;
                            Ok::<_, Infallible>(format!("computation #{n}").into_bytes())
                        },
                    )
                    .await;
                    match value {
                        Ok(bytes) => Response::new(
                            StatusCode::OK,
                            format!("{sku}: cached from {}\n", String::from_utf8_lossy(&bytes)),
                        ),
                        Err(_) => Response::new(StatusCode::INTERNAL_SERVER_ERROR, "error\n"),
                    }
                }
            })?;
            router.route(Method::POST, "/avatars", move |request: Request| {
                let (store, signer) = (Arc::clone(&store), Arc::clone(&signer));
                async move {
                    let options = match UploadOptions::new(
                        "avatar",
                        &["image/png", "image/jpeg"],
                        max_upload,
                    ) {
                        Ok(options) => options.prefix("avatars"),
                        Err(error) => return error.into_response(&request),
                    };
                    let received = match receive(&request, store.as_ref(), &options).await {
                        Ok(received) => received,
                        Err(error) => return error.into_response(&request),
                    };
                    let Some(file) = received.files.first() else {
                        return Response::new(StatusCode::BAD_REQUEST, "no file\n");
                    };
                    match signer.url(
                        "/files",
                        &file.key,
                        &SignOptions::default().ttl(link_ttl).inline(true),
                    ) {
                        Ok(url) => Response::new(StatusCode::CREATED, format!("{url}\n")),
                        Err(error) => error.into_response(&request),
                    }
                }
            })?;
            Ok(())
        })
        .run()
}
