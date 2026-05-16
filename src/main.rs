mod cli;
mod command;
mod config;
mod git;
mod output;
mod targets;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
