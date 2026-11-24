//! newm-river: newm's strict tile canvas as a river window manager.
//!
//! river composites and talks to the apps; this client decides every window's tiles, its size, its
//! place on screen and the keyboard focus over river-window-management-v1. The tile rules are
//! `newm-canvas`'s; the config is niri's KDL with newm's `canvas` settings.
//!
//! Modules: `model` (the decisions, no Wayland), `rules` (window rules), `config` (what the
//! config gives), `glow` (the focus glow's pixels), `river` (the protocol glue and main loop),
//! `input` (keymap, repeat, libinput), `ipc` (niri's socket protocol), `render` (what needs the
//! render patch), `pointer` (Super drag, pan and tap, scrolling and swipes, no Wayland) and
//! `pointer_wl` (their bindings), `screencast` (Spectacle's recordings) with `pw` (their PipeWire
//! streams) and `kwin` (the window uuids Spectacle records by). `supervise` runs the window
//! manager as a child and starts it again after a crash.

mod anim;
mod block;
mod capture;
mod config;
mod draw;
mod glow;
mod icons;
mod input;
mod ipc;
mod kde;
mod kwin;
mod model;
mod outputs;
mod overview;
mod placeholder;
mod pointer;
mod pointer_wl;
mod protocols;
mod pw;
mod reload;
mod render;
mod river;
mod rules;
mod screencast;
mod screenshot;
mod session;
mod shell;
mod supervise;
mod switcher;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(version, about = "newm's tile canvas as a river window manager")]
struct Cli {
    /// Config file (default: $NIRI_CONFIG, else ~/.config/newm-river/config.kdl).
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(Subcommand, Debug)]
enum Sub {
    /// Check the config and exit.
    Validate,
    /// Talk to the running newm-river over $NIRI_SOCKET.
    Msg {
        /// Print the reply as JSON.
        #[arg(short, long)]
        json: bool,
        #[command(subcommand)]
        msg: ipc::MsgCmd,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "newm_river=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Sub::Validate) => {
            let path = config::path(cli.config);
            river::validate(&path)?;
            println!("config {} is valid", path.display());
            Ok(())
        }
        Some(Sub::Msg { json, msg }) => ipc::msg(msg, json),
        // The window manager runs as the supervisor's child, started again after a crash.
        None if std::env::var_os(supervise::CHILD_ENV).is_none() => {
            std::process::exit(supervise::run()?)
        }
        None => {
            newm_kde::apps::enable();
            river::run(&config::path(cli.config))
        }
    }
}
