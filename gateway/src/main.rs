//! Roster — the trusted host-side control plane, one binary with subcommands
//! (D20: the language boundary is the trust boundary; TS lives only in the box).
//!
//!   roster serve                       the governed-egress gateway
//!   roster create <name>               scaffold a worker spec
//!   roster deploy                      compile specs → runtime config
//!   roster box [--worker n] [--ceiling m] "<prompt>"   run one pi session
//!   roster connect <provider>          create a credential via its login flow
//!   roster vault-sync                  import an existing pi login into the vault

mod budget;
mod ca;
mod cmd;
mod judge;
mod ledger;
mod providers;
mod proxy;
mod registry;
mod scope;
mod schema;
mod tls;
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
    eprintln!(
        "roster — digital workers with owned governance\n\n\
         usage: roster <command>\n\n\
         commands:\n  \
           serve                       run the governed-egress gateway\n  \
           create <name>               scaffold workers/<name>/worker.toml\n  \
           deploy                      compile org.toml + workers/* → runs/compiled/\n  \
           box [--worker <n>] [--ceiling <m>] \"<prompt>\"   run one pi session in the box\n  \
           connect <provider>          create a credential via its login flow\n  \
           vault-sync                  import an existing pi login into the vault"
    );
}
