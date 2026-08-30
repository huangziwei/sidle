//! The LAN server the Kindle pulls from.

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use sidle_core::library::daemon;

use crate::ctx::Ctx;

#[derive(Subcommand)]
pub enum ServerCmd {
    /// Is a daemon serving, on which port, with which token?
    Status {
        #[arg(long, default_value_t = daemon::DEFAULT_PORT)]
        port: u16,
    },
    /// Start one, replacing any daemon already on the port.
    Start {
        #[arg(long, default_value_t = daemon::DEFAULT_PORT)]
        port: u16,
    },
    /// Ask the running daemon to stop.
    Stop {
        #[arg(long, default_value_t = daemon::DEFAULT_PORT)]
        port: u16,
    },
}

#[derive(Serialize)]
struct Status {
    running: bool,
    port: u16,
    pid: Option<i32>,
    token: Option<String>,
    /// A listener that is not a daemon we can verify — a leftover from a
    /// pre-TLS build, or an unrelated process.
    port_held_by_something_else: bool,
}

pub fn run(ctx: &Ctx, cmd: ServerCmd) -> Result<()> {
    match cmd {
        ServerCmd::Status { port } => status(ctx, port),
        ServerCmd::Start { port } => start(ctx, port),
        ServerCmd::Stop { port } => stop(ctx, port),
    }
}

fn observe(ctx: &Ctx, port: u16) -> Status {
    let running = daemon::probe(&ctx.paths, port);
    Status {
        running,
        port,
        pid: running.then(|| daemon::read_pid(&ctx.paths)).flatten(),
        token: sidle_server::load_or_generate_token(&ctx.paths.root).ok(),
        port_held_by_something_else: !running && daemon::port_open(port),
    }
}

fn status(ctx: &Ctx, port: u16) -> Result<()> {
    let status = observe(ctx, port);
    ctx.report(&status, || {
        if status.running {
            println!(
                "serving on :{}{}",
                status.port,
                match status.pid {
                    Some(pid) => format!(" (pid {pid})"),
                    None => String::new(),
                }
            );
            if let Some(token) = &status.token {
                println!("token {token}");
            }
        } else if status.port_held_by_something_else {
            println!(
                "not serving — something else holds :{}, and it cannot present a \
                 certificate this library's CA vouches for",
                status.port
            );
        } else {
            println!("not serving");
        }
    })
}

fn start(ctx: &Ctx, port: u16) -> Result<()> {
    // Replace rather than adopt: a daemon on the port is running whatever code
    // was on disk when it started. The exception is one we cannot name — no PID
    // file means someone else's server, and there is nothing to signal.
    if daemon::port_open(port) {
        if daemon::read_pid(&ctx.paths).is_none() {
            ctx.say(format!(
                "something already holds :{port} and names no pid — leaving it alone"
            ));
            return status(ctx, port);
        }
        daemon::signal_stop(&ctx.paths, port);
        if !daemon::wait_for_port_free(port, std::time::Duration::from_secs(10)) {
            anyhow::bail!("the daemon on :{port} was asked to stop and did not release the port");
        }
    }
    // The child is deliberately dropped: the daemon is detached and outlives
    // this process, which is the whole point of it being a daemon. It writes its
    // own pid file, so a later `stop` still reaches it.
    let child = daemon::start(&ctx.paths, port)?;
    std::mem::forget(child);
    status(ctx, port)
}

fn stop(ctx: &Ctx, port: u16) -> Result<()> {
    if !daemon::signal_stop(&ctx.paths, port) {
        ctx.say("nothing to stop");
        return Ok(());
    }
    let stopped = daemon::wait_for_port_free(port, std::time::Duration::from_secs(10));
    ctx.report(&stopped, || {
        if stopped {
            println!("stopped");
        } else {
            println!("asked it to stop, but :{port} is still held");
        }
    })
}
