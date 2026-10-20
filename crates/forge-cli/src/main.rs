use std::process::ExitCode;

fn main() -> ExitCode {
    match forge_cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // `forge doctor` has printed its findings already.
            if !matches!(error, forge_cli::CliError::Reported) {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}
