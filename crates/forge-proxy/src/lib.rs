//! forge-proxy, the reverse proxy that `forge deploy` runs on every web host
//! (ADR 0042, the Rust port of Forge-Go's ADR 0030). It routes requests by
//! `Host` to the current target container of each service and switches
//! targets without dropping requests: a new target receives traffic only
//! after its health check passed several times in a row, and the previous
//! target is drained before the deploy command returns.
//!
//! The proxy is controlled through a Unix socket inside its container (see
//! [`control::listen_control`]); it has no network-reachable control
//! surface. Its command line, control protocol, state file and access log
//! are Forge-Go's, so either framework's `forge deploy` drives either
//! proxy image.

pub mod acme;
pub mod cli;
pub mod clock;
pub mod control;
pub mod der;
pub mod duration;
mod flags;
pub mod forward;
mod listen;
pub mod log;
pub mod server;
pub mod settings;
pub mod tls;

pub use control::{ControlRequest, ControlResponse, call, listen_control};
pub use duration::GoDuration;
pub use listen::HttpLimits;
pub use server::{DeployError, Server};
pub use settings::{DeployRequest, DeployResult, ServiceStatus};

/// Serves plain HTTP on `listener` until `shutdown` is cancelled (what
/// `forge-proxy run` does on `-http-addr`).
pub async fn serve_plain(
    server: Server,
    listener: tokio::net::TcpListener,
    limits: HttpLimits,
    shutdown: tokio_util::sync::CancellationToken,
) {
    listen::serve(
        server,
        listener,
        std::sync::Arc::new(listen::Plain),
        limits,
        shutdown,
    )
    .await;
}

/// Serves HTTPS on `listener` with the certificates of `tls` until
/// `shutdown` is cancelled (what `forge-proxy run` does on `-https-addr`).
pub async fn serve_tls(
    server: Server,
    listener: tokio::net::TcpListener,
    tls: &tls::Tls,
    limits: HttpLimits,
    shutdown: tokio_util::sync::CancellationToken,
) {
    listen::serve(
        server,
        listener,
        std::sync::Arc::new(tls.acceptor()),
        limits,
        shutdown,
    )
    .await;
}
