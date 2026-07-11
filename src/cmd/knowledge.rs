//! Print the location of a worker's Git-backed world knowledge repository.
//! Everything after discovery uses the normal Git CLI.

type BErr = Box<dyn std::error::Error>;

pub fn run(args: &[String]) -> Result<(), BErr> {
    let worker = args.first().ok_or("usage: roster knowledge <worker>")?;
    if args.len() != 1 {
        return Err("usage: roster knowledge <worker>".into());
    }
    println!("{}", crate::knowledge::repo_path(worker)?.display());
    Ok(())
}
