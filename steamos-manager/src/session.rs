/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 * Copyright © 2024 Igalia S.L.
 *
 * SPDX-License-Identifier: MIT
 */

#[cfg(test)]
use anyhow::anyhow;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::ErrorKind;
use strum::{Display, EnumString};
#[cfg(test)]
use tokio::fs::{create_dir_all, write};
use tokio::fs::{read_dir, try_exists};
use tokio::sync::mpsc::Sender;
use tokio::sync::{broadcast, oneshot};
use tokio_stream::StreamExt;
use tracing::debug;
use xdg::BaseDirectories;
use zbus::proxy::PropertyChanged;
use zbus::{Connection, fdo};

use crate::daemon::user::{Command as DaemonCommand, UserCommand};
use crate::manager::root::RootManagerProxy;
use crate::platform::session_config;
use crate::systemd::{ActiveState, JobMode, SystemdUnit};
use crate::{Service, path};

const CONFIG_PREFIX: &str = "/etc/sddm.conf.d";
const SESSION_CHECK_PATH: &str = "steamos.conf";
const CONFIG_PATH: &str = "zz-steamos-autologin.conf";
const TEMPORARY_CONFIG_PATH: &str = "zzt-steamos-temp-login.conf";

#[derive(Default, Deserialize, Serialize, Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "snake_case")]
pub enum LoginMode {
    #[default]
    Game,
    Desktop,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
#[repr(transparent)]
struct DesktopSession(pub String);

impl Default for DesktopSession {
    fn default() -> DesktopSession {
        DesktopSession(String::from("plasma.desktop"))
    }
}

impl From<String> for DesktopSession {
    fn from(val: String) -> DesktopSession {
        DesktopSession(val)
    }
}

#[derive(Clone, Deserialize, Serialize, Debug)]
pub(crate) struct SessionManagerState {
    default_login_mode: LoginMode,
    desktop_session: Option<DesktopSession>,
}

impl Default for SessionManagerState {
    fn default() -> SessionManagerState {
        SessionManagerState {
            default_login_mode: LoginMode::Game,
            desktop_session: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SessionManager {
    connection: Connection,
    manager: RootManagerProxy<'static>,
    channel: Sender<DaemonCommand>,
}

pub(crate) struct SessionManagerService {
    session: Connection,
    current_mode: LoginMode,
    channel: broadcast::Sender<SessionManagerMessage>,
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum SessionManagerMessage {
    LoginModeChanged(LoginMode),
}

pub(crate) async fn is_session_managed() -> Result<bool> {
    Ok(try_exists(path(CONFIG_PREFIX).join(SESSION_CHECK_PATH)).await?)
}

#[cfg(test)]
pub(crate) async fn make_managed() -> Result<()> {
    let check_path = path(CONFIG_PREFIX).join(SESSION_CHECK_PATH);
    create_dir_all(check_path.parent().ok_or(anyhow!("Couldn't make dir"))?).await?;
    write(check_path, "").await?;
    Ok(())
}

fn is_valid_desktop_session_name(path: &str) -> bool {
    if path.starts_with('.') {
        return false;
    }
    if !path.ends_with(".desktop") {
        return false;
    }
    if path.contains("gamescope") {
        return false;
    }
    true
}

pub(crate) async fn valid_desktop_sessions() -> Result<Vec<String>> {
    let mut sessions = Vec::new();
    for dir in BaseDirectories::new()
        .data_dirs
        .into_iter()
        .flat_map(|dir| [dir.join("wayland-sessions"), dir.join("xsessions")])
    {
        let mut entries = match read_dir(path(dir)).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if !is_valid_desktop_session_name(name) {
                continue;
            }
            sessions.push(name.to_string());
        }
    }
    Ok(sessions)
}

pub(crate) async fn is_valid_desktop_session(session: &str) -> Result<bool> {
    if !is_valid_desktop_session_name(session) {
        return Ok(false);
    }
    for dir in BaseDirectories::new()
        .data_dirs
        .into_iter()
        .flat_map(|dir| [dir.join("wayland-sessions"), dir.join("xsessions")])
    {
        if try_exists(path(dir).join(session)).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

impl SessionManager {
    pub(crate) async fn new(
        connection: Connection,
        system: &Connection,
        channel: Sender<DaemonCommand>,
    ) -> Result<SessionManager> {
        Ok(SessionManager {
            manager: RootManagerProxy::new(system).await?,
            connection,
            channel,
        })
    }

    pub(crate) async fn create_service(
        &self,
    ) -> Result<(
        SessionManagerService,
        broadcast::Receiver<SessionManagerMessage>,
    )> {
        let (tx, rx) = broadcast::channel(5);
        let service = SessionManagerService {
            channel: tx,
            current_mode: self.current_login_mode().await?,
            session: self.connection.clone(),
        };
        Ok((service, rx))
    }

    async fn unit_is_active(&self, unit: &str) -> Result<bool> {
        let unit = SystemdUnit::new(&self.connection, unit).await?;
        match unit.active().await {
            Ok(state) => Ok(state.is_active()),
            Err(e) => {
                let err: &fdo::Error = if let Some(e) = e.downcast_ref::<fdo::Error>() {
                    e
                } else if let Some(zbus::Error::FDO(e)) = e.downcast_ref::<zbus::Error>() {
                    e
                } else {
                    return Err(e);
                };
                if matches!(err, fdo::Error::UnknownObject(_)) {
                    return Ok(false);
                }
                Err(e)
            }
        }
    }

    pub(crate) async fn current_login_mode(&self) -> Result<LoginMode> {
        let config = session_config().await;
        if self.unit_is_active(&config.gamescope_session_service).await? {
            return Ok(LoginMode::Game);
        }
        Ok(LoginMode::Desktop)
    }

    async fn logout(&self) -> Result<()> {
        SystemdUnit::new(&self.connection, "graphical-session.target")
            .await?
            .stop(JobMode::Fail)
            .await?;
        Ok(())
    }

    pub(crate) async fn switch_to_login_mode(&self, mode: LoginMode) -> Result<()> {
        self.manager
            .set_temporary_session(self.session_for_mode(mode).await?.as_str())
            .await?;
        self.logout().await
    }

    async fn get_state(&self) -> Result<SessionManagerState> {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(DaemonCommand::ContextCommand(
                UserCommand::GetSessionManagerState(tx),
            ))
            .await?;
        Ok(rx.await?)
    }

    async fn write_state(&self, state: SessionManagerState) -> Result<()> {
        Ok(self
            .channel
            .send(DaemonCommand::ContextCommand(
                UserCommand::SetSessionManagerState(state),
            ))
            .await?)
    }

    pub(crate) async fn default_desktop_session(&self) -> Result<String> {
        let state = self.get_state().await?;
        match state.desktop_session {
            Some(session) => Ok(session.0),
            None => Ok(session_config().await.desktop),
        }
    }

    pub(crate) async fn set_default_desktop_session(&self, session: &str) -> Result<()> {
        ensure!(
            is_valid_desktop_session(session).await?,
            "Invalid desktop session {session}"
        );
        let mut state = self.get_state().await?;
        state.desktop_session = Some(session.to_string().into());
        if state.default_login_mode == LoginMode::Desktop {
            self.manager.set_default_session(session).await?;
        }
        self.write_state(state).await
    }

    pub(crate) async fn default_login_mode(&self) -> Result<LoginMode> {
        Ok(self.get_state().await?.default_login_mode)
    }

    pub(crate) async fn set_default_login_mode(&self, mode: LoginMode) -> Result<()> {
        let mut state = self.get_state().await?;
        state.default_login_mode = mode;
        self.manager
            .set_default_session(self.session_for_mode(mode).await?.as_str())
            .await?;
        self.write_state(state).await
    }

    pub(crate) async fn session_for_mode(&self, mode: LoginMode) -> Result<String> {
        match mode {
            LoginMode::Game => Ok(session_config().await.gamescope_session_desktop),
            LoginMode::Desktop => self.default_desktop_session().await,
        }
    }
}

pub(crate) mod root {
    use anyhow::{Result, ensure};
    use std::io::ErrorKind;
    use tokio::fs::{remove_file, write};

    use crate::path;
    use crate::session::{CONFIG_PATH, CONFIG_PREFIX, TEMPORARY_CONFIG_PATH};

    pub(crate) async fn clean_temporary_sessions() -> Result<()> {
        let prefix = path(CONFIG_PREFIX);

        match remove_file(prefix.join(TEMPORARY_CONFIG_PATH)).await {
            Ok(()) => (),
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    pub(crate) async fn set_temporary_session(session: &str) -> Result<()> {
        ensure!(
            !session.contains('\n'),
            "Session name cannot contain newlines"
        );
        Ok(write(
            path(CONFIG_PREFIX).join(TEMPORARY_CONFIG_PATH),
            format!("[Autologin]\nSession={session}\n").as_bytes(),
        )
        .await?)
    }

    pub(crate) async fn set_default_session(session: &str) -> Result<()> {
        ensure!(
            !session.contains('\n'),
            "Session name cannot contain newlines"
        );
        Ok(write(
            path(CONFIG_PREFIX).join(CONFIG_PATH),
            format!("[Autologin]\nSession={session}\n").as_bytes(),
        )
        .await?)
    }
}

impl SessionManagerService {
    async fn active_state_adapter(
        prop_changed: PropertyChanged<'_, String>,
    ) -> Result<ActiveState> {
        let prop = prop_changed.get().await?;
        Ok(ActiveState::try_from(prop.as_str())?)
    }
}

impl Service for SessionManagerService {
    const NAME: &'static str = "session-manager";

    async fn run(&mut self) -> Result<()> {
        let config = session_config().await;
        let unit = SystemdUnit::new(&self.session, &config.gamescope_session_service).await?;

        let stream = unit
            .proxy
            .receive_active_state_changed()
            .await
            .then(SessionManagerService::active_state_adapter);

        tokio::pin!(stream);

        if unit.active().await.unwrap_or(ActiveState::Failed) == ActiveState::Active {
            self.current_mode = LoginMode::Game;
        } else {
            self.current_mode = LoginMode::Desktop;
        }
        let _ = self
            .channel
            .send(SessionManagerMessage::LoginModeChanged(self.current_mode));

        loop {
            let Some(state) = stream.next().await else {
                return Ok(());
            };

            let Ok(state) = state else {
                continue;
            };

            debug!("Game mode changed to {state}");

            match state {
                ActiveState::Active => {
                    if self.current_mode == LoginMode::Game {
                        continue;
                    }
                    self.current_mode = LoginMode::Game;
                    let _ = self
                        .channel
                        .send(SessionManagerMessage::LoginModeChanged(LoginMode::Game));
                }
                ActiveState::Inactive | ActiveState::Deactivating | ActiveState::Failed => {
                    if self.current_mode == LoginMode::Desktop {
                        continue;
                    }
                    self.current_mode = LoginMode::Desktop;
                    let _ = self
                        .channel
                        .send(SessionManagerMessage::LoginModeChanged(LoginMode::Desktop));
                }
                _ => (),
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::systemd::test::{MockManager, MockUnit};
    use crate::testing;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::spawn;
    use tokio::sync::Notify;
    use tokio::sync::mpsc::channel;
    use tokio::time::sleep;
    use zbus::interface;

    #[derive(Debug, Default)]
    struct MockRootManager {
        temporary_session: String,
        default_session: String,
        notify: Arc<Notify>,
    }

    #[interface(name = "com.steampowered.SteamOSManager1.RootManager")]
    impl MockRootManager {
        async fn set_temporary_session(&mut self, session: &str) {
            self.temporary_session = session.to_string();
            self.notify.notify_one();
        }

        async fn set_default_session(&mut self, session: &str) {
            self.default_session = session.to_string();
            self.notify.notify_one();
        }
    }

    #[test]
    fn test_valid_desktop_session_names() {
        assert!(is_valid_desktop_session_name("plasma.desktop"));
        assert!(!is_valid_desktop_session_name(".plasma.desktop"));
        assert!(!is_valid_desktop_session_name("plasma"));
        assert!(!is_valid_desktop_session_name("gamescope.desktop"));
    }

    #[tokio::test]
    async fn test_valid_desktop_sessions() {
        let _handle = testing::start();

        create_dir_all(path("/usr/share/wayland-sessions"))
            .await
            .unwrap();
        create_dir_all(path("/usr/share/xsessions")).await.unwrap();

        write(path("/usr/share/wayland-sessions/plasma.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/wayland-sessions/gamescope.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/xsessions/plasmax11.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/xsessions/.fake.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/xsessions/fake"), b"").await.unwrap();

        assert_eq!(
            valid_desktop_sessions().await.unwrap(),
            &["plasma.desktop", "plasmax11.desktop"]
        );
    }

    #[tokio::test]
    async fn test_is_valid_desktop_session() {
        let _handle = testing::start();

        create_dir_all(path("/usr/share/wayland-sessions"))
            .await
            .unwrap();
        create_dir_all(path("/usr/share/xsessions")).await.unwrap();

        write(path("/usr/share/wayland-sessions/plasma.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/wayland-sessions/gamescope.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/xsessions/plasmax11.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/xsessions/.fake.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/xsessions/fake"), b"").await.unwrap();

        assert!(is_valid_desktop_session("plasma.desktop").await.unwrap());
        assert!(is_valid_desktop_session("plasmax11.desktop").await.unwrap());
        assert!(!is_valid_desktop_session("gamescope.desktop").await.unwrap());
        assert!(
            !is_valid_desktop_session("doesnotexist.desktop")
                .await
                .unwrap()
        );
        assert!(!is_valid_desktop_session(".fake.desktop").await.unwrap());
        assert!(!is_valid_desktop_session("fake").await.unwrap());
    }

    #[tokio::test]
    async fn test_get_state() {
        let mut handle = testing::start();
        let connection = handle.new_dbus().await.unwrap();
        let (tx, mut rx) = channel(2);

        let task = spawn(async move {
            while let Some(message) = rx.recv().await {
                match message {
                    DaemonCommand::ContextCommand(UserCommand::GetSessionManagerState(sender)) => {
                        _ = sender.send(SessionManagerState {
                            default_login_mode: LoginMode::Desktop,
                            desktop_session: Some(String::from("plasma.desktop").into()),
                        })
                    }
                    _ => (),
                }
            }
        });

        let manager = SessionManager::new(connection.clone(), &connection, tx)
            .await
            .unwrap();

        assert_eq!(
            manager.default_desktop_session().await.unwrap(),
            "plasma.desktop"
        );
        assert_eq!(
            manager.default_login_mode().await.unwrap(),
            LoginMode::Desktop
        );

        task.abort();
    }

    #[tokio::test]
    async fn test_write_state() {
        let mut handle = testing::start();
        let connection = handle.new_dbus().await.unwrap();
        let (tx, mut rx) = channel(2);
        connection
            .request_name("com.steampowered.SteamOSManager1")
            .await
            .unwrap();

        let manager = MockRootManager::default();
        connection
            .object_server()
            .at("/com/steampowered/SteamOSManager1", manager)
            .await
            .unwrap();

        let task = spawn(async move {
            let mut state = SessionManagerState::default();
            while let Some(message) = rx.recv().await {
                match message {
                    DaemonCommand::ContextCommand(UserCommand::GetSessionManagerState(sender)) => {
                        _ = sender.send(state.clone())
                    }
                    DaemonCommand::ContextCommand(UserCommand::SetSessionManagerState(
                        new_state,
                    )) => {
                        state = new_state;
                    }
                    _ => (),
                }
            }
        });

        sleep(Duration::from_millis(1)).await;

        create_dir_all(path("/usr/share/wayland-sessions"))
            .await
            .unwrap();
        create_dir_all(path("/usr/share/xsessions")).await.unwrap();

        write(path("/usr/share/xsessions/plasmax11.desktop"), b"")
            .await
            .unwrap();
        write(path("/usr/share/wayland-sessions/gamescope.desktop"), b"")
            .await
            .unwrap();
        let manager = SessionManager::new(connection.clone(), &connection, tx)
            .await
            .unwrap();

        assert_eq!(
            manager.default_desktop_session().await.unwrap(),
            DesktopSession::default().0
        );
        assert_eq!(manager.default_login_mode().await.unwrap(), LoginMode::Game);
        assert!(manager.get_state().await.unwrap().desktop_session.is_none());

        manager
            .set_default_login_mode(LoginMode::Desktop)
            .await
            .unwrap();

        assert_eq!(
            manager.default_login_mode().await.unwrap(),
            LoginMode::Desktop
        );
        assert_eq!(
            manager.default_desktop_session().await.unwrap(),
            DesktopSession::default().0
        );
        assert!(manager.get_state().await.unwrap().desktop_session.is_none());

        manager
            .set_default_desktop_session("city17.desktop")
            .await
            .unwrap_err();
        assert_eq!(
            manager.default_desktop_session().await.unwrap(),
            DesktopSession::default().0
        );
        assert!(manager.get_state().await.unwrap().desktop_session.is_none());

        write(path("/usr/share/xsessions/city17.desktop"), b"")
            .await
            .unwrap();
        manager
            .set_default_desktop_session("city17.desktop")
            .await
            .unwrap();
        assert_eq!(
            manager.default_desktop_session().await.unwrap(),
            "city17.desktop"
        );
        assert_eq!(
            manager.get_state().await.unwrap().desktop_session,
            Some(DesktopSession(String::from("city17.desktop")))
        );

        task.abort();
    }

    #[tokio::test]
    async fn test_temporary_session() {
        let mut handle = testing::start();
        let connection = handle.new_dbus().await.unwrap();
        let (tx, mut rx) = channel(2);
        connection
            .request_name("com.steampowered.SteamOSManager1")
            .await
            .unwrap();
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .unwrap();

        let manager = MockRootManager::default();
        let notify = manager.notify.clone();
        let mut unit = MockUnit::default();
        unit.active = String::from("inactive");
        unit.unit_file = String::from("disabled");

        let object_server = connection.object_server();
        object_server
            .at("/com/steampowered/SteamOSManager1", manager)
            .await
            .unwrap();
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .unwrap();
        object_server
            .at(
                "/org/freedesktop/systemd1/unit/graphical_2dsession_2etarget",
                unit,
            )
            .await
            .unwrap();

        let root_manager = object_server
            .interface::<_, MockRootManager>("/com/steampowered/SteamOSManager1")
            .await
            .unwrap();
        let unit = object_server
            .interface::<_, MockUnit>("/org/freedesktop/systemd1/unit/graphical_2dsession_2etarget")
            .await
            .unwrap();

        let task = spawn(async move {
            let mut state = SessionManagerState::default();
            while let Some(message) = rx.recv().await {
                match message {
                    DaemonCommand::ContextCommand(UserCommand::GetSessionManagerState(sender)) => {
                        _ = sender.send(state.clone())
                    }
                    DaemonCommand::ContextCommand(UserCommand::SetSessionManagerState(
                        new_state,
                    )) => {
                        state = new_state;
                    }
                    _ => (),
                }
            }
        });

        create_dir_all(path("/usr/share/wayland-sessions"))
            .await
            .unwrap();
        create_dir_all(path("/usr/share/xsessions")).await.unwrap();

        write(path("/usr/share/wayland-sessions/city17.desktop"), b"")
            .await
            .unwrap();
        write(
            path("/usr/share/wayland-sessions/gamescope-wayland.desktop"),
            b"",
        )
        .await
        .unwrap();

        sleep(Duration::from_millis(1)).await;

        let manager = SessionManager::new(connection.clone(), &connection, tx)
            .await
            .unwrap();

        manager
            .switch_to_login_mode(LoginMode::Desktop)
            .await
            .unwrap();
        notify.notified().await;
        assert_eq!(
            root_manager.get().await.temporary_session,
            DesktopSession::default().0
        );
        {
            let mut unit = unit.get_mut().await;
            assert_eq!(unit.active, "inactive");
            unit.active = String::from("active");
        }

        manager.switch_to_login_mode(LoginMode::Game).await.unwrap();
        notify.notified().await;
        assert_eq!(
            root_manager.get().await.temporary_session,
            "gamescope-session-ogui-steam.desktop"
        );
        {
            let mut unit = unit.get_mut().await;
            assert_eq!(unit.active, "inactive");
            unit.active = String::from("active");
        }

        task.abort();
    }
}
