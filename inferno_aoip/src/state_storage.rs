use std::{
  error::Error,
  fs::{create_dir_all, File},
  io::{Read, Write},
  path::MAIN_SEPARATOR_STR,
  sync::Arc,
};

use crate::{common::*, device_info, device_info::DeviceInfo};
use platform_dirs::AppDirs;
use serde::{Deserialize, Serialize};
use toml;

const PATH_SUFFIX: &str = ".toml";

pub struct StateStorage {
  path_prefix: String,
}

impl StateStorage {
  pub fn new(self_info: &DeviceInfo) -> Self {
    // STATE_DIR pins the location. The default is keyed by the device id,
    // which is derived from the IP address, so an address change silently
    // started from empty state (channel names and subscriptions lost).
    let dir = match &self_info.state_dir {
      Some(dir) => dir.to_string_lossy().into_owned(),
      None => {
        AppDirs::new(Some("inferno_aoip"), false)
          .map(|d| d.state_dir.to_string_lossy().into_owned())
          .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned())
          + MAIN_SEPARATOR_STR
          + &hex::encode(self_info.factory_device_id)
      }
    };
    create_dir_all(&dir).log_and_forget();
    info!("using state directory: {dir}");
    Self { path_prefix: dir + MAIN_SEPARATOR_STR }
  }
  fn full_path(&self, name: &str) -> String {
    format!("{}{name}{PATH_SUFFIX}", self.path_prefix)
  }
  pub fn save(&self, name: &str, value: &impl Serialize) -> Result<(), Box<dyn Error>> {
    let content = toml::to_string(&value)?;
    let tmp_path = self.full_path(&format!("tmp.{name}"));
    let mut file = File::create(&tmp_path)?;
    // write_all: a bare write() may store only part of the buffer and
    // report success. sync_all before the rename, or after a power cut the
    // rename can be on disk while the content is not (an empty file).
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    let final_path = self.full_path(name);
    std::fs::rename(tmp_path, &final_path)?;
    // and the directory, so the rename itself survives; best effort
    if let Some(dir) = std::path::Path::new(&final_path).parent() {
      if let Ok(d) = File::open(if dir.as_os_str().is_empty() { std::path::Path::new(".") } else { dir }) {
        let _ = d.sync_all();
      }
    }
    Ok(())
  }
  pub fn load<T: for<'a> Deserialize<'a>>(&self, name: &str) -> Result<T, Box<dyn Error>> {
    let mut file = File::open(self.full_path(name))?;
    let mut content: String = "".to_owned();
    file.read_to_string(&mut content)?;
    Ok(toml::from_str(&content)?)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_state_storage() -> StateStorage {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("inferno_aoip_test_{ts}")).to_str().unwrap().to_owned();
    std::fs::create_dir_all(&dir).unwrap();
    StateStorage { path_prefix: dir + MAIN_SEPARATOR_STR }
  }

  #[derive(Serialize, Deserialize, Debug, PartialEq)]
  struct TestConfig {
    name: String,
    count: u32,
  }

  #[test]
  fn save_and_load_roundtrip() {
    let storage = temp_state_storage();
    let config = TestConfig { name: "test".to_string(), count: 42 };
    storage.save("config", &config).unwrap();
    let loaded: TestConfig = storage.load("config").unwrap();
    assert_eq!(config, loaded);
  }

  #[test]
  fn load_missing_file_fails() {
    let storage = temp_state_storage();
    let result: Result<TestConfig, Box<dyn Error>> = storage.load("nonexistent");
    assert!(result.is_err());
  }

  #[test]
  fn save_overwrites_existing() {
    let storage = temp_state_storage();
    let config1 = TestConfig { name: "first".to_string(), count: 1 };
    let config2 = TestConfig { name: "second".to_string(), count: 2 };
    storage.save("config", &config1).unwrap();
    storage.save("config", &config2).unwrap();
    let loaded: TestConfig = storage.load("config").unwrap();
    assert_eq!(loaded.name, "second");
    assert_eq!(loaded.count, 2);
  }

  #[test]
  fn save_uses_atomic_rename() {
    let storage = temp_state_storage();
    let config = TestConfig { name: "atomic".to_string(), count: 99 };
    storage.save("atomic", &config).unwrap();
    // tmp file should not exist after rename
    let tmp_path = format!("{}tmp.atomic{PATH_SUFFIX}", storage.path_prefix);
    assert!(!std::path::Path::new(&tmp_path).exists());
    // final file should exist
    let final_path = format!("{}atomic{PATH_SUFFIX}", storage.path_prefix);
    assert!(std::path::Path::new(&final_path).exists());
  }

  #[test]
  fn load_malformed_toml_fails() {
    let storage = temp_state_storage();
    let path = format!("{}bad{PATH_SUFFIX}", storage.path_prefix);
    std::fs::write(&path, "not valid toml [[[").unwrap();
    let result: Result<TestConfig, Box<dyn Error>> = storage.load("bad");
    assert!(result.is_err());
  }
}


#[cfg(test)]
mod robustness_tests {
  use super::*;
  use serde::{Deserialize, Serialize};
  use std::time::{SystemTime, UNIX_EPOCH};

  #[derive(Serialize, Deserialize, Debug, PartialEq)]
  struct TestConfig { name: String, count: u32 }

  fn temp() -> StateStorage {
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("inferno_aoip_rb_{ts}")).to_str().unwrap().to_owned();
    std::fs::create_dir_all(&dir).unwrap();
    StateStorage { path_prefix: dir + std::path::MAIN_SEPARATOR_STR }
  }

  #[test]
  fn load_empty_file_fails() {
    let storage = temp();
    let path = format!("{}empty{PATH_SUFFIX}", storage.path_prefix);
    std::fs::write(&path, "").unwrap();
    let result: Result<TestConfig, Box<dyn Error>> = storage.load("empty");
    assert!(result.is_err());
  }

  #[test]
  fn load_whitespace_only_fails() {
    let storage = temp();
    let path = format!("{}ws{PATH_SUFFIX}", storage.path_prefix);
    std::fs::write(&path, "   
").unwrap();
    let result: Result<TestConfig, Box<dyn Error>> = storage.load("ws");
    assert!(result.is_err());
  }
}
