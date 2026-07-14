//! Print the location of an imp's Git-backed world knowledge repository.
//! Everything after discovery uses the normal Git CLI.

use crate::util::BErr;

pub fn run(imp: &str) -> Result<(), BErr> {
    crate::imp::require_imp(imp)?;
    println!("{}", crate::imp::knowledge::repo_path(imp)?.display());
    Ok(())
}
