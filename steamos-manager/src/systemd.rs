/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, anyhow, bail};
use std::path::PathBuf;
use std::str::FromStr;
use strum::{Display, EnumString};
use tokio_stream::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{self, Connection};

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Unit",
    default_service = "org.freedesktop.systemd1"
)]
pub(crate) trait SystemdUnit {
    #[zbus(property)]
    fn active_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn unit_file_state(&self) -> zbus::Result<String>;

    async fn restart(&self, mode: &str) -> zbus::Result<OwnedObjectPath>;
    async fn start(&self, mode: &str) -> zbus::Result<OwnedObjectPath>;
    async fn stop(&self, mode: &str) -> zbus::Result<OwnedObjectPath>;
    async fn reload(&self, mode: &str) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    #[allow(clippy::type_complexity)]
    async fn enable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> zbus::Result<(bool, Vec<(String, String, String)>)>;

    async fn disable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
    ) -> zbus::Result<Vec<(String, String, String)>>;

    async fn mask_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> zbus::Result<Vec<(String, String, String)>>;

    async fn unmask_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
    ) -> zbus::Result<Vec<(String, String, String)>>;

    async fn reload(&self) -> zbus::Result<()>;

    async fn get_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;

    #[zbus(signal)]
    async fn job_removed(
        &self,
        id: u32,
        job: ObjectPath<'_>,
        unit: &str,
        result: &str,
    ) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub(crate) trait LogindManager {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "lowercase")]
pub enum ActiveState {
    Active,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Reloading,
    Refreshing,
}

impl ActiveState {
    pub fn is_active(self) -> bool {
        match self {
            ActiveState::Active
            | ActiveState::Activating
            | ActiveState::Reloading
            | ActiveState::Refreshing => true,
            ActiveState::Inactive | ActiveState::Deactivating | ActiveState::Failed => false,
        }
    }
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "lowercase")]
pub enum EnableState {
    Disabled,
    Enabled,
    Masked,
    Static,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "kebab-case")]
pub enum JobMode {
    Replace,
    Fail,
    Isolate,
    IgnoreDependencies,
    IgnoreRequirements,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "kebab-case")]
pub enum JobResult {
    Done,
    Canceled,
    Timeout,
    Failed,
    Dependency,
    Skipped,
}

pub struct SystemdUnit<'dbus> {
    pub(crate) proxy: SystemdUnitProxy<'dbus>,
    manager: SystemdManagerProxy<'dbus>,
    name: String,
}

pub struct SystemdJobWaiter {
    receiver: JobRemovedStream,
    object: OwnedObjectPath,
}

pub async fn daemon_reload(connection: &Connection) -> Result<()> {
    let proxy = SystemdManagerProxy::new(connection).await?;
    proxy.reload().await?;
    Ok(())
}

