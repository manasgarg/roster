//! Print the location of a worker's Git-backed world knowledge repository.
//! Everything after discovery uses the normal Git CLI.

use crate::util::BErr;

pub fn run(worker: &str) -> Result<(), BErr> {
    crate::worker::require_worker(worker)?;
    println!("{}", crate::worker::knowledge::repo_path(worker)?.display());
    Ok(())
}
