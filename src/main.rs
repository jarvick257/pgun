mod config;
mod manager;
mod tunnel;
mod ui;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::stdout;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pgun", about = "Portal gun for ssh tunnels — TUI tunnel manager")]
struct Cli {
    /// Path to config file (default: $XDG_CONFIG_HOME/pgun/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg_path = match cli.config {
        Some(p) => p,
        None => config::default_config_path()?,
    };
    let cfg = config::load(&cfg_path)
        .with_context(|| format!("loading config from {}", cfg_path.display()))?;

    let mut app = ui::App::new(cfg, cfg_path);

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend)?;

    let run_res = ui::run(&mut term, &mut app).await;

    app.mgr.shutdown_all();
    app.mgr.await_all().await;

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;

    run_res
}
