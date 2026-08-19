/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

#[cfg(not(test))]
use anyhow::Context;
use anyhow::Result;
use nix::errno::Errno;
use nix::unistd::{AccessFlags, access};
use serde::Deserialize;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use tokio::fs::{metadata, read_to_string};
#[cfg(not(test))]
use tokio::sync::OnceCell;
use tokio::task::spawn_blocking;
use zbus::Connection;

#[cfg(test)]
use crate::path;
use crate::systemd::SystemdUnit;

#[cfg(not(test))]
static PLATFORM_CONFIG: OnceCell<Option<PlatformConfig>> = OnceCell::const_new();

#[derive(Clone, Deserialize, Debug)]
#[serde(default)]
pub(crate) struct SessionConfig {
    pub gamescope_session_service: String,
    pub gamescope_session_desktop: String,
    pub desktop: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            gamescope_session_service: String::from("gamescope-session-plus@ogui-steam.service"),
            gamescope_session_desktop: String::from("gamescope-session-ogui-steam.desktop"),
            desktop: String::from("plasma.desktop"),
        }
    }
}

#[derive(Clone, Deserialize, Debug)]
#[serde(default)]
pub(crate) struct ScxConfig {
    pub scx_service: String,
}

impl Default for ScxConfig {
    fn default() -> Self {
        ScxConfig {
            scx_service: String::from("scx.service"),
        }
    }
}


#[derive(Clone, Default, Deserialize, Debug)]
#[serde(default)]
pub(crate) struct PlatformConfig {
    pub factory_reset: Option<ResetConfig>,
    pub update_bios: Option<ScriptConfig>,
    pub update_dock: Option<ScriptConfig>,
    pub storage: Option<StorageConfig>,
    pub fan_control: Option<ServiceConfig>,
    pub session: Option<SessionConfig>,
    pub scx: Option<ScxConfig>
}

#[derive(Clone, Default, Deserialize, Debug)]
pub(crate) struct ScriptConfig {
    pub script: PathBuf,
    #[serde(default)]
    pub script_args: Vec<String>,
}

