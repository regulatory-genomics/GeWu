//! Entry point: parse CLI options and run the GUI server.

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    gewu_gui::run(gewu_gui::Cli::parse()).await
}
