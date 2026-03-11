use std::path::PathBuf;
use temp_dir::TempDir;

pub struct FileEnv {
    temp_dir: TempDir,
    #[allow(dyn_drop)]
    insta_settings_bind_drop_guard: Option<Box<dyn Drop>>,
}

impl FileEnv {
    pub fn new() -> Self {
        Self {
            temp_dir: TempDir::new().expect("create TempDir"),
            insta_settings_bind_drop_guard: None,
        }
    }

    pub fn setup_insta_filter(&mut self) {
        let mut settings = insta::Settings::clone_current();
        settings.add_filter(&self.temp_dir.path().to_string_lossy(), "[TEMP_DIR]");
        self.insta_settings_bind_drop_guard = Some(Box::new(settings.bind_to_scope()));
    }

    pub fn write_file(&self, path: &str, contents: &[u8]) -> PathBuf {
        let full_path = self.temp_dir.child(path);
        std::fs::write(&full_path, contents).expect("write file");
        full_path
    }
}
