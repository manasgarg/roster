//! Roster — the trusted host-side control plane, one binary (D20: the language
//! boundary is the trust boundary; TS lives only in the box). The command
//! grammar is the product thesis — rented intelligence, owned governance:
//!
//!   roster server …       the owned machinery: daemon, config, desk, channels, run log
//!   roster connection …   the org's relationships with external services
//!   roster worker …       the governed identities: lifecycle, trust, memory, work, sessions

mod action;
mod channel;
mod cli;
mod config;
mod credential;
mod gateway;
mod worker;
mod paths;
mod run;
mod statefile;
mod util;
mod work;

use clap::{Parser, Subcommand};

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("ROSTER_BUILD"), ")");

#[derive(Parser)]
#[command(
    name = "roster",
    version = VERSION,
    about = "roster — digital workers with owned governance",
    after_help = "quickstart: roster talk   (first run: creates a worker and opens a chat)"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Initialize the deployment: config, data, and state roots (XDG)
    Init,
    /// The owned machinery: daemon, config, approval desk, channels, run log (bare: status)
    #[command(
        after_help = "glossary: a box is the sandboxed container one session runs in; a gate is a \
                      proposed action awaiting your approval; a grant is an egress rule in org.toml; \
                      an exposure is a credential env var a grant injects; a listener connects a \
                      worker to a chat platform; the heartbeat is the built-in recurring task that \
                      keeps each worker checking its queue"
    )]
    Server {
        #[command(subcommand)]
        cmd: Option<ServerCmd>,
    },
    /// Connections to external services: catalog, add, inspect, remove (bare: ls)
    Connection {
        #[command(subcommand)]
        cmd: Option<ConnectionCmd>,
    },
    #[command(subcommand, hide = true)]
    Credential(CredentialCmd),
    /// The governed identities: lifecycle, trust, memory, knowledge, work (bare: ls)
    #[command(
        after_help = "glossary: standing marks whose work a task is (owner: always runs; proactive: \
                      paced by budgets); a box is the sandboxed container one session runs in; a \
                      gate is a proposed action awaiting your approval; knowledge is the worker's \
                      git-backed notes repo; memory is what it recalls about people and channels"
    )]
    Worker {
        #[command(subcommand)]
        cmd: Option<WorkerCmd>,
    },
    /// Talk with a worker right here in the terminal — a trusted chat channel
    Talk {
        /// The worker to talk to (omit: list workers, or create "elf" on first run)
        name: Option<String>,
        /// Wind a quiet session down after this many seconds (the
        /// conversation stays open; your next message wakes a fresh one)
        #[arg(long, default_value_t = 300)]
        idle: u64,
    },
    /// Generate shell completions (bash, zsh, fish, …) to stdout
    Completions {
        /// The shell to generate for
        shell: clap_complete::Shell,
    },
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
        /// Gateway listen address (default: loopback + the docker bridge;
        /// pass 0.0.0.0:7300 to listen on every interface)
        #[arg(long)]
        addr: Option<String>,
    },
    /// Daemon health: components, queue, gates, compiled config
    Status {
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Parse and check all config; print every error (config loads live)
    #[command(alias = "deploy")]
    Validate,
    /// The approval desk: what is pending your approval; inspect, approve, deny (bare: ls)
    #[command(alias = "gates")]
    Approvals {
        #[command(subcommand)]
        cmd: Option<ApprovalsCmd>,
    },
    /// Channel edges: trust designation, response mode, memory settings (bare: ls)
    Channel {
        #[command(subcommand)]
        cmd: Option<ChannelCmd>,
    },
    /// The run log: every session, whoever it was attributed to (bare: ls)
    Runs {
        #[command(subcommand)]
        cmd: Option<RunsCmd>,
    },
}

