use std::env;
use std::fs::File;
use std::io::{self, BufReader};
use std::process::ExitCode;

use keyo_flow::core::scenario::Scenario;
use keyo_flow::storage::read_json;

const USAGE: &str =
    "Использование:\n  keyo play <файл.json> [--dry-run]\n  keyo record <файл.json>";

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
    let Some(path) = path else {
        return Err(USAGE.into());
    };
    if args.len() != 0 {
        return Err(USAGE.into());
    }
    match command.as_deref().and_then(|arg| arg.to_str()) {
        Some("play") if option.as_ref().is_none_or(|arg| arg == "--dry-run") => {
            let reader = BufReader::new(File::open(&path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("Не удалось открыть {path:?}: {error}"),
                )
            })?);
            play(read_json(reader)?, option.is_some())?;
        }
        Some("record") if option.is_none() => record(path.as_ref())?,
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

#[cfg(windows)]
fn record(path: &std::path::Path) -> io::Result<()> {
    if path.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Файл {path:?} уже существует. Укажи новый путь."),
        ));
    }
    if let Some(scenario) = keyo_flow::platform::windows::record()? {
        keyo_flow::storage::write_json_new(path, &scenario).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("Не удалось сохранить {path:?}: {error}"),
            )
        })?;
        println!(
            "Сценарий сохранён в {path:?}. Событий: {}.",
            scenario.events.len()
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn record(_: &std::path::Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Команда record доступна только в Windows",
    ))
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
