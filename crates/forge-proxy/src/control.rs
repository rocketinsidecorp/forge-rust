//! The control socket: a Unix socket inside the proxy container (directory
//! 0700, socket 0600), reached with `docker exec forge-proxy forge-proxy
//! …`. There is no network control surface. Each connection carries one
//! JSON request and one JSON response, newline-terminated.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::duration::GoDuration;
use crate::server::Server;
use crate::settings::{DEFAULT_DRAIN_TIMEOUT, DeployRequest, DeployResult, ServiceStatus};

const MAX_CONTROL_REQUEST: u64 = 64 << 10;
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The most control connections handled at once.
const MAX_CONTROL_CONNECTIONS: usize = 64;

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_duration(value: &GoDuration) -> bool {
    value.is_zero()
}

/// One message on the control socket.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    /// `deploy`, `remove`, `maintenance` or `status`.
    #[serde(default)]
    pub op: String,
    /// The deploy request of `deploy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<DeployRequest>,
    /// The service of `remove` and `maintenance`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service: String,
    /// The state `maintenance` sets.
    #[serde(default, skip_serializing_if = "is_false")]
    pub maintenance: bool,
    /// The drain budget of `remove`.
    #[serde(default, skip_serializing_if = "is_zero_duration")]
    pub drain_timeout: GoDuration,
}

/// The answer to a [`ControlRequest`].
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlResponse {
    /// The operation succeeded.
    pub ok: bool,
    /// What went wrong.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// `unhealthy` or `invalid` for a refused deploy.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    /// The completed switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<DeployResult>,
    /// The services of `status`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceStatus>,
}

impl ControlResponse {
    fn error(message: impl Into<String>) -> Self {
        Self {
            error: message.into(),
            ..Self::default()
        }
    }
}

/// Creates the control socket at `path`. Its directory is created 0700 and
/// the socket is 0600, so only the proxy's user (and root on the host,
/// through `docker exec`) can connect. A stale socket left by a previous
/// process is replaced; any other file at `path` is an error.
///
/// # Errors
///
/// The directory or socket cannot be created or restricted.
pub fn listen_control(path: &Path) -> Result<UnixListener, String> {
    use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _, PermissionsExt as _};

    if let Some(directory) = path.parent()
        && !directory.as_os_str().is_empty()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|error| format!("create control directory: {error}"))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(format!(
                    "control path {} exists and is not a socket",
                    path.display()
                ));
            }
            std::fs::remove_file(path)
                .map_err(|error| format!("remove stale control socket: {error}"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect control path: {error}")),
    }
    let listener =
        UnixListener::bind(path).map_err(|error| format!("listen on control socket: {error}"))?;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        return Err(format!("restrict control socket: {error}"));
    }
    Ok(listener)
}

impl Server {
    /// Answers control requests until `shutdown` is cancelled. Connections
    /// are handled concurrently (at most 64), each in a task owned by this
    /// call and finished before it returns.
    pub async fn serve_control(&self, listener: UnixListener, shutdown: CancellationToken) {
        let tasks = TaskTracker::new();
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONTROL_CONNECTIONS));
        loop {
            let permit = tokio::select! {
                () = shutdown.cancelled() => break,
                permit = std::sync::Arc::clone(&permits).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
            let accepted = tokio::select! {
                () = shutdown.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, _)) => {
                    let server = self.clone();
                    let cancelled = shutdown.clone();
                    tasks.spawn(async move {
                        tokio::select! {
                            () = server.handle_control(stream) => {}
                            () = cancelled.cancelled() => {}
                        }
                        drop(permit);
                    });
                }
                Err(error) => {
                    self.logger().warn(
                        "control accept failed",
                        &[("error", crate::log::text(error.to_string()))],
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        tasks.close();
        tasks.wait().await;
    }

    async fn handle_control(&self, stream: UnixStream) {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half.take(MAX_CONTROL_REQUEST));
        let mut line = Vec::new();
        let read =
            tokio::time::timeout(CONTROL_READ_TIMEOUT, reader.read_until(b'\n', &mut line)).await;
        let response = match read {
            Ok(Ok(_)) => match serde_json::from_slice::<ControlRequest>(&line) {
                Ok(request) => self.dispatch(request).await,
                Err(_) => ControlResponse::error("invalid control request"),
            },
            _ => ControlResponse::error("invalid control request"),
        };
        if let Ok(mut encoded) = serde_json::to_vec(&response) {
            encoded.push(b'\n');
            let _ =
                tokio::time::timeout(CONTROL_READ_TIMEOUT, write_half.write_all(&encoded)).await;
            let _ = write_half.shutdown().await;
        }
    }

    async fn dispatch(&self, request: ControlRequest) -> ControlResponse {
        match request.op.as_str() {
            "deploy" => {
                let Some(deploy) = request.deploy else {
                    return ControlResponse::error("deploy request missing");
                };
                match self.deploy(deploy).await {
                    Ok(result) => ControlResponse {
                        ok: true,
                        deploy: Some(result),
                        ..ControlResponse::default()
                    },
                    Err(error) => ControlResponse {
                        code: if error.unhealthy {
                            "unhealthy"
                        } else {
                            "invalid"
                        }
                        .to_owned(),
                        error: error.message,
                        ..ControlResponse::default()
                    },
                }
            }
            "remove" => {
                let drain = if request.drain_timeout.as_nanos() > 0 {
                    request.drain_timeout
                } else {
                    DEFAULT_DRAIN_TIMEOUT
                };
                match self.remove(&request.service, drain.to_std()).await {
                    Ok(()) => ControlResponse {
                        ok: true,
                        ..ControlResponse::default()
                    },
                    Err(error) => ControlResponse::error(error),
                }
            }
            "maintenance" => match self
                .set_maintenance(&request.service, request.maintenance)
                .await
            {
                Ok(()) => ControlResponse {
                    ok: true,
                    ..ControlResponse::default()
                },
                Err(error) => ControlResponse::error(error),
            },
            "status" => ControlResponse {
                ok: true,
                services: self.status(),
                ..ControlResponse::default()
            },
            _ => ControlResponse::error("unknown op"),
        }
    }
}

/// Sends `request` to the control socket at `path` and returns the
/// response, waiting at most `timeout`. A response with `ok: false` is
/// returned as such; an error means the exchange itself failed.
///
/// # Errors
///
/// The socket cannot be reached, or the answer is missing or malformed.
pub async fn call(
    path: &Path,
    request: &ControlRequest,
    timeout: Duration,
) -> Result<ControlResponse, String> {
    let exchange = async {
        let mut stream = UnixStream::connect(path)
            .await
            .map_err(|error| format!("connect to forge-proxy control socket: {error}"))?;
        let mut encoded = serde_json::to_vec(request).map_err(|error| error.to_string())?;
        encoded.push(b'\n');
        stream
            .write_all(&encoded)
            .await
            .map_err(|error| format!("send control request: {error}"))?;
        let mut answer = Vec::new();
        stream
            .take(MAX_CONTROL_REQUEST * 16)
            .read_to_end(&mut answer)
            .await
            .map_err(|error| format!("read control response: {error}"))?;
        serde_json::from_slice::<ControlResponse>(&answer)
            .map_err(|error| format!("read control response: {error}"))
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| "read control response: i/o timeout".to_owned())?
}
