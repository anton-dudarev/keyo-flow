use std::env;
use std::fs::File;
use std::io::{self, BufReader};
use std::process::ExitCode;

use keyo_flow::core::scenario::Scenario;
use keyo_flow::storage::read_json;

const USAGE: &str = "Использование: keyo play <файл.json> [--dry-run]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let command = args.next();
    if matches!(
        command.as_deref().and_then(|arg| arg.to_str()),
        Some("-h" | "--help")
    ) && args.len() == 0
    {
        println!("{USAGE}");
        return Ok(());
    }
    let path = args.next();
    let option = args.next();
    if command.as_deref().and_then(|arg| arg.to_str()) != Some("play")
        || path.is_none()
        || option.as_ref().is_some_and(|arg| arg != "--dry-run")
        || args.len() != 0
    {
        return Err(USAGE.into());
    }
    let path = path.unwrap();
    let reader = BufReader::new(File::open(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("Не удалось открыть {:?}: {error}", path),
        )
    })?);
    let scenario = read_json(reader)?;
    play(scenario, option.is_some())?;
    Ok(())
}

#[cfg(windows)]
fn play(scenario: Scenario, dry_run: bool) -> io::Result<()> {
    keyo_flow::platform::windows::play(scenario, dry_run)
}

#[cfg(not(windows))]
fn play(_: Scenario, _: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Команда play доступна только в Windows",
    ))
}
