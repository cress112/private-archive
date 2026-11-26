use clap::Parser;
use private_archive::Cli;
fn main() -> std::process::ExitCode {
    let prepared = Cli::try_parse().map(Cli::prepare);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime");
    let code = runtime.block_on(async {
        match prepared {
            Ok(Ok(command)) => private_archive::execute(command).await,
            Ok(Err(error)) => private_archive::display_preparation_error(error).await,
            Err(error) => private_archive::display_parse_error(error).await,
        }
    });
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::ExitCode::from(code)
}
