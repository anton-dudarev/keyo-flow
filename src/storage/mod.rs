use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::scenario::{Scenario, TimedEvent};

const FORMAT_VERSION: u32 = 1;

#[derive(Serialize)]
struct ScenarioOutput<'a> {
    version: u32,
    events: &'a [TimedEvent],
}

#[derive(Deserialize)]
struct ScenarioInput {
    #[serde(deserialize_with = "read_version")]
    version: (),
    events: Vec<TimedEvent>,
}

fn read_version<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    let version = u32::deserialize(deserializer)?;
    if version != FORMAT_VERSION {
        return Err(serde::de::Error::custom(format!(
            "unsupported scenario version {version}"
        )));
    }
    Ok(())
}

pub fn read_json(reader: impl Read) -> serde_json::Result<Scenario> {
    let ScenarioInput {
        version: (),
        events,
    } = serde_json::from_reader(reader)?;
    Ok(Scenario { events })
}

pub fn write_json(writer: impl Write, scenario: &Scenario) -> serde_json::Result<()> {
    serde_json::to_writer_pretty(
        writer,
        &ScenarioOutput {
            version: FORMAT_VERSION,
            events: &scenario.events,
        },
    )
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
