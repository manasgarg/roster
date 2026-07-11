//! Owner inspection for a worker's Git-backed world knowledge.

type BErr = Box<dyn std::error::Error>;

pub fn run(args: &[String]) -> Result<(), BErr> {
    match args.first().map(String::as_str).unwrap_or("status") {
        "status" => {
            let worker = args
                .get(1)
                .ok_or("usage: roster knowledge status <worker>")?;
            if args.len() != 2 {
                return Err("usage: roster knowledge status <worker>".into());
            }
            println!("{}", crate::knowledge::status(worker)?);
            Ok(())
        }
        "log" => log(&args[1..]),
        "reset" => reset(&args[1..]),
        "show" => {
            let worker = args
                .get(1)
                .ok_or("usage: roster knowledge show <worker> <commit>")?;
            let commit = args
                .get(2)
                .ok_or("usage: roster knowledge show <worker> <commit>")?;
            if args.len() != 3 {
                return Err("usage: roster knowledge show <worker> <commit>".into());
            }
            println!("{}", crate::knowledge::show(worker, commit)?);
            Ok(())
        }
        other => Err(format!(
            "unknown knowledge subcommand \"{other}\" (try: status, log, show, reset)"
        )
        .into()),
    }
}

fn reset(args: &[String]) -> Result<(), BErr> {
    let worker = args
        .first()
        .ok_or("usage: roster knowledge reset <worker> [--to <commit>] --yes")?;
    let mut revision: Option<&str> = None;
    let mut confirmed = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--to" => {
                revision = Some(
                    args.get(index + 1)
                        .map(String::as_str)
                        .ok_or("--to wants a commit")?,
                );
                index += 2;
            }
            "--yes" => {
                confirmed = true;
                index += 1;
            }
            flag => return Err(format!("unknown knowledge reset flag \"{flag}\"").into()),
        }
    }
    if !confirmed {
        return Err(
            "reset changes the current knowledge tree; repeat with --yes after reviewing knowledge log"
                .into(),
        );
    }
    let result = crate::knowledge::reset(worker, revision)?;
    println!("{result}");
    Ok(())
}

fn log(args: &[String]) -> Result<(), BErr> {
    let worker = args
        .first()
        .ok_or("usage: roster knowledge log <worker> [--limit <n>]")?;
    let mut limit = 20usize;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                limit = args
                    .get(index + 1)
                    .and_then(|value| value.parse().ok())
                    .filter(|value| *value > 0)
                    .ok_or("--limit wants a positive integer")?;
                index += 2;
            }
            flag => return Err(format!("unknown knowledge log flag \"{flag}\"").into()),
        }
    }
    println!("{}", crate::knowledge::log(worker, limit)?);
    Ok(())
}