#[derive(Subcommand)]
enum ApprovalsCmd {
    /// What is pending your approval
    Ls {
        /// Machine-readable JSON
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
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// One channel's settings, readable
    Show {
        /// Channel id (see: roster server channel ls)
        channel_id: String,
    },
    /// Trust a channel: its participants may administer; replies need no gate
    Trust {
        /// Channel id (see: roster server channel ls)
        channel_id: String,
    },
    /// Untrust a channel: participants are content-only
    Untrust {
        /// Channel id (see: roster server channel ls)
        channel_id: String,
    },
    /// Tune a setting (just the id: list keys, allowed values, current values)
    Set {
        /// Channel id (see: roster server channel ls)
        channel_id: String,
        /// A settings key (omit to list them all)
        key: Option<String>,
        /// The new value (omit to list keys and values)
        value: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConnectionCmd {
    /// List services available to connect, grouped by what connecting gives you
    Catalog,
    /// Connect a service (bare: guided session — pick from the catalog or declare)
    Add {
        /// Service from the connection catalog (omit for the guided session)
        #[arg(value_name = "SERVICE")]
        service: Option<String>,
        /// Grant to this worker (repeatable); default is to ask
        #[arg(long)]
        worker: Vec<String>,
        /// Org-wide: every current and future worker (the explicit escalation)
        #[arg(long)]
        org: bool,
        /// Connection and secret name when it differs from the service
        #[arg(long)]
        name: Option<String>,
        /// Allowed hostname (repeatable; required for services outside the catalog)
        #[arg(long)]
        host: Vec<String>,
        /// Header template, for example: "Authorization: Bearer {token}"
        #[arg(long)]
        header: Option<String>,
        /// Environment variable exposed to the worker (default: <NAME>_TOKEN)
        #[arg(long)]
        env: Option<String>,
        /// Allowed HTTP method (repeatable; default: GET)
        #[arg(long)]
        method: Vec<String>,
        /// Which use(s) to set up on a multi-use provider (channel, capability)
        #[arg(long = "use", value_name = "USE")]
        uses: Vec<String>,
        /// Auth method when the provider offers several (api_key, oauth)
        #[arg(long)]
        auth: Option<String>,
        /// Interview for an unknown service; OAuth knowledge lands in providers.toml
        #[arg(long)]
        declare: bool,
        /// Test the stored credential against the live service before finishing
        #[arg(long)]
        verify: bool,
    },
    /// Every connection, its use(s) — capability, channel, model — and state
    Ls {
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete the secret from the vault; report every surviving reference
    Rm {
        /// Connection name (see: roster connection ls)
        name: String,
    },
}

/// Retired: the one noun is `connection` (docs/connections.md). Each
/// arm points at its replacement and exits nonzero.
#[derive(Subcommand)]
enum CredentialCmd {
    #[command(hide = true)]
    Add { provider: String },
    #[command(hide = true)]
    Ls {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum WorkerCmd {
    /// Scaffold a worker: spec, identity, knowledge repo
    Init {
        /// The new worker's name (lowercase letters/numbers/hyphens)
        name: String,
    },
    /// List workers and their state
    Ls {
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// One worker: spec, budgets and spend, queue, gates, memory, knowledge
    Show {
        /// The worker's name
        name: String,
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Per-action trust: grants, earned history, promotion rules
    Trust {
        /// The worker's name
        name: String,
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// One governed session, now, bypassing the queue
    Run {
        /// The worker to run
        name: String,
        /// Wall-clock ceiling in minutes
        #[arg(long, default_value_t = 30.0)]
        ceiling: f64,
        /// The prompt (bare words are joined)
        #[arg(required = true, value_name = "PROMPT")]
        prompt: Vec<String>,
    },
    /// Retire a worker: archive its spec, memory, knowledge, and history (never deletes)
    Rm {
        /// The worker to retire
        name: String,
        /// Skip the typed-name confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Scripted stdin session, one message per turn (conversations: roster talk)
    Chat {
        /// The worker to chat with
        name: String,
        /// End after this much quiet, in seconds
        #[arg(long, default_value_t = 20)]
        idle: u64,
    },
    /// Its durable work: add, relay, ls, show, requeue (bare: ls)
    Task {
        #[command(subcommand)]
        cmd: Option<TaskCmd>,
    },
    /// Inspect and repair interaction memory
    #[command(subcommand)]
    Memory(MemoryCmd),
    /// Print the knowledge repo path (then use git)
    Knowledge {
        /// The worker's name
        name: String,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// File a task for a worker
    Add {
        /// The worker to file it for
        worker: String,
        /// Wall-clock ceiling in minutes
        #[arg(long, default_value_t = 30.0)]
        ceiling: f64,
        /// File as proactive: waits out spent budget windows (owner-filed work always runs)
        #[arg(long)]
        proactive: bool,
        /// An exclusive knowledge-reorganization session (no other work alongside it)
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
    /// File an inbound message as a task (framed as untrusted content)
    Relay {
        /// The worker to relay it to
        worker: String,
        /// Sender label recorded with the task
        #[arg(long)]
        from: Option<String>,
        /// The message (bare words are joined)
        #[arg(required = true, value_name = "MESSAGE")]
        message: Vec<String>,
    },
    /// List tasks — queued, recurring, and recent outcomes
    Ls {
        /// Machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Inspect a task: state, gates, journal, prompt
    Show {
        /// Task id (any unique prefix; journaled tasks need the exact id)
        id: String,
    },
    /// Put a stuck, failed, or finished task back to waiting
    Requeue {
        /// Task id (any unique prefix; journaled tasks need the exact id)
        id: String,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// List memory notes
    Ls {
        /// The worker whose memory to list
        worker: String,
        /// Filter: worker | channel | user
        #[arg(long)]
        scope: Option<String>,
        /// Filter: the scope's id (a channel id, a user id)
        #[arg(long)]
        scope_id: Option<String>,
    },
    /// Print one note in full
    Show {
        /// The worker whose note it is
        worker: String,
        /// Note id (see: roster worker memory ls <worker>)
        id: String,
    },
    /// Replace a note's content (recorded, not a silent edit)
    Correct {
        /// The worker whose note it is
        worker: String,
        /// Note id (see: roster worker memory ls <worker>)
        id: String,
        /// The replacement content (bare words are joined)
        #[arg(required = true, value_name = "REPLACEMENT")]
        replacement: Vec<String>,
    },
    /// Remove a note
    Rm {
        /// The worker whose note it is
        worker: String,
        /// Note id (see: roster worker memory ls <worker>)
        id: String,
    },
    /// Include a note in every recall, until unpinned
    Pin {
        /// The worker whose note it is
        worker: String,
        /// Note id (see: roster worker memory ls <worker>)
        id: String,
    },
    /// Stop force-including a pinned note
    Unpin {
        /// The worker whose note it is
        worker: String,
        /// Note id (see: roster worker memory ls <worker>)
        id: String,
    },
    /// Keep a note but stop recalling it
    Disable {
        /// The worker whose note it is
        worker: String,
        /// Note id (see: roster worker memory ls <worker>)
        id: String,
    },
    /// Recall a disabled note again
    Enable {
        /// The worker whose note it is
        worker: String,
        /// Note id (see: roster worker memory ls <worker>)
        id: String,
    },
    /// Drop dead notes, keep live ones
    Compact {
        /// The worker whose memory to compact
        worker: String,
    },
}

#[derive(Subcommand)]
enum RunsCmd {
    /// List past sessions, whoever they were attributed to
    Ls {
        /// Only sessions attributed to this worker
        #[arg(long)]
        worker: Option<String>,
        /// Show at most this many
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Machine-readable JSON
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
    // Die quietly on a closed pipe (`roster … | head`) instead of panicking:
    // Rust ignores SIGPIPE by default, turning EPIPE into a println panic.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Old top-level commands point at their new home instead of half-working:
    // the argument shapes changed (positional workers, merged daemons), so a
    // silent translation could misparse. One clear error, one clear fix.
    if let Some(first) = std::env::args().nth(1) {
        if let Some(new_home) = legacy_pointer(&first) {
            eprintln!("roster: `roster {first}` has moved — use: {new_home}");
            std::process::exit(2);
        }
    }

    let cli = Cli::parse();
    // No first-run ceremony: the deployment roots and a starter org.toml
    // exist by the time any command needs them (idempotent, quiet). The
    // explicit `roster init` narrates its own work instead — pre-creating
    // here would make it report "kept" for files it just wrote.
    if !matches!(cli.command, Cmd::Init) {
        if let Err(e) = cli::init::ensure() {
            eprintln!("roster: could not initialize the deployment roots: {e}");
            std::process::exit(1);
        }
    }
    let result: Result<(), Box<dyn std::error::Error>> = match cli.command {
        Cmd::Init => cli::init::run(),
        Cmd::Server { cmd } => match cmd {
            None => cli::server::status(false).await,
            Some(ServerCmd::Start {
                cap,
                no_listen,
                once,
                addr,
            }) => cli::server::run(cap, once, no_listen, addr.as_deref()).await,
            Some(ServerCmd::Status { json }) => cli::server::status(json).await,
            Some(ServerCmd::Validate) => cli::server::validate(),
            Some(ServerCmd::Approvals { cmd }) => match cmd {
                None => cli::approvals::ls(false),
                Some(ApprovalsCmd::Ls { json }) => cli::approvals::ls(json),
                Some(ApprovalsCmd::Show { id }) => cli::approvals::show(&id),
                Some(ApprovalsCmd::Approve { id, note }) => {
                    cli::approvals::approve(&id, note.as_deref()).await
                }
                Some(ApprovalsCmd::Deny { id, note }) => {
                    cli::approvals::deny(&id, note.as_deref())
                }
            },
            Some(ServerCmd::Channel { cmd }) => match cmd {
                None => cli::channel::ls(false),
                Some(ChannelCmd::Ls { json }) => cli::channel::ls(json),
                Some(ChannelCmd::Show { channel_id }) => cli::channel::show(&channel_id),
                Some(ChannelCmd::Trust { channel_id }) => {
                    cli::channel::set_trust(&channel_id, true)
                }
                Some(ChannelCmd::Untrust { channel_id }) => {
                    cli::channel::set_trust(&channel_id, false)
                }
                Some(ChannelCmd::Set {
                    channel_id,
                    key,
                    value,
                }) => match (key, value) {
                    (Some(key), Some(value)) => cli::channel::set(&channel_id, &key, &value),
                    _ => cli::channel::set_help(&channel_id),
                },
            },
            Some(ServerCmd::Runs { cmd }) => match cmd {
                None => cli::runs::ls(None, 20, false),
                Some(RunsCmd::Ls { worker, limit, json }) => {
                    cli::runs::ls(worker.as_deref(), limit, json)
                }
                Some(RunsCmd::Show { run }) => cli::runs::show(&run),
                Some(RunsCmd::Context { run, all }) => cli::runs::context(&run, all),
                Some(RunsCmd::Recall { run }) => cli::runs::recall(&run),
            },
        },
        Cmd::Connection { cmd } => match cmd {
            None => cli::connections::ls(false),
            Some(ConnectionCmd::Catalog) => cli::connections::catalog(),
            Some(ConnectionCmd::Add {
                service,
                worker,
                org,
                name,
                host,
                header,
                env,
                method,
                uses,
                auth,
                declare,
                verify,
            }) => match service {
                Some(service) => {
                    cli::connections::connect(
                        service,
                        cli::connections::ConnectOptions {
                            workers: worker,
                            org,
                            alias: name,
                            hosts: host,
                            header,
                            env,
                            methods: method,
                            uses,
                            auth,
                            declare,
                            verify,
                        },
                    )
                    .await
                }
                None if org
                    || !worker.is_empty()
                    || name.is_some()
                    || !host.is_empty()
                    || header.is_some()
                    || env.is_some()
                    || !method.is_empty()
                    || !uses.is_empty()
                    || auth.is_some() =>
                {
                    Err("connection options require a service name".into())
                }
                None => cli::connections::guided().await,
            },
            Some(ConnectionCmd::Ls { json }) => cli::connections::ls(json),
            Some(ConnectionCmd::Rm { name }) => cli::connections::rm(&name),
        },
        Cmd::Credential(cmd) => match cmd {
            CredentialCmd::Add { provider } => Err(format!(
                "`roster credential add` has retired — run: roster connection add {provider}"
            )
            .into()),
            CredentialCmd::Ls { .. } => {
                Err("`roster credential ls` has retired — run: roster connection ls".into())
            }
        },
        Cmd::Talk { name, idle } => match name {
            Some(name) => run::session::talk(&name, idle).await,
            None => talk_bare(idle).await,
        },
        Cmd::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "roster",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Cmd::Worker { cmd } => match cmd {
            None => cli::worker::ls(false),
            Some(WorkerCmd::Init { name }) => cli::create::run(&name),
            Some(WorkerCmd::Ls { json }) => cli::worker::ls(json),
            Some(WorkerCmd::Show { name, json }) => cli::worker::show(&name, json),
            Some(WorkerCmd::Trust { name, json }) => cli::worker::trust(&name, json),
            Some(WorkerCmd::Run {
                name,
                ceiling,
                prompt,
            }) => run::boxed::run_once(&name, ceiling, prompt.join(" ")).await,
            Some(WorkerCmd::Rm { name, yes }) => cli::worker::rm(&name, yes),
            Some(WorkerCmd::Chat { name, idle }) => run::session::chat(&name, idle).await,
            Some(WorkerCmd::Task { cmd }) => match cmd {
                None => cli::task::ls(false),
                Some(TaskCmd::Add {
                    worker,
                    ceiling,
                    proactive,
                    reorganize,
                    repo,
                    base,
                    prompt,
                }) => cli::task::add(
                    &worker,
                    ceiling,
                    proactive,
                    reorganize,
                    repo,
                    &base,
                    prompt.join(" "),
                ),
                Some(TaskCmd::Relay { worker, from, message }) => {
                    channel::relay::run(&worker, from.as_deref(), message.join(" "))
                }
                Some(TaskCmd::Ls { json }) => cli::task::ls(json),
                Some(TaskCmd::Show { id }) => cli::task::show(&id),
                Some(TaskCmd::Requeue { id }) => cli::task::requeue(&id),
            },
            Some(WorkerCmd::Memory(cmd)) => match cmd {
                MemoryCmd::Ls {
                    worker,
                    scope,
                    scope_id,
                } => cli::memory::ls(&worker, scope.as_deref(), scope_id.as_deref()),
                MemoryCmd::Show { worker, id } => cli::memory::show(&worker, &id),
                MemoryCmd::Correct {
                    worker,
                    id,
                    replacement,
                } => cli::memory::correct(&worker, &id, &replacement.join(" ")),
                MemoryCmd::Rm { worker, id } => cli::memory::mutate("forget", &worker, &id),
                MemoryCmd::Pin { worker, id } => cli::memory::mutate("pin", &worker, &id),
                MemoryCmd::Unpin { worker, id } => cli::memory::mutate("unpin", &worker, &id),
                MemoryCmd::Disable { worker, id } => cli::memory::mutate("disable", &worker, &id),
                MemoryCmd::Enable { worker, id } => cli::memory::mutate("enable", &worker, &id),
                MemoryCmd::Compact { worker } => cli::memory::compact(&worker),
            },
            Some(WorkerCmd::Knowledge { name }) => cli::knowledge::run(&name),
        },
    };

    if let Err(e) = result {
        eprintln!("roster: {e}");
        std::process::exit(1);
    }
}

/// `roster talk` with no worker named: on a fresh deployment scaffold "elf"
/// and start talking — the zero-config first conversation. Otherwise show the
/// talk help and who is available.
async fn talk_bare(idle: u64) -> Result<(), Box<dyn std::error::Error>> {
    let workers = worker::names();
    if workers.is_empty() {
        eprintln!("no workers yet — creating one named \"elf\"");
        cli::create::run("elf")?;
        return run::session::talk("elf", idle).await;
    }
    use clap::CommandFactory;
    Cli::command()
        .find_subcommand("talk")
        .expect("talk subcommand exists")
        .clone()
        .bin_name("roster talk")
        .print_help()?;
    println!("\nworkers:");
    for name in workers {
        println!("  {name}");
    }
    Ok(())
}

/// Where each pre-clap command lives now. Kept until the muscle memory fades.
fn legacy_pointer(first: &str) -> Option<&'static str> {
    Some(match first {
        "imp" => "roster worker <init|ls|show|trust|run|chat|task|memory|knowledge>  (imps are now workers)",
        "serve" => "roster server start",
        "supervise" => "roster server start  (the daemons merged; --cap and --once moved there)",
        "listen" => "roster server start  (listeners start for every worker with a [channels] entry)",
        "deploy" => "roster server validate  (config now loads live — there is no deploy step)",
        "gates" => "roster server approvals <ls|show|approve|deny>",
        "channel" => "roster server channel <ls|show|trust|untrust|set>",
        "connect" => "roster connection add <provider>",
        "create" => "roster worker init <name>",
        "queue" => "roster worker task <add|relay|ls|show|requeue>",
        "relay" => "roster worker task relay <worker> \"<message>\"",
        "memory" | "notes" => "roster worker memory <ls|show|correct|rm|…> <worker>",
        "knowledge" => "roster worker knowledge <name>",
        "box" => "roster worker run <name> \"<prompt>\"",
        "session" => "roster worker chat <name>",
        "runs" => "roster server runs <ls|show|context|recall>",
        "agent" => {
            "roster worker <run|chat>, roster server runs  (sessions belong to workers; the log to the server)"
        }
        _ => return None,
    })
}
