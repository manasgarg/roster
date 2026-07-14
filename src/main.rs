//! Impyard — the trusted host-side control plane, one binary (D20: the language
//! boundary is the trust boundary; TS lives only in the box). The command
//! grammar is the product thesis — rented intelligence, owned governance:
//!
//!   impyard server …   the owned machinery: daemon, config, desk, channels, vault, run log
//!   impyard imp …   the governed identities: lifecycle, trust, memory, work, sessions

mod action;
mod channel;
mod cli;
mod config;
mod credential;
mod gateway;
mod imp;
mod paths;
mod run;
mod util;
mod work;

use clap::{Parser, Subcommand};

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("IMPYARD_BUILD"), ")");

#[derive(Parser)]
#[command(
    name = "impyard",
    version = VERSION,
    about = "impyard — digital imps with owned governance"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize the deployment: config, data, and state roots (XDG)
    Init,
    /// The owned machinery: daemon, config, approval desk, channels, vault
    #[command(subcommand)]
    Server(ServerCmd),
    /// The governed identities: lifecycle, trust, memory, knowledge, work
    #[command(subcommand)]
    Imp(ImpCmd),
}

#[derive(Subcommand)]
enum ServerCmd {
    /// Run the daemon in the foreground: gateway, task dispatch, channel listeners
    #[command(alias = "run")]
    Start {
        /// Max concurrent boxes
        #[arg(long, default_value_t = 3)]
        cap: usize,
        /// Skip channel listeners (dev: never double-connect a bot)
        #[arg(long)]
        no_listen: bool,
        /// Fire due triggers, drain due tasks, then exit
        #[arg(long)]
        once: bool,
        /// Gateway listen address
        #[arg(long, default_value = "0.0.0.0:7300")]
        addr: String,
    },
    /// Daemon health: components, queue, gates, compiled config
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Parse and check all config; print every error (config loads live)
    #[command(alias = "deploy")]
    Validate,
    /// Connect a service in one step: login, vault, scaffolded connection file
    Connect {
        /// Catalog name (bare: list the catalog)
        service: Option<String>,
        /// Grant to this imp (repeatable); default is to ask
        #[arg(long)]
        imp: Vec<String>,
        /// Org-wide: every current and future imp (the explicit escalation)
        #[arg(long)]
        org: bool,
        /// Connection/credential name when it differs from the service
        /// (e.g. --as github-yuko for a per-imp identity)
        #[arg(long = "as")]
        alias: Option<String>,
    },
    /// Service connections: scope, hosts, env, and whether each is active
    Connections {
        #[arg(long)]
        json: bool,
    },
    /// The approval desk: list, inspect, approve, deny
    #[command(subcommand)]
    Gates(GatesCmd),
    /// Channel edges: trust designation, response mode, memory settings
    #[command(subcommand)]
    Channel(ChannelCmd),
    /// Credentials — held on the host, injected in transit; imps never see keys
    #[command(subcommand)]
    Vault(VaultCmd),
    /// The run log: every session, whoever it was attributed to
    #[command(subcommand)]
    Runs(RunsCmd),
}

#[derive(Subcommand)]
enum GatesCmd {
    /// Pending gates
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// The exact action that would execute (charter/code gates show a diff)
    Show {
        /// Gate id (any unique prefix)
        id: String,
    },
    /// Approve and execute the gated action idempotently
    Approve {
        /// Gate id (any unique prefix)
        id: String,
        /// Note recorded with the decision
        note: Option<String>,
    },
    /// Record the refusal
    Deny {
        /// Gate id (any unique prefix)
        id: String,
        /// Note recorded with the decision
        note: Option<String>,
    },
}

#[derive(Subcommand)]
enum ChannelCmd {
    /// All configured channels
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// One channel's settings, readable
    Show { channel_id: String },
    /// Trust a channel: its participants may administer; replies need no gate
    Trust { channel_id: String },
    /// Untrust a channel: participants are content-only
    Untrust { channel_id: String },
    /// Tune a setting: mode, memory, memory-inferred, memory-kinds,
    /// memory-retention, memory-notes, memory-chars
    Set {
        channel_id: String,
        key: String,
        value: String,
    },
}