impl ScriptConfig {
    pub(crate) async fn is_valid(&self, root: bool) -> Result<bool> {
        let meta = match metadata(&self.script).await {
            Ok(meta) => meta,
            Err(e) if [ErrorKind::NotFound, ErrorKind::PermissionDenied].contains(&e.kind()) => {
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        if !meta.is_file() {
            return Ok(false);
        }
        if root {
            let script = self.script.clone();
            if !spawn_blocking(move || match access(&script, AccessFlags::X_OK) {
                Ok(()) => Ok(true),
                Err(Errno::ENOENT | Errno::EACCES) => Ok(false),
                Err(e) => Err(e),
            })
            .await??
            {
                return Ok(false);
            }
        } else if (meta.mode() & 0o111) == 0 {
            return Ok(false);
        }
        Ok(true)
    }
}

#[derive(Clone, Default, Deserialize, Debug)]
pub(crate) struct ResetConfig {
    pub all: ScriptConfig,
    pub os: ScriptConfig,
    pub user: ScriptConfig,
}

impl ResetConfig {
    pub(crate) async fn is_valid(&self, root: bool) -> Result<bool> {
        Ok(self.all.is_valid(root).await?
            && self.os.is_valid(root).await?
            && self.user.is_valid(root).await?)
    }
}

#[derive(Clone, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServiceConfig {
    Systemd(String),
    Script {
        start: ScriptConfig,
        stop: ScriptConfig,
        status: ScriptConfig,
    },
}

impl ServiceConfig {
    pub(crate) async fn is_valid(&self, connection: &Connection, root: bool) -> Result<bool> {
        match self {
            ServiceConfig::Systemd(unit) => SystemdUnit::exists(connection, unit).await,
            ServiceConfig::Script {
                start,
                stop,
                status,
            } => Ok(start.is_valid(root).await?
                && stop.is_valid(root).await?
                && status.is_valid(root).await?),
        }
    }
}

#[derive(Clone, Default, Deserialize, Debug)]
pub(crate) struct StorageConfig {
    pub trim_devices: ScriptConfig,
    pub format_device: FormatDeviceConfig,
}

impl StorageConfig {
    pub(crate) async fn is_valid(&self, root: bool) -> Result<bool> {
        Ok(self.trim_devices.is_valid(root).await? && self.format_device.is_valid(root).await?)
    }
}

#[derive(Clone, Default, Deserialize, Debug)]
pub(crate) struct FormatDeviceConfig {
    pub script: PathBuf,
    #[serde(default)]
    pub script_args: Vec<String>,
    pub label_flag: String,
    #[serde(default)]
    pub device_flag: Option<String>,
    #[serde(default)]
    pub validate_flag: Option<String>,
    #[serde(default)]
    pub no_validate_flag: Option<String>,
}

impl FormatDeviceConfig {
    pub(crate) async fn is_valid(&self, root: bool) -> Result<bool> {
        let meta = match metadata(&self.script).await {
            Ok(meta) => meta,
            Err(e) if [ErrorKind::NotFound, ErrorKind::PermissionDenied].contains(&e.kind()) => {
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        if !meta.is_file() {
            return Ok(false);
        }
        if root {
            let script = self.script.clone();
            if !spawn_blocking(move || match access(&script, AccessFlags::X_OK) {
                Ok(()) => Ok(true),
                Err(Errno::ENOENT | Errno::EACCES) => Ok(false),
                Err(e) => Err(e),
            })
            .await??
            {
                return Ok(false);
            }
        } else if (meta.mode() & 0o111) == 0 {
            return Ok(false);
        }
        Ok(true)
    }
}

impl PlatformConfig {
    #[cfg(not(test))]
    async fn load() -> Result<Option<PlatformConfig>> {
        let path = "/usr/share/steamos-manager/platform.toml";
        let config = read_to_string(path)
            .await
            .with_context(|| format!("Failed to read {path}"))?;
        Ok(Some(toml::from_str(config.as_ref())?))
    }

    #[cfg(test)]
    pub(crate) fn set_test_paths(&mut self) {
        if let Some(ref mut factory_reset) = self.factory_reset {
            if factory_reset.all.script.as_os_str().is_empty() {
                factory_reset.all.script = path("exe");
            }
            if factory_reset.os.script.as_os_str().is_empty() {
                factory_reset.os.script = path("exe");
            }
            if factory_reset.user.script.as_os_str().is_empty() {
                factory_reset.user.script = path("exe");
            }
        }
        if let Some(ref mut storage) = self.storage {
            if storage.trim_devices.script.as_os_str().is_empty() {
                storage.trim_devices.script = path("exe");
            }
            if storage.format_device.script.as_os_str().is_empty() {
                storage.format_device.script = path("exe");
            }
        }
        if let Some(ref mut update_bios) = self.update_bios
            && update_bios.script.as_os_str().is_empty()
        {
            update_bios.script = path("exe");
        }
        if let Some(ref mut update_dock) = self.update_dock
            && update_dock.script.as_os_str().is_empty()
        {
            update_dock.script = path("exe");
        }
    }
}

#[cfg(not(test))]
pub(crate) async fn platform_config() -> Result<&'static Option<PlatformConfig>> {
    PLATFORM_CONFIG.get_or_try_init(PlatformConfig::load).await
}

#[cfg(test)]
pub(crate) async fn platform_config() -> Result<Option<PlatformConfig>> {
    let test = crate::testing::current();
    let config = (*test.platform_config.lock().await).clone();
    Ok(config)
}

#[cfg(not(test))]
pub(crate) async fn session_config() -> SessionConfig {
    match platform_config().await {
        Ok(config) => config
            .as_ref()
            .map(|c| c.session.clone().unwrap_or_default())
            .unwrap_or_default(),
        Err(_) => SessionConfig::default(),
    }
}

#[cfg(test)]
pub(crate) async fn session_config() -> SessionConfig {
    platform_config()
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
        .session
        .unwrap_or_default()
}

#[cfg(not(test))]
pub(crate) async fn scx_config() -> ScxConfig {
    match platform_config().await {
        Ok(config) => config
            .as_ref()
            .map(|c| c.scx.clone().unwrap_or_default())
            .unwrap_or_default(),
        Err(_) => ScxConfig::default(),
    }
}

#[cfg(test)]
pub(crate) async fn scx_config() -> ScxConfig {
    platform_config()
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
        .scx
        .unwrap_or_default()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{path, testing};
    use std::os::unix::fs::PermissionsExt;
    use tokio::fs::{set_permissions, write};

    #[tokio::test]
    async fn script_config_valid_no_path() {
        assert!(!ScriptConfig::default().is_valid(false).await.unwrap());
    }

    #[tokio::test]
    async fn script_config_valid_root_no_path() {
        assert!(!ScriptConfig::default().is_valid(true).await.unwrap());
    }

    #[tokio::test]
    async fn script_config_valid_directory() {
        assert!(
            !ScriptConfig {
                script: PathBuf::from("/"),
                script_args: Vec::new(),
            }
            .is_valid(false)
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn script_config_valid_root_directory() {
        assert!(
            !ScriptConfig {
                script: PathBuf::from("/"),
                script_args: Vec::new(),
            }
            .is_valid(true)
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn script_config_valid_noexec() {
        let _handle = testing::start();
        let exe_path = path("exe");
        write(&exe_path, "").await.unwrap();
        set_permissions(&exe_path, PermissionsExt::from_mode(0o600))
            .await
            .unwrap();

        assert!(
            !ScriptConfig {
                script: exe_path,
                script_args: Vec::new(),
            }
            .is_valid(false)
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn script_config_root_valid_noexec() {
        let _handle = testing::start();
        let exe_path = path("exe");
        write(&exe_path, "").await.unwrap();
        set_permissions(&exe_path, PermissionsExt::from_mode(0o600))
            .await
            .unwrap();

        assert!(
            !ScriptConfig {
                script: exe_path,
                script_args: Vec::new(),
            }
            .is_valid(true)
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn script_config_valid() {
        let _handle = testing::start();
        let exe_path = path("exe");
        write(&exe_path, "").await.unwrap();
        set_permissions(&exe_path, PermissionsExt::from_mode(0o700))
            .await
            .unwrap();

        assert!(
            ScriptConfig {
                script: exe_path,
                script_args: Vec::new(),
            }
            .is_valid(false)
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn script_config_root_valid() {
        let _handle = testing::start();
        let exe_path = path("exe");
        write(&exe_path, "").await.unwrap();
        set_permissions(&exe_path, PermissionsExt::from_mode(0o700))
            .await
            .unwrap();

        assert!(
            ScriptConfig {
                script: exe_path,
                script_args: Vec::new(),
            }
            .is_valid(true)
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn jupiter_valid() {
        let config = read_to_string("../data/devices/steam-deck.toml")
            .await
            .expect("read_to_string");
        let res = toml::from_str::<PlatformConfig>(config.as_ref());
        assert!(res.is_ok(), "{res:?}");
    }
}
