//! Roster gateway (Rust) — the trusted core. Port in progress (docs/rust-port.md).
//! P0: CA + leaf minting. Proxy/judge/vault/metering land in P1–P4.

mod ca;

use ca::Ca;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ca = Ca::ensure()?;
    let (_key, chain) = ca.mint_leaf("chatgpt.com")?;
    println!(
        "CA ensured; minted a leaf for chatgpt.com ({} byte chain). CA cert:\n{}",
        chain.len(),
        ca.cert_pem()
    );
    Ok(())
}