impl<'dbus> SystemdUnit<'dbus> {
    pub async fn exists(connection: &Connection, name: &str) -> Result<bool> {
        let manager = SystemdManagerProxy::new(connection).await?;
        // This is kinda hacky, but zbus makes it pretty hard to get the proper error name
        let expected_error = format!("Unit {name} not loaded.");
        match manager.get_unit(name).await {
            Ok(_) => Ok(true),
            Err(zbus::Error::MethodError(name, _, _))
                if name == "org.freedesktop.systemd1.NoSuchUnit" =>
            {
                Ok(false)
            }
            Err(zbus::Error::Failure(message)) if message == expected_error => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn new(connection: &Connection, name: &str) -> Result<SystemdUnit<'dbus>> {
        let path = PathBuf::from("/org/freedesktop/systemd1/unit").join(escape(name));
        let path = String::from(path.to_str().ok_or(anyhow!("Unit name {name} invalid"))?);
        let manager = SystemdManagerProxy::new(connection).await?;
        Ok(SystemdUnit {
            proxy: SystemdUnitProxy::builder(connection)
                .path(path)?
                .build()
                .await?,
            manager,
            name: String::from(name),
        })
    }

    pub async fn restart(&self, job_mode: JobMode) -> Result<SystemdJobWaiter> {
        let receiver = self.manager.receive_job_removed().await?;
        let object = self.proxy.restart(job_mode.to_string().as_str()).await?;
        Ok(SystemdJobWaiter { receiver, object })
    }

    pub async fn start(&self, job_mode: JobMode) -> Result<SystemdJobWaiter> {
        let receiver = self.manager.receive_job_removed().await?;
        let object = self.proxy.start(job_mode.to_string().as_str()).await?;
        Ok(SystemdJobWaiter { receiver, object })
    }

    pub async fn stop(&self, job_mode: JobMode) -> Result<SystemdJobWaiter> {
        let receiver = self.manager.receive_job_removed().await?;
        let object = self.proxy.stop(job_mode.to_string().as_str()).await?;
        Ok(SystemdJobWaiter { receiver, object })
    }

    pub async fn reload(&self, job_mode: JobMode) -> Result<SystemdJobWaiter> {
        let receiver = self.manager.receive_job_removed().await?;
        let object = self.proxy.reload(job_mode.to_string().as_str()).await?;
        Ok(SystemdJobWaiter { receiver, object })
    }

    #[allow(unused)]
    pub async fn enable(&self) -> Result<bool> {
        let (_, res) = self
            .manager
            .enable_unit_files(&[self.name.as_str()], false, false)
            .await?;
        Ok(!res.is_empty())
    }

    #[allow(unused)]
    pub async fn disable(&self) -> Result<bool> {
        let res = self
            .manager
            .disable_unit_files(&[self.name.as_str()], false)
            .await?;
        Ok(!res.is_empty())
    }

    pub async fn mask(&self) -> Result<bool> {
        let res = self
            .manager
            .mask_unit_files(&[self.name.as_str()], false, false)
            .await?;
        Ok(!res.is_empty())
    }

    pub async fn unmask(&self) -> Result<bool> {
        let res = self
            .manager
            .unmask_unit_files(&[self.name.as_str()], false)
            .await?;
        Ok(!res.is_empty())
    }

    pub async fn active(&self) -> Result<ActiveState> {
        Ok(ActiveState::from_str(
            self.proxy.active_state().await?.as_str(),
        )?)
    }

    pub async fn enabled(&self) -> Result<EnableState> {
        Ok(EnableState::from_str(
            self.proxy.unit_file_state().await?.as_str(),
        )?)
    }
}

impl SystemdJobWaiter {
    pub async fn wait(mut self) -> Result<JobResult> {
        while let Some(job) = self.receiver.next().await {
            let args = job.args()?;
            if args.job != self.object.as_ref() {
                continue;
            }
            return Ok(JobResult::try_from(args.result)?);
        }
        bail!("Job removed pipe exited prematurely");
    }
}

pub fn escape(name: &str) -> String {
    let mut parts = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            parts.push(c);
        } else {
            let escaped = format!("_{:02x}", u32::from(c));
            parts.push_str(escaped.as_str());
        }
    }
    parts
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::error::to_zbus_fdo_error;
    use crate::{enum_roundtrip, testing};
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::time::sleep;
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::ObjectPath;
    use zbus::{ObjectServer, fdo};

    #[test]
    fn enable_state_roundtrip() {
        enum_roundtrip!(EnableState {
            "disabled": str = Disabled,
            "enabled": str = Enabled,
            "masked": str = Masked,
            "static": str = Static,
        });
        assert!(EnableState::from_str("loaded").is_err());
    }

    #[test]
    fn job_mode_roundtrip() {
        enum_roundtrip!(JobMode {
            "replace": str = Replace,
            "fail": str = Fail,
            "isolate": str = Isolate,
            "ignore-dependencies": str = IgnoreDependencies,
            "ignore-requirements": str = IgnoreRequirements,
        });
        assert!(JobMode::from_str("failed").is_err());
    }

    #[test]
    fn test_escape() {
        assert_eq!(escape("systemd"), "systemd");
        assert_eq!(escape("system d"), "system_20d");
    }

    #[derive(Default)]
    pub struct MockUnit {
        pub active: String,
        pub unit_file: String,
        job: u32,
    }

    #[derive(Default)]
    pub struct MockManager {
        states: HashMap<String, EnableState>,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl MockUnit {
        #[zbus(property)]
        fn active_state(&self) -> fdo::Result<String> {
            Ok(self.active.clone())
        }

        #[zbus(property)]
        fn unit_file_state(&self) -> fdo::Result<String> {
            Ok(self.unit_file.clone())
        }

        async fn restart(
            &mut self,
            mode: &str,
            #[zbus(object_server)] object_server: &ObjectServer,
            #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
        ) -> fdo::Result<OwnedObjectPath> {
            if JobMode::from_str(mode).is_err() {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            let path = ObjectPath::try_from(format!("/restart/{mode}/{}", self.job))
                .map_err(to_zbus_fdo_error)?;
            let manager = object_server
                .interface::<_, MockManager>("/org/freedesktop/systemd1")
                .await?;
            manager
                .job_removed(self.job, path.clone(), "", "done")
                .await?;
            self.job += 1;
            self.active = String::from("active");
            self.active_state_changed(&ctx).await?;
            Ok(path.into())
        }

        async fn start(
            &mut self,
            mode: &str,
            #[zbus(object_server)] object_server: &ObjectServer,
            #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
        ) -> fdo::Result<OwnedObjectPath> {
            if JobMode::from_str(mode).is_err() {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            let path = ObjectPath::try_from(format!("/start/{mode}/{}", self.job))
                .map_err(to_zbus_fdo_error)?;
            let manager = object_server
                .interface::<_, MockManager>("/org/freedesktop/systemd1")
                .await?;
            manager
                .job_removed(self.job, path.clone(), "", "done")
                .await?;
            self.job += 1;
            self.active = String::from("active");
            self.active_state_changed(&ctx).await?;
            Ok(path.into())
        }

        async fn stop(
            &mut self,
            mode: &str,
            #[zbus(object_server)] object_server: &ObjectServer,
            #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
        ) -> fdo::Result<OwnedObjectPath> {
            if JobMode::from_str(mode).is_err() {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            let path = ObjectPath::try_from(format!("/stop/{mode}/{}", self.job))
                .map_err(to_zbus_fdo_error)?;
            let manager = object_server
                .interface::<_, MockManager>("/org/freedesktop/systemd1")
                .await?;
            manager
                .job_removed(self.job, path.clone(), "", "done")
                .await?;
            self.job += 1;
            self.active = String::from("inactive");
            self.active_state_changed(&ctx).await?;
            Ok(path.into())
        }

        async fn reload(
            &mut self,
            mode: &str,
            #[zbus(object_server)] object_server: &ObjectServer,
        ) -> fdo::Result<OwnedObjectPath> {
            let path = ObjectPath::try_from(format!("/reload/{mode}/{}", self.job))
                .map_err(to_zbus_fdo_error)?;
            let manager = object_server
                .interface::<_, MockManager>("/org/freedesktop/systemd1")
                .await?;
            manager
                .job_removed(self.job, path.clone(), "", "done")
                .await?;
            self.job += 1;
            Ok(path.into())
        }
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl MockManager {
        #[allow(clippy::type_complexity)]
        async fn enable_unit_files(
            &mut self,
            files: Vec<String>,
            _runtime: bool,
            _force: bool,
            #[zbus(object_server)] object_server: &ObjectServer,
        ) -> fdo::Result<(bool, Vec<(String, String, String)>)> {
            let mut res = Vec::new();
            for file in files {
                if let Some(state) = self.states.get(&file) {
                    if *state == EnableState::Disabled {
                        self.states.insert(file.to_string(), EnableState::Enabled);
                        res.push((String::default(), String::default(), file.to_string()));
                    }
                } else {
                    self.states.insert(file.to_string(), EnableState::Enabled);
                    res.push((String::default(), String::default(), file.to_string()));
                }
                let path = PathBuf::from("/org/freedesktop/systemd1/unit").join(escape(&file));
                let mock_unit = object_server
                    .interface::<_, MockUnit>(path.to_string_lossy())
                    .await;
                if let Ok(mock_unit) = mock_unit {
                    let mut unit = mock_unit.get_mut().await;
                    unit.unit_file = String::from("enabled");
                    unit.unit_file_state_changed(mock_unit.signal_emitter())
                        .await?;
                }
            }
            Ok((true, res))
        }

        async fn disable_unit_files(
            &mut self,
            files: Vec<String>,
            _runtime: bool,
            #[zbus(object_server)] object_server: &ObjectServer,
        ) -> fdo::Result<Vec<(String, String, String)>> {
            let mut res = Vec::new();
            for file in files {
                if let Some(state) = self.states.get(&file) {
                    if *state == EnableState::Enabled {
                        self.states.insert(file.to_string(), EnableState::Disabled);
                        res.push((String::default(), String::default(), file.to_string()));
                    }
                } else {
                    self.states.insert(file.to_string(), EnableState::Disabled);
                    res.push((String::default(), String::default(), file.to_string()));
                }
                let path = PathBuf::from("/org/freedesktop/systemd1/unit").join(escape(&file));
                let mock_unit = object_server
                    .interface::<_, MockUnit>(path.to_string_lossy())
                    .await;
                if let Ok(mock_unit) = mock_unit {
                    let mut unit = mock_unit.get_mut().await;
                    unit.unit_file = String::from("disabled");
                    unit.unit_file_state_changed(mock_unit.signal_emitter())
                        .await?;
                }
            }
            Ok(res)
        }

        async fn mask_unit_files(
            &mut self,
            files: Vec<String>,
            _runtime: bool,
            _force: bool,
        ) -> fdo::Result<Vec<(String, String, String)>> {
            let mut res = Vec::new();
            for file in files {
                if let Some(state) = self.states.get(&file) {
                    if *state != EnableState::Masked {
                        self.states.insert(file.to_string(), EnableState::Masked);
                        res.push((String::default(), String::default(), file.to_string()));
                    }
                } else {
                    self.states.insert(file.to_string(), EnableState::Masked);
                    res.push((String::default(), String::default(), file.to_string()));
                }
            }
            Ok(res)
        }

        async fn unmask_unit_files(
            &mut self,
            files: Vec<String>,
            _runtime: bool,
        ) -> fdo::Result<Vec<(String, String, String)>> {
            let mut res = Vec::new();
            for file in files {
                if let Some(state) = self.states.get(&file) {
                    if *state == EnableState::Masked {
                        self.states.remove(&file);
                        res.push((String::default(), String::default(), file.to_string()));
                    }
                }
            }
            Ok(res)
        }

        async fn reload(&self) -> fdo::Result<()> {
            Ok(())
        }

        async fn get_unit(&mut self, unit: &str) -> fdo::Result<OwnedObjectPath> {
            Ok(
                ObjectPath::try_from(format!("/org/freedesktop/systemd1/unit/{}", escape(unit)))
                    .unwrap()
                    .into(),
            )
        }

        #[zbus(signal)]
        async fn job_removed(
            signal_emitter: &SignalEmitter<'_>,
            id: u32,
            job: ObjectPath<'_>,
            unit: &str,
            result: &str,
        ) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn test_unit() {
        let mut h = testing::start();
        let mut unit = MockUnit::default();
        unit.active = String::from("inactive");
        unit.unit_file = String::from("disabled");
        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1/unit/test_2eservice", unit)
            .await
            .expect("at");
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");
        let mock_unit = object_server
            .interface::<_, MockUnit>("/org/freedesktop/systemd1/unit/test_2eservice")
            .await
            .unwrap();

        sleep(Duration::from_millis(10)).await;

        let unit = SystemdUnit::new(&connection, "test.service")
            .await
            .expect("unit");
        assert_eq!(unit.active().await.unwrap(), ActiveState::Inactive);

        assert!(unit.start(JobMode::Fail).await.is_ok());
        assert_eq!(mock_unit.get().await.active, "active");
        assert_eq!(unit.active().await.unwrap(), ActiveState::Active);

        assert!(unit.restart(JobMode::Fail).await.is_ok());
        assert_eq!(mock_unit.get().await.active, "active");
        assert_eq!(unit.active().await.unwrap(), ActiveState::Active);

        assert!(unit.stop(JobMode::Fail).await.is_ok());
        assert_eq!(mock_unit.get().await.active, "inactive");
        assert_eq!(unit.active().await.unwrap(), ActiveState::Inactive);

        assert_eq!(mock_unit.get().await.unit_file, "disabled");
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Disabled);

        assert!(unit.enable().await.unwrap());
        assert_eq!(mock_unit.get().await.unit_file, "enabled");
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Enabled);

        assert!(unit.disable().await.unwrap());
        assert_eq!(mock_unit.get().await.unit_file, "disabled");
        assert_eq!(unit.enabled().await.unwrap(), EnableState::Disabled);
    }

    #[tokio::test]
    async fn test_manager() {
        let mut h = testing::start();
        let mut unit = MockUnit::default();
        unit.active = String::from("active");
        unit.unit_file = String::from("enabled");
        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1/unit/test_2eservice", unit)
            .await
            .expect("at");
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");

        sleep(Duration::from_millis(10)).await;

        let unit = SystemdUnit::new(&connection, "test.service")
            .await
            .expect("unit");
        assert!(unit.enable().await.unwrap());
        assert!(!unit.enable().await.unwrap());
        assert!(unit.disable().await.unwrap());
        assert!(!unit.disable().await.unwrap());
        assert!(unit.mask().await.unwrap());
        assert!(!unit.mask().await.unwrap());
        assert!(unit.unmask().await.unwrap());
        assert!(!unit.unmask().await.unwrap());
    }
}
