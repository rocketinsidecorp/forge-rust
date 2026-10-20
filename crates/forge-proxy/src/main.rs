//! `forge-proxy`: the reverse proxy `forge deploy` runs on each web host.
//! It routes by Host, obtains TLS certificates through ACME, and switches
//! traffic between application containers without dropping requests. See
//! docs/deployment.md and ADR 0042.

use std::process::ExitCode;

use tokio_util::sync::CancellationToken;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("forge-proxy: start runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = runtime.block_on(async {
        let shutdown = CancellationToken::new();
        let signals = shutdown.clone();
        let main = forge_proxy::cli::main(
            &args,
            Box::new(std::io::stdout()),
            Box::new(std::io::stderr()),
            shutdown.clone(),
        );
        tokio::pin!(main);
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("forge-proxy: install signal handler: {error}");
                    return 1;
                }
            };
        loop {
            tokio::select! {
                code = &mut main => return code,
                _ = tokio::signal::ctrl_c() => signals.cancel(),
                _ = terminate.recv() => signals.cancel(),
            }
        }
    });
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
