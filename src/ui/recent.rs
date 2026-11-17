use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use crate::storage::write_replacement;

const HISTORY_LIMIT: usize = 10;

#[derive(Default)]
pub(super) struct RecentScenarios {
    pub(super) paths: Vec<PathBuf>,
    file: Option<PathBuf>,
}

impl RecentScenarios {
    pub(super) fn load() -> Self {
        let file = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|dir| dir.join("Keyo").join("recent.json"));
        Self::from_file(file)
    }

    fn from_file(file: Option<PathBuf>) -> Self {
        let mut paths = Vec::new();
        if let Some(entries) = file.as_deref().and_then(|path| {
            let reader = BufReader::new(File::open(path).ok()?);
            serde_json::from_reader::<_, Vec<PathBuf>>(reader).ok()
        }) {
            for path in entries {
                if !paths.contains(&path) {
                    paths.push(path);
                    if paths.len() == HISTORY_LIMIT {
                        break;
                    }
                }
            }
        }
        Self { paths, file }
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
            serde_json::to_writer(writer, &self.paths).map_err(io::Error::from)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
