use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

use directories::ProjectDirs;

use crate::auth::Credentials;

#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
}

impl Store {
    #[cfg(test)]
    pub fn for_test(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn new() -> io::Result<Self> {
        let directory = if let Some(path) = std::env::var_os("XYZ_TUI_STATE_DIR") {
            if path.is_empty() {
                return Err(io::Error::other("XYZ_TUI_STATE_DIR 不能为空"));
            }
            PathBuf::from(path)
        } else {
            ProjectDirs::from("", "", "xyz-tui")
                .ok_or_else(|| io::Error::other("无法定位用户目录"))?
                .data_local_dir()
                .to_owned()
        };
        Ok(Self { directory })
    }

    pub fn load(&self) -> io::Result<Option<Credentials>> {
        let bytes = match fs::read(self.directory.join("session.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let credentials: Credentials = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "登录信息无法读取"))?;
        if !credentials.is_complete() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "登录信息不完整"));
        }
        Ok(Some(credentials))
    }

    pub fn device_id(&self) -> io::Result<String> {
        let path = self.directory.join("device-id");
        match fs::read_to_string(&path) {
            Ok(id) if uuid::Uuid::parse_str(id.trim()).is_ok() => return Ok(id.trim().into()),
            Ok(_) => return Err(io::Error::other("设备标识无法读取")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        self.create_directory()?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.write_all(id.as_bytes())?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(&path) {
            Ok(_) => Ok(id),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => self.device_id(),
            Err(error) => Err(error.error),
        }
    }

    pub fn save(&self, credentials: &Credentials) -> io::Result<()> {
        if !credentials.is_complete() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "登录信息不完整",
            ));
        }
        self.create_directory()?;
        // tempfile creates the file with mode 0600 on Unix, before any tokens are written.
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(&mut file, credentials).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("session.json"))
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn create_directory(&self) -> io::Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.directory)
    }

    pub fn clear(&self) -> io::Result<()> {
        match fs::remove_file(self.directory.join("session.json")) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}
