//! `roster box [--worker <n>] [--ceiling <m>] "<prompt>"` — run one pi session.
//! Port pending (R2); the TS implementation still works during the transition.

pub async fn run(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    Err("box is not ported to Rust yet — use: node src/cli.ts box ...".into())
}
