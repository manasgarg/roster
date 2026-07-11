//! Roster — the trusted host-side control plane, one binary with subcommands
//! (D20: the language boundary is the trust boundary; TS lives only in the box).
//!
//!   roster serve                       the governed-egress gateway
//!   roster create <name>               scaffold a worker spec
//!   roster deploy                      compile specs → runtime config
//!   roster box [--worker n] [--ceiling m] "<prompt>"   run one pi session
//!   roster connect <provider>          create a credential via its login flow
//!   roster vault-sync                  import an existing pi login into the vault

mod action;
mod budget;
mod ca;
mod cmd;
mod context;
mod discord;
mod gate;
mod journal;
mod judge;
mod knowledge;
mod ledger;
mod memory;
mod providers;
mod proxy;
mod queue;
mod registry;
mod runlog;
mod scope;
mod schema;
mod smtp;
mod storage;
mod tls;
mod trigger;
mod trust;
mod util;
mod vault;

use ca::Ca;
use std::sync::Arc;
use tokio::net::TcpListener;

const ADDR: &str = "0.0.0.0:7300";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("help");

    let result: Result<(), Box<dyn std::error::Error>> = match command {
        "serve" => serve().await,
        "create" => cmd::create::run(&args[2..]),
        "deploy" => cmd::deploy::run(),
        "connect" => cmd::connect::run(&args[2..]).await,
        "vault-sync" => cmd::vault_sync::run(),
        "box" => cmd::run_box::run(&args[2..]).await,
        "gates" => cmd::gates::run(&args[2..]).await,
        "queue" => cmd::queue::run(&args[2..]),
        "supervise" => cmd::supervise::run(&args[2..]).await,
        "relay" => cmd::relay::run(&args[2..]),
        "listen" => cmd::listen::run(&args[2..]).await,
        "memory" => cmd::notes::run(&args[2..]),
        "notes" => {
            eprintln!("warning: `roster notes` is deprecated; use `roster memory`");
            cmd::notes::run(&args[2..])
        }
        "knowledge" => cmd::knowledge::run(&args[2..]),
        "runs" => cmd::runs::run(&args[2..]),
        "channel" => cmd::channel::run(&args[2..]),
        "session" => cmd::session::run(&args[2..]).await,
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command \"{other}\" (try: roster help)").into()),
    };

    if let Err(e) = result {
        eprintln!("roster: {e}");
        std::process::exit(1);
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider().install_default().ok();

    let ca = Arc::new(Ca::ensure()?);
    let tls = tls::acceptor(ca.clone());
    let client = proxy::build_client();

    // Rebuild budget counters from the current window's usage so a restart
    // doesn't reset budgets.
    ledger::rehydrate(&budget::load_budget().limits);

    let listener = TcpListener::bind(ADDR).await?;
    eprintln!("roster serve — listening on {ADDR} (governed egress: judge + inject + budget)");

    loop {
        let (stream, _peer) = listener.accept().await?;
        let (tls, client, ca) = (tls.clone(), client.clone(), ca.clone());
        tokio::spawn(proxy::serve(stream, tls, client, ca));
    }
}

fn print_help() {
    let names = registry::provider_names();
    let providers = if names.is_empty() {
        "  connect <provider>          create a credential via its login flow".to_string()
    } else {
        format!(
            "  connect <provider>          create a credential via its login flow\n{}providers: {}",
            " ".repeat(30),
            names.join(", ")
        )
    };
    eprintln!(
        "roster — digital workers with owned governance\n\n\
         usage: roster <command>\n\n\
         commands:\n  \
           serve                       run the governed-egress gateway\n  \
           create <name>               scaffold workers/<name>/worker.toml\n  \
           deploy                      compile org.toml + workers/* → runs/compiled/\n  \
           box [--worker <n>] [--ceiling <m>] \"<prompt>\"   run one pi session in the box\n  \
           queue [add|ls|show|requeue] file/list/inspect tasks; add supports --reorganize\n  \
           supervise [--cap n] [--once]  dispatch queued tasks to the box\n  \
           relay --worker <n> \"<msg>\"    turn an inbound message into a task\n  \
           listen --worker <n>          run the Discord gateway (inbound)\n  \
           channel [ls|trust|mode|memory]  manage channel behavior\n  \
           memory [ls|show|rm|correct|pin|explain] inspect and repair interaction memory\n  \
           knowledge <worker>           print the worker's Git repository path\n  \
           runs [ls|show|context]      inspect executions and exact compiled context\n  \
           gates [ls|show|approve|deny] approval desk for proposed actions\n\
         {providers}\n  \
           vault-sync                  import an existing pi login into the vault"
    );
}
