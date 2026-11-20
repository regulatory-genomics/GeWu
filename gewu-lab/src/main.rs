use clap::Parser;
use gewu_lab::{Lab, Project, Task, gui::LabGui};
use gewu_server::{
    bootstrap::{CommonArgs, build_provider},
    config::LoadedConfig,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    demo: bool,
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    #[arg(long, default_value_t = 8440)]
    port: u16,
    #[arg(long, env = "GEWU_LAB_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long)]
    config: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut lab = Lab::new();
    if args.demo {
        let mut project = Project::new();
        project.add_task(
            Task::new("outline", "Draft an outline", "Outline"),
            Default::default(),
        )?;
        project.add_task(
            Task::new("report", "Write the report", "Report"),
            ["outline".into()].into(),
        )?;
        lab.add_project("Example project", project)?;
    }
    let chat = if args.token.is_some() || args.config.is_some() {
        let loaded = LoadedConfig::load(args.config.as_deref())?;
        match loaded.config.model.clone() {
            Some(model) => Some(
                build_provider(
                    &CommonArgs {
                        model: Some(model),
                        ..Default::default()
                    },
                    &loaded.config,
                )?
                .0,
            ),
            None if args.config.is_some() => {
                anyhow::bail!("set the top-level model in the Lab config")
            }
            None => None,
        }
    } else {
        None
    };
    let mut gui = LabGui::bind_to(
        Arc::new(Mutex::new(lab)),
        SocketAddr::new(args.host, args.port),
        args.token,
    )
    .await?;
    if let Some(provider) = chat {
        gui = gui.with_chat(provider)?;
    }
    println!("Listening on {}", gui.url());
    gui.serve().await?;
    Ok(())
}
