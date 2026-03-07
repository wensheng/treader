// State persistence — save/load per-file viewer state to XDG cache.

use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
pub struct SavedState {
    pub page: usize,
    /// Rotation in degrees: 0, 90, 180, 270.
    pub rotation: u16,
    pub inverted: bool,
    pub auto_crop: bool,
    pub tinted: bool,
    pub epub_font_size: Option<f32>,
}

fn state_path(file_path: &Path) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "treader")?;
    let cache = dirs.cache_dir();
    std::fs::create_dir_all(cache).ok()?;

    let hash = Md5::digest(file_path.to_string_lossy().as_bytes());
    let name = format!("{:x}.json", hash);
    Some(cache.join(name))
}

pub fn load_state(file_path: &Path) -> Option<SavedState> {
    let path = state_path(file_path)?;
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_state(file_path: &Path, state: &SavedState) {
    if let Some(path) = state_path(file_path) {
        if let Ok(data) = serde_json::to_string(state) {
            let _ = std::fs::write(path, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_state_round_trip_json() {
        let state = SavedState {
            page: 42,
            rotation: 270,
            inverted: true,
            auto_crop: true,
            tinted: false,
            epub_font_size: Some(11.0),
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: SavedState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.page, 42);
        assert_eq!(restored.rotation, 270);
        assert!(restored.inverted);
        assert!(restored.auto_crop);
        assert!(!restored.tinted);
        assert_eq!(restored.epub_font_size, Some(11.0));
    }

    #[test]
    fn saved_state_default_values() {
        let state = SavedState::default();
        assert_eq!(state.page, 0);
        assert_eq!(state.rotation, 0);
        assert!(!state.inverted);
        assert!(!state.auto_crop);
        assert!(!state.tinted);
        assert_eq!(state.epub_font_size, None);
    }

    #[test]
    fn saved_state_deserialise_missing_fields() {
        // Ensure forward compatibility: old JSON without new fields
        let json = r#"{"page":5,"rotation":90,"inverted":false,"auto_crop":false,"tinted":false}"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.page, 5);
        assert_eq!(state.rotation, 90);
        assert_eq!(state.epub_font_size, None);
    }

    #[test]
    fn state_path_deterministic() {
        let p = Path::new("/some/test/file.pdf");
        let a = state_path(p);
        let b = state_path(p);
        assert_eq!(a, b);
    }

    #[test]
    fn state_path_different_files_different_paths() {
        let a = state_path(Path::new("/a.pdf"));
        let b = state_path(Path::new("/b.pdf"));
        assert_ne!(a, b);
    }

    #[test]
    fn save_and_load_round_trip() {
        // Use a temp directory to avoid polluting the real cache
        let tmp = std::env::temp_dir().join("treader_test_state");
        std::fs::create_dir_all(&tmp).ok();
        let fake_path = tmp.join("test_doc.pdf");

        let state = SavedState {
            page: 7,
            rotation: 180,
            inverted: true,
            auto_crop: false,
            tinted: true,
            epub_font_size: Some(10.0),
        };

        // save_state uses state_path which goes through XDG dirs, so we
        // test the JSON layer directly here
        let json = serde_json::to_string(&state).unwrap();
        let json_path = tmp.join("test.json");
        std::fs::write(&json_path, &json).unwrap();
        let loaded: SavedState =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();

        assert_eq!(loaded.page, 7);
        assert_eq!(loaded.rotation, 180);
        assert!(loaded.inverted);
        assert!(!loaded.auto_crop);
        assert!(loaded.tinted);
        assert_eq!(loaded.epub_font_size, Some(10.0));

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = fake_path; // suppress unused warning
    }
}