#[derive(Subcommand)]
enum VaultCmd {
    /// Create a credential via a provider's login flow (no argument: list providers)
    #[command(after_help = credential::connect::provider_help())]
    Connect { provider: Option<String> },
    /// Import an existing pi login into the vault
    Sync,
    /// Credential names and types (never values)
    Ls {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ImpCmd {
    /// Scaffold an imp: spec, identity, knowledge repo
    Init { name: String },
    /// List imps and their state
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// One imp: spec, budgets and spend, queue, gates, memory, knowledge
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Per-action trust: grants, earned history, promotion rules
    Trust {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// One governed session, now, bypassing the queue
    Run {
        name: String,
        /// Wall-clock ceiling in minutes
        #[arg(long, default_value_t = 30.0)]
        ceiling: f64,
        #[arg(required = true, value_name = "PROMPT")]
        prompt: Vec<String>,
    },
    /// Interactive warm session fed from stdin, one message per turn
    Chat {
        name: String,
        /// End after this much quiet, in seconds
        #[arg(long, default_value_t = 20)]
        idle: u64,
    },
    /// Its durable work: add, relay, ls, show, requeue
    #[command(subcommand)]
    Task(TaskCmd),
    /// Inspect and repair interaction memory
    #[command(subcommand)]
    Memory(MemoryCmd),
    /// Print the knowledge repo path (then use git)
    Knowledge { name: String },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// File a task for an imp
    Add {
        imp: String,
        /// Wall-clock ceiling in minutes
        #[arg(long, default_value_t = 30.0)]
        ceiling: f64,
        /// Budget-gated at dispatch; admin-filed work always runs
        #[arg(long)]
        proactive: bool,
        /// Exclusive knowledge reorganization
        #[arg(long, conflicts_with = "repo")]
        reorganize: bool,
        /// Code task in a worktree of this git repo
        #[arg(long)]
        repo: Option<String>,
        /// Base ref for --repo
        #[arg(long, default_value = "main")]
        base: String,
        /// The prompt (bare words are joined)
        #[arg(required = true, value_name = "PROMPT")]
        prompt: Vec<String>,
    },
    /// File an inbound message as a task (untrusted-content framing)
    Relay {
        imp: String,
        /// Sender label recorded with the task
        #[arg(long)]
        from: Option<String>,
        #[arg(required = true, value_name = "MESSAGE")]
        message: Vec<String>,
    },
    /// List tasks, newest first
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Inspect a task: state, gates, journal, prompt
    Show {
        /// Task id (any unique prefix)
        id: String,
    },
    /// Put a stuck or finished task back to waiting
    Requeue {
        /// Task id (any unique prefix)
        id: String,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// List memory notes
    Ls {
        imp: String,
        /// Filter: imp | channel | user
        #[arg(long)]
        scope: Option<String>,
        /// Filter: the scope's id (a channel id, a user id)
        #[arg(long)]
        scope_id: Option<String>,
    },
    /// Print one note in full
    Show {
        imp: String,
        id: String,
    },
    /// Replace a note's content (recorded, not a silent edit)
    Correct {
        imp: String,
        id: String,
        #[arg(required = true, value_name = "REPLACEMENT")]
        replacement: Vec<String>,
    },
    /// Remove a note
    Rm {
        imp: String,
        id: String,
    },
    Pin {
        imp: String,
        id: String,
    },
    Unpin {
        imp: String,
        id: String,
    },
    Disable {
        imp: String,
        id: String,
    },
    Enable {
        imp: String,
        id: String,
    },
    /// Drop dead notes, keep live ones (finishes the notes/ → memory/ migration)
    Compact {
        imp: String,
    },
}

#[derive(Subcommand)]
enum RunsCmd {
    /// List past sessions, whoever they were attributed to
    Ls {
        #[arg(long)]
        imp: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// One session: transcript, journal, knowledge commits, files
    Show {
        /// Run id (any unique prefix)
        run: String,
    },
    /// The exact compiled prompts a session saw
    Context {
        /// Run id (any unique prefix)
        run: String,
        /// Every turn, not just the last
        #[arg(long)]
        all: bool,
    },
    /// The memory recall trace
    Recall {
        /// Run id (any unique prefix)
        run: String,
    },
}

#[tokio::main]
async fn main() {
    // Die quietly on a closed pipe (`impyard … | head`) instead of panicking:
    // Rust ignores SIGPIPE by default, turning EPIPE into a println panic.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Old top-level commands point at their new home instead of half-working:
    // the argument shapes changed (positional imps, merged daemons), so a
    // silent translation could misparse. One clear error, one clear fix.
    if let Some(first) = std::env::args().nth(1) {
        if let Some(new_home) = legacy_pointer(&first) {
            eprintln!("impyard: `impyard {first}` has moved — use: {new_home}");
            std::process::exit(2);
        }
    }

    let cli = Cli::parse();
    let result: Result<(), Box<dyn std::error::Error>> = match cli.command {
        Cmd::Init => cli::init::run(),
        Cmd::Server(cmd) => match cmd {
            ServerCmd::Start {
                cap,
                no_listen,
                once,
                addr,
            } => cli::server::run(cap, once, no_listen, &addr).await,
            ServerCmd::Status { json } => cli::server::status(json).await,
            ServerCmd::Validate => cli::server::validate(),
            ServerCmd::Connect {
                service,
                imp,
                org,
                alias,
            } => cli::connections::connect(service, imp, org, alias).await,
            ServerCmd::Connections { json } => cli::connections::ls(json),
            ServerCmd::Gates(cmd) => match cmd {
                GatesCmd::Ls { json } => cli::gates::ls(json),
                GatesCmd::Show { id } => cli::gates::show(&id),
                GatesCmd::Approve { id, note } => cli::gates::approve(&id, note.as_deref()).await,
                GatesCmd::Deny { id, note } => cli::gates::deny(&id, note.as_deref()),
            },
            ServerCmd::Channel(cmd) => match cmd {
                ChannelCmd::Ls { json } => cli::channel::ls(json),
                ChannelCmd::Show { channel_id } => cli::channel::show(&channel_id),
                ChannelCmd::Trust { channel_id } => cli::channel::set_trust(&channel_id, true),
                ChannelCmd::Untrust { channel_id } => cli::channel::set_trust(&channel_id, false),
                ChannelCmd::Set {
                    channel_id,
                    key,
                    value,
                } => cli::channel::set(&channel_id, &key, &value),
            },
            ServerCmd::Vault(cmd) => match cmd {
                VaultCmd::Connect { provider } => match provider {
                    Some(provider) => credential::connect::run(&provider).await,
                    None => credential::connect::list(),
                },
                VaultCmd::Sync => cli::vault::run(),
                VaultCmd::Ls { json } => cli::vault::ls(json),
            },
            ServerCmd::Runs(cmd) => match cmd {
                RunsCmd::Ls { imp, limit, json } => cli::runs::ls(imp.as_deref(), limit, json),
                RunsCmd::Show { run } => cli::runs::show(&run),
                RunsCmd::Context { run, all } => cli::runs::context(&run, all),
                RunsCmd::Recall { run } => cli::runs::recall(&run),
            },
        },
        Cmd::Imp(cmd) => match cmd {
            ImpCmd::Init { name } => cli::create::run(&name),
            ImpCmd::Ls { json } => cli::imp::ls(json),
            ImpCmd::Show { name, json } => cli::imp::show(&name, json),
            ImpCmd::Trust { name, json } => cli::imp::trust(&name, json),
            ImpCmd::Run {
                name,
                ceiling,
                prompt,
            } => run::boxed::run_once(&name, ceiling, prompt.join(" ")).await,
            ImpCmd::Chat { name, idle } => run::session::chat(&name, idle).await,
            ImpCmd::Task(cmd) => match cmd {
                TaskCmd::Add {
                    imp,
                    ceiling,
                    proactive,
                    reorganize,
                    repo,
                    base,
                    prompt,
                } => cli::task::add(
                    &imp,
                    ceiling,
                    proactive,
                    reorganize,
                    repo,
                    &base,
                    prompt.join(" "),
                ),
                TaskCmd::Relay { imp, from, message } => {
                    channel::relay::run(&imp, from.as_deref(), message.join(" "))
                }
                TaskCmd::Ls { json } => cli::task::ls(json),
                TaskCmd::Show { id } => cli::task::show(&id),
                TaskCmd::Requeue { id } => cli::task::requeue(&id),
            },
            ImpCmd::Memory(cmd) => match cmd {
                MemoryCmd::Ls {
                    imp,
                    scope,
                    scope_id,
                } => cli::memory::ls(&imp, scope.as_deref(), scope_id.as_deref()),
                MemoryCmd::Show { imp, id } => cli::memory::show(&imp, &id),
                MemoryCmd::Correct {
                    imp,
                    id,
                    replacement,
                } => cli::memory::correct(&imp, &id, &replacement.join(" ")),
                MemoryCmd::Rm { imp, id } => cli::memory::mutate("forget", &imp, &id),
                MemoryCmd::Pin { imp, id } => cli::memory::mutate("pin", &imp, &id),
                MemoryCmd::Unpin { imp, id } => cli::memory::mutate("unpin", &imp, &id),
                MemoryCmd::Disable { imp, id } => cli::memory::mutate("disable", &imp, &id),
                MemoryCmd::Enable { imp, id } => cli::memory::mutate("enable", &imp, &id),
                MemoryCmd::Compact { imp } => cli::memory::compact(&imp),
            },
            ImpCmd::Knowledge { name } => cli::knowledge::run(&name),
        },
    };

    if let Err(e) = result {
        eprintln!("impyard: {e}");
        std::process::exit(1);
    }
}

/// Where each pre-clap command lives now. Kept until the muscle memory fades.
fn legacy_pointer(first: &str) -> Option<&'static str> {
    Some(match first {
        "serve" => "impyard server start",
        "supervise" => "impyard server start  (the daemons merged; --cap and --once moved there)",
        "listen" => "impyard server start  (listeners start for every imp with a [channels] entry)",
        "deploy" => "impyard server validate  (config now loads live — there is no deploy step)",
        "gates" => "impyard server gates <ls|show|approve|deny>",
        "channel" => "impyard server channel <ls|show|trust|untrust|set>",
        "connect" => "impyard server vault connect <provider>",
        "vault-sync" => "impyard server vault sync",
        "create" => "impyard imp init <name>",
        "queue" => "impyard imp task <add|relay|ls|show|requeue>",
        "relay" => "impyard imp task relay <imp> \"<message>\"",
        "memory" | "notes" => "impyard imp memory <ls|show|correct|rm|…> <imp>",
        "knowledge" => "impyard imp knowledge <name>",
        "box" => "impyard imp run <name> \"<prompt>\"",
        "session" => "impyard imp chat <name>",
        "runs" => "impyard server runs <ls|show|context|recall>",
        "agent" => {
            "impyard imp <run|chat>, impyard server runs  (sessions belong to imps; the log to the server)"
        }
        _ => return None,
    })
}
