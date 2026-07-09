//! `roster connect <provider>` — create a credential via its login flow.
//! Port pending (R3); the TS implementation still works during the transition.

pub async fn run(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    Err("connect is not ported to Rust yet — use: node src/cli.ts connect <provider>".into())
}
