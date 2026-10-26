//! Minimal Forge application.
//!
//! Run the server (binds loopback by default):
//!
//! ```text
//! FORGE_HTTP_ADDR=127.0.0.1:3000 cargo run -p forge --example serve
//! curl http://127.0.0.1:3000/hello
//! ```
//!
//! Probe it like a container `HEALTHCHECK` would, then stop it with
//! `SIGTERM` or Ctrl-C for a graceful shutdown:
//!
//! ```text
//! FORGE_HTTP_ADDR=127.0.0.1:3000 cargo run -p forge --example serve -- healthcheck
//! ```

use std::process::ExitCode;

use forge::{
    App,
    http::{Method, Response, StatusCode},
};

fn main() -> ExitCode {
    App::new("serve-example")
        .version(env!("CARGO_PKG_VERSION"))
        .routes(|router| {
            router.route(Method::GET, "/hello", |_| async {
                Response::new(StatusCode::OK, "hello from forge\n")
            })?;
            Ok(())
        })
        .run()
}
