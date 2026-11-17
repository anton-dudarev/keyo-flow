use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::core::scenario::{CoordinateSpace, Scenario, TimedEvent};

#[derive(Serialize)]
struct ScenarioOutput<'a> {
    version: u32,
    events: &'a [TimedEvent],
}

#[derive(Deserialize)]
struct ScenarioInput {
    #[serde(deserialize_with = "read_version")]
    version: CoordinateSpace,
    events: Vec<TimedEvent>,
}

fn read_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<CoordinateSpace, D::Error> {
    let version = u32::deserialize(deserializer)?;
    match version {
        2 => Ok(CoordinateSpace::Desktop),
        3 => Ok(CoordinateSpace::Window),
        _ => Err(serde::de::Error::custom(format!(
            "unsupported scenario version {version}"
        ))),
    }
}

pub fn read_json(reader: impl Read) -> serde_json::Result<Scenario> {
    let ScenarioInput {
        version: coordinates,
        events,
    } = serde_json::from_reader(reader)?;
    let scenario = Scenario {
        coordinates,
        events,
    };
    scenario
        .validate_motion()
        .map_err(serde::de::Error::custom)?;
    validate_coordinates(&scenario).map_err(serde::de::Error::custom)?;
    Ok(scenario)
}

pub fn write_json(writer: impl Write, scenario: &Scenario) -> serde_json::Result<()> {
    scenario
        .validate_motion()
        .map_err(serde::ser::Error::custom)?;
    validate_coordinates(scenario).map_err(serde::ser::Error::custom)?;
    serde_json::to_writer_pretty(
        writer,
        &ScenarioOutput {
            version: match scenario.coordinates {
                CoordinateSpace::Desktop => 2,
                CoordinateSpace::Window => 3,
            },
            events: &scenario.events,
        },
    )
}

fn validate_coordinates(scenario: &Scenario) -> Result<(), &'static str> {
    for timed in &scenario.events {
        if let Some((x, y)) = timed.event.pointer_position() {
            scenario.coordinates.validate_point(x, y)?;
        }
    }
    Ok(())
}

/// Сохраняет сценарий в новый файл и сообщает об ошибке, если путь уже занят.
pub fn write_json_new(path: &Path, scenario: &Scenario) -> io::Result<()> {
    let mut writer = BufWriter::new(File::create_new(path)?);
    let result = write_json(&mut writer, scenario)
        .map_err(io::Error::from)
        .and_then(|()| writer.flush());
    drop(writer);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

/// Заменяет файл только после успешной записи и сброса буфера нового JSON.
pub fn write_json_replace(path: &Path, scenario: &Scenario) -> io::Result<()> {
    write_replacement(path, |writer| {
        write_json(writer, scenario).map_err(io::Error::from)
    })
}

pub(crate) fn write_replacement(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    static TEMP_NUMBER: AtomicU64 = AtomicU64::new(0);
    let (temporary, file) = loop {
        let number = TEMP_NUMBER.fetch_add(1, Ordering::Relaxed);
        let candidate = path.with_file_name(format!(".keyo-{}-{number}.tmp", std::process::id()));
        match File::create_new(&candidate) {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    };
    let mut writer = BufWriter::new(file);
    let written = write(&mut writer)
        .and_then(|()| writer.flush())
        .and_then(|()| writer.get_ref().sync_all());
    drop(writer);
    let result = written.and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_keeps_original_when_writing_fails() {
        let dir = std::env::temp_dir().join(format!("keyo-replace-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("scenario.json");
        let original = b"{\"version\":2,\"events\":[]}";
        fs::write(&path, original).unwrap();
        let result = write_replacement(&path, |writer| {
            writer.write_all(b"incomplete")?;
            Err(io::Error::other("Ошибка тестовой записи"))
        });
        let contents = fs::read(&path).unwrap();
        let count = fs::read_dir(&dir).unwrap().count();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
        assert!(result.is_err());
        assert_eq!(contents, original);
        assert_eq!(count, 1);
    }
}
