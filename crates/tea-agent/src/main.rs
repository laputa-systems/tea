//! The `tea` binary entrypoint.

fn main() -> std::process::ExitCode {
    tea_agent::cli::run(std::env::args_os())
}
