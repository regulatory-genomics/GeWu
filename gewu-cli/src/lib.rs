//! Terminal frontends for the same agent service used by the browser.
mod endpoint;
mod exec;

pub use exec::{ExecArgs, exec};

mod chat;
mod commands;
mod interactive;
mod key_entry;
mod view;
mod workflow;
pub use interactive::{CliArgs, cli};
