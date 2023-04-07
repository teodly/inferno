use std::{fs::{File, create_dir_all}, io::{Read, Write}, error::Error, path::MAIN_SEPARATOR_STR};

use crate::{common::*, DeviceInfo};
use serde::{Serialize, Deserialize};
use toml;
use platform_dirs::AppDirs;

const PATH_SUFFIX: &str = ".toml";

pub struct StateStorage {
  path_prefix: String
}

impl StateStorage {
  pub fn new(self_info: &DeviceInfo) -> Self {
    let dir = AppDirs::new(Some("inferno_aoip"), false).unwrap().
      state_dir.to_str().unwrap().to_owned() +
    MAIN_SEPARATOR_STR +
    &hex::encode(self_info.factory_device_id);
    create_dir_all(&dir).log_and_forget();
    info!("using state directory: {dir}");
    Self { path_prefix: dir + MAIN_SEPARATOR_STR }
  }
  fn full_path(&self, name: &str) -> String {
    format!("{}{name}{PATH_SUFFIX}", self.path_prefix)
  }
  pub fn save(&self, name: &str, value: &impl Serialize) -> Result<(), Box<dyn Error>> {
    let mut file = File::create(self.full_path(name))?;
    let content = toml::to_string(&value)?;
    file.write(content.as_bytes())?;
    return Ok(());
  }
  pub fn load<T: for<'a> Deserialize<'a>>(&self, name: &str) -> Result<T, Box<dyn Error>> {
    let mut file = File::open(self.full_path(name))?;
    let mut content: String = "".to_owned();
    file.read_to_string(&mut content);
    return Ok(toml::from_str(&content)?);
  }
}
