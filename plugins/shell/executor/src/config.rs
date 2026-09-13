use std::{io, path::Path};

pub struct Config {
    pub path: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            path: "/usr/local/bin:/usr/bin:/bin".into(),
        }
    }
}
impl Config {
    pub fn from_flags(path: String) -> io::Result<Self> {
        let config = Self { path };
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.path.split(':').any(|p| !Path::new(p).is_absolute()) {
            return Err(io::Error::other("PATH entries must be absolute"));
        }
        Ok(())
    }
}
