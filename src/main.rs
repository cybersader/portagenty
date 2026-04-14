use clap::Parser;
use portagenty::cli::Cli;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    portagenty::run(cli)
}
