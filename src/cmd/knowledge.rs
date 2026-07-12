//! Print the location of a worker's Git-backed world knowledge repository.
//! Everything after discovery uses the normal Git CLI.

use super::BErr;

pub fn run(worker: &str) -> Result<(), BErr> {
    super::require_worker(worker)?;
    println!("{}", crate::knowledge::repo_path(worker)?.display());
    Ok(())
}
