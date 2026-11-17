use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::storage::write_replacement;

const HISTORY_LIMIT: usize = 10;

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct RecentScenarios {
    pub(super) paths: Vec<PathBuf>,
    pub(super) last_open_directory: Option<PathBuf>,
    #[serde(skip)]
    file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredRecent {
    Current(RecentScenarios),
    Legacy(Vec<PathBuf>),
}

impl RecentScenarios {
    pub(super) fn load() -> Self {
        Self::from_file(super::profile_file("recent.json"))
    }

    fn from_file(file: Option<PathBuf>) -> Self {
        let stored = file.as_deref().and_then(|path| {
            let reader = BufReader::new(File::open(path).ok()?);
            serde_json::from_reader(reader).ok()
        });
        let mut recent = match stored {
            Some(StoredRecent::Current(recent)) => recent,
            Some(StoredRecent::Legacy(paths)) => Self {
                paths,
                ..Self::default()
            },
            None => Self::default(),
        };
        let entries = std::mem::take(&mut recent.paths);
        for path in entries {
            if !recent.paths.contains(&path) {
                recent.paths.push(path);
                if recent.paths.len() == HISTORY_LIMIT {
                    break;
                }
            }
        }
        recent.file = file;
        recent
    }

    pub(super) fn initial_directory(
        remembered: Option<&Path>,
        executable: &Path,
    ) -> Option<PathBuf> {
        remembered
            .filter(|path| path.is_absolute() && path.is_dir())
            .or_else(|| executable.parent())
            .map(Path::to_owned)
    }

    pub(super) fn remember_opened(&mut self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent().filter(|parent| parent.is_absolute()) {
            self.last_open_directory = Some(parent.to_owned());
        }
        self.remember(path)
    }

    pub(super) fn remember(&mut self, path: &Path) -> io::Result<()> {
        if self.paths.first().is_none_or(|first| first != path) {
            self.paths.retain(|known| known != path);
            self.paths.insert(0, path.to_owned());
            self.paths.truncate(HISTORY_LIMIT);
        }
        let Some(file) = &self.file else {
            return Ok(());
        };
        fs::create_dir_all(file.parent().unwrap())?;
        write_replacement(file, |writer| {
            serde_json::to_writer(writer, self).map_err(io::Error::from)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_directory_survives_restart_and_falls_back_when_missing() {
        let root = std::env::temp_dir().join(format!("keyo-open-dir-{}", std::process::id()));
        let folder = root.join("scenarios");
        fs::create_dir_all(&folder).unwrap();
        let profile = root.join("recent.json");
        fs::write(&profile, br#"["scenario.json"]"#).unwrap();
        let executable = root.join("keyo-gui.exe");
        let mut recent = RecentScenarios::from_file(Some(profile.clone()));
        assert_eq!(recent.paths, [PathBuf::from("scenario.json")]);
        assert_eq!(
            RecentScenarios::initial_directory(recent.last_open_directory.as_deref(), &executable),
            Some(root.clone())
        );
        recent.remember_opened(&folder.join("opened.json")).unwrap();
        recent.remember(&root.join("saved.json")).unwrap();
        let recent = RecentScenarios::from_file(Some(profile.clone()));
        assert_eq!(
            RecentScenarios::initial_directory(recent.last_open_directory.as_deref(), &executable),
            Some(folder.clone())
        );
        fs::remove_dir(&folder).unwrap();
        assert_eq!(
            RecentScenarios::initial_directory(recent.last_open_directory.as_deref(), &executable),
            Some(root.clone())
        );
        fs::remove_file(&profile).unwrap();
        fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn remembers_ten_unique_paths_in_order_after_restarting() {
        let dir = std::env::temp_dir().join(format!("keyo-recent-{}", std::process::id()));
        let file = dir.join("recent.json");
        let mut recent = RecentScenarios::from_file(Some(file.clone()));
        for number in 0..12 {
            recent
                .remember(Path::new(&format!("scenario-{number}.json")))
                .unwrap();
        }
        recent.remember(Path::new("scenario-5.json")).unwrap();
        let restored = RecentScenarios::from_file(Some(file.clone()));
        fs::remove_file(&file).unwrap();
        fs::remove_dir(&dir).unwrap();
        assert_eq!(restored.paths, recent.paths);
        assert_eq!(restored.paths.len(), 10);
        assert_eq!(restored.paths[0], Path::new("scenario-5.json"));
        assert_eq!(restored.paths[1], Path::new("scenario-11.json"));
        assert!(!restored.paths.contains(&PathBuf::from("scenario-1.json")));
    }
}
