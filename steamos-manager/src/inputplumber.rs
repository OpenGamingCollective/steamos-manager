/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::Result;
use tokio::spawn;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::fdo::{InterfacesAdded, ObjectManagerProxy};
use zbus::names::OwnedInterfaceName;
use zbus::proxy::CacheProperties;
use zbus::zvariant::ObjectPath;

use crate::Service;
use crate::hardware::{InputPlumberConfig, InputPlumberTargetDevice, device_config};
use crate::manager::root::RootManagerProxy;
use crate::session::{LoginMode, SessionManagerMessage};

#[zbus::proxy(
    interface = "org.shadowblip.Input.CompositeDevice",
    default_service = "org.shadowblip.InputPlumber"
)]
trait CompositeDevice {
    #[zbus(property)]
    fn target_devices(&self) -> Result<Vec<String>>;

    async fn set_target_devices(&self, devices: &[&str]) -> Result<()>;

    #[zbus(property)]
    fn set_intercept_mode(&self, mode: u32) -> Result<()>;
}

#[zbus::proxy(
    interface = "org.shadowblip.Input.Target",
    default_service = "org.shadowblip.InputPlumber"
)]
trait Target {
    #[zbus(property)]
    fn device_type(&self) -> Result<String>;
}

#[derive(Clone, Debug)]
pub struct DeckService {
    connection: Connection,
    composite_device_iface_name: OwnedInterfaceName,
}

pub(crate) struct InterceptModeService {
    pub(crate) manager: RootManagerProxy<'static>,
    pub(crate) channel: broadcast::Receiver<SessionManagerMessage>,
}

/// Returns the target devices to apply, or `None` if target device management is
/// disabled for this device, which is the case whenever no target devices are
/// configured.
fn resolve_targets(config: Option<&InputPlumberConfig>) -> Option<&[InputPlumberTargetDevice]> {
    let targets = config?.target_devices.as_slice();
    if targets.is_empty() {
        return None;
    }
    Some(targets)
}

impl DeckService {
    pub fn init(connection: Connection) -> DeckService {
        DeckService {
            connection,
            composite_device_iface_name: OwnedInterfaceName::try_from(
                "org.shadowblip.Input.CompositeDevice",
            )
            .unwrap(),
        }
    }

    async fn check_devices(&self, object_manager: &ObjectManagerProxy<'_>) -> Result<()> {
        for (path, ifaces) in object_manager.get_managed_objects().await? {
            if ifaces.contains_key(&self.composite_device_iface_name) {
                self.make_deck(&path).await?;
            }
        }
        Ok(())
    }

    async fn make_deck_from_ifaces_added(&self, msg: InterfacesAdded) -> Result<()> {
        let args = msg.args()?;
        if !args
            .interfaces_and_properties
            .contains_key(&self.composite_device_iface_name.as_ref())
        {
            return Ok(());
        }
        debug!("New CompositeDevice found at {}", args.object_path());
        self.make_deck(args.object_path()).await
    }

    async fn is_deck(&self, device: &CompositeDeviceProxy<'_>) -> Result<bool> {
        let targets = device.target_devices().await?;
        if targets.len() != 1 {
            return Ok(false);
        }

        let target = TargetProxy::builder(&self.connection)
            .path(targets[0].as_str())?
            .build()
            .await?;
        Ok(target.device_type().await? == InputPlumberTargetDevice::DeckUhid.as_ref())
    }

    async fn make_deck(&self, path: &ObjectPath<'_>) -> Result<()> {
        if !path
            .as_str()
            .starts_with("/org/shadowblip/InputPlumber/CompositeDevice")
        {
            return Ok(());
        }
        let proxy = CompositeDeviceProxy::builder(&self.connection)
            .cache_properties(CacheProperties::No)
            .path(path)?
            .build()
            .await?;
        if !self.is_deck(&proxy).await? {
            let config = device_config().await?;
            let Some(targets) =
                resolve_targets(config.as_ref().and_then(|c| c.inputplumber.as_ref()))
            else {
                debug!("Target device management disabled for CompositeDevice {path}");
                return Ok(());
            };
            let new_targets: Vec<&str> = targets.iter().map(AsRef::as_ref).collect();

            debug!(
                "Changing CompositeDevice {} into {:?} type",
                path, new_targets
            );
            proxy.set_target_devices(&new_targets).await
        } else {
            debug!("CompositeDevice {} is already `deck-uhid` type", path);
            Ok(())
        }
    }
}

async fn reset_intercept_mode(connection: &Connection, path: &ObjectPath<'_>) -> Result<()> {
    let proxy = CompositeDeviceProxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .path(path)?
        .build()
        .await?;
    debug!("Resetting intercept mode on CompositeDevice {path}");
    proxy.set_intercept_mode(0).await
}

/// Sets every InputPlumber composite device back to intercept mode `None`.
pub(crate) async fn reset_intercept_modes(connection: &Connection) -> Result<()> {
    let object_manager = match ObjectManagerProxy::new(
        connection,
        "org.shadowblip.InputPlumber",
        "/org/shadowblip/InputPlumber",
    )
    .await
    {
        Ok(object_manager) => object_manager,
        Err(e) => {
            debug!("InputPlumber not available, not resetting intercept modes: {e}");
            return Ok(());
        }
    };

    let objects = match object_manager.get_managed_objects().await {
        Ok(objects) => objects,
        Err(e) => {
            debug!("Can't query InputPlumber devices, not resetting intercept modes: {e}");
            return Ok(());
        }
    };

    let composite_device_iface_name =
        OwnedInterfaceName::try_from("org.shadowblip.Input.CompositeDevice")?;

    for (path, ifaces) in objects {
        if !ifaces.contains_key(&composite_device_iface_name) {
            continue;
        }
        if let Err(e) = reset_intercept_mode(connection, &path).await {
            warn!("Couldn't reset intercept mode on CompositeDevice {path}: {e}");
        }
    }

    Ok(())
}

impl Service for DeckService {
    const NAME: &'static str = "inputplumber";

    async fn run(&mut self) -> Result<()> {
        let object_manager = ObjectManagerProxy::new(
            &self.connection,
            "org.shadowblip.InputPlumber",
            "/org/shadowblip/InputPlumber",
        )
        .await?;
        let mut iface_added = object_manager.receive_interfaces_added().await?;

        // This needs to be done in a separate task to prevent the
        // signal listener from filling up. We just clone `self`
        // for this since it doesn't hold any state.
        let ctx = self.clone();
        spawn(async move {
            if let Err(e) = ctx.check_devices(&object_manager).await {
                info!("Can't query initial InputPlumber devices: {e}");
            }
        });

        loop {
            tokio::select! {
                Some(iface) = iface_added.next() => {
                    let ctx = self.clone();
                    spawn(async move {
                        ctx.make_deck_from_ifaces_added(iface).await
                    });
                }
            }
        }
    }
}

impl Service for InterceptModeService {
    const NAME: &'static str = "inputplumber-intercept-mode";

    async fn run(&mut self) -> Result<()> {
        loop {
            match self.channel.recv().await {
                Ok(SessionManagerMessage::LoginModeChanged(LoginMode::Desktop)) => {
                    if let Err(e) = self.manager.reset_intercept_modes().await {
                        warn!(
                            "Could not reset InputPlumber intercept modes when entering desktop mode: {e}"
                        );
                    }
                }
                Ok(SessionManagerMessage::LoginModeChanged(LoginMode::Game)) => (),
                Err(RecvError::Closed) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::testing;
    use std::time::Duration;
    use tokio::spawn;
    use tokio::time::sleep;
    use zbus::fdo::{self, ObjectManager};

    #[derive(Default)]
    struct MockRootManager {
        reset_count: u32,
    }

    #[zbus::interface(name = "com.steampowered.SteamOSManager1.RootManager")]
    impl MockRootManager {
        async fn reset_intercept_modes(&mut self) -> fdo::Result<()> {
            self.reset_count += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn intercept_mode_service_resets_on_desktop_only() {
        let mut h = testing::start();
        let connection = h.new_dbus().await.expect("dbus");
        let object_server = connection.object_server().clone();

        object_server
            .at(
                "/com/steampowered/SteamOSManager1",
                MockRootManager::default(),
            )
            .await
            .expect("at");
        connection
            .request_name("com.steampowered.SteamOSManager1")
            .await
            .expect("request_name");

        let client = h.new_connection().await.expect("connection");
        let (tx, rx) = broadcast::channel(5);
        let mut service = InterceptModeService {
            manager: RootManagerProxy::new(&client).await.expect("proxy"),
            channel: rx,
        };
        let handle = spawn(async move { service.run().await });

        let mock = object_server
            .interface::<_, MockRootManager>("/com/steampowered/SteamOSManager1")
            .await
            .expect("interface");

        tx.send(SessionManagerMessage::LoginModeChanged(LoginMode::Game))
            .expect("send");
        sleep(Duration::from_millis(10)).await;
        assert_eq!(mock.get().await.reset_count, 0);

        tx.send(SessionManagerMessage::LoginModeChanged(LoginMode::Desktop))
            .expect("send");
        sleep(Duration::from_millis(10)).await;
        assert_eq!(mock.get().await.reset_count, 1);

        handle.abort();
    }

    #[derive(Default)]
    struct MockCompositeDevice {
        intercept_mode: u32,
        fail: bool,
    }

    #[zbus::interface(name = "org.shadowblip.Input.CompositeDevice")]
    impl MockCompositeDevice {
        #[zbus(property)]
        async fn intercept_mode(&self) -> u32 {
            self.intercept_mode
        }

        #[zbus(property)]
        async fn set_intercept_mode(&mut self, mode: u32) -> fdo::Result<()> {
            if self.fail {
                return Err(fdo::Error::Failed(String::from("mock failure")));
            }
            self.intercept_mode = mode;
            Ok(())
        }
    }

    struct MockNotAComposite;

    #[zbus::interface(name = "org.shadowblip.Input.Source")]
    impl MockNotAComposite {
        #[zbus(property)]
        async fn unique_id(&self) -> String {
            String::from("mock")
        }
    }

    async fn mock_intercept_mode(object_server: &zbus::ObjectServer, path: &str) -> u32 {
        object_server
            .interface::<_, MockCompositeDevice>(path)
            .await
            .expect("interface")
            .get()
            .await
            .intercept_mode
    }

    #[tokio::test]
    async fn reset_intercept_modes_resets_every_device() {
        let mut h = testing::start();
        let connection = h.new_dbus().await.expect("dbus");
        let object_server = connection.object_server().clone();

        object_server
            .at(
                "/org/shadowblip/InputPlumber/CompositeDevice0",
                MockCompositeDevice {
                    intercept_mode: 2,
                    fail: false,
                },
            )
            .await
            .expect("at");
        object_server
            .at(
                "/org/shadowblip/InputPlumber/CompositeDevice1",
                MockCompositeDevice {
                    intercept_mode: 1,
                    fail: false,
                },
            )
            .await
            .expect("at");
        object_server
            .at(
                "/org/shadowblip/InputPlumber/SourceDevice0",
                MockNotAComposite,
            )
            .await
            .expect("at");
        object_server
            .at("/org/shadowblip/InputPlumber", ObjectManager {})
            .await
            .expect("at");
        connection
            .request_name("org.shadowblip.InputPlumber")
            .await
            .expect("request_name");

        let client = h.new_connection().await.expect("connection");
        reset_intercept_modes(&client).await.expect("reset");

        assert_eq!(
            mock_intercept_mode(
                &object_server,
                "/org/shadowblip/InputPlumber/CompositeDevice0"
            )
            .await,
            0
        );
        assert_eq!(
            mock_intercept_mode(
                &object_server,
                "/org/shadowblip/InputPlumber/CompositeDevice1"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn reset_intercept_modes_continues_past_a_failing_device() {
        let mut h = testing::start();
        let connection = h.new_dbus().await.expect("dbus");
        let object_server = connection.object_server().clone();

        object_server
            .at(
                "/org/shadowblip/InputPlumber/CompositeDevice0",
                MockCompositeDevice {
                    intercept_mode: 2,
                    fail: true,
                },
            )
            .await
            .expect("at");
        object_server
            .at(
                "/org/shadowblip/InputPlumber/CompositeDevice1",
                MockCompositeDevice {
                    intercept_mode: 2,
                    fail: false,
                },
            )
            .await
            .expect("at");
        object_server
            .at("/org/shadowblip/InputPlumber", ObjectManager {})
            .await
            .expect("at");
        connection
            .request_name("org.shadowblip.InputPlumber")
            .await
            .expect("request_name");

        let client = h.new_connection().await.expect("connection");
        reset_intercept_modes(&client).await.expect("reset");

        assert_eq!(
            mock_intercept_mode(
                &object_server,
                "/org/shadowblip/InputPlumber/CompositeDevice0"
            )
            .await,
            2
        );
        assert_eq!(
            mock_intercept_mode(
                &object_server,
                "/org/shadowblip/InputPlumber/CompositeDevice1"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn reset_intercept_modes_ignores_missing_inputplumber() {
        let mut h = testing::start();
        let _connection = h.new_dbus().await.expect("dbus");
        let client = h.new_connection().await.expect("connection");

        assert!(reset_intercept_modes(&client).await.is_ok());
    }

    #[test]
    fn resolve_targets_disabled_without_config() {
        assert_eq!(resolve_targets(None), Option::None);
    }

    #[test]
    fn resolve_targets_passes_through_configured_list() {
        let config = InputPlumberConfig {
            target_devices: vec![
                InputPlumberTargetDevice::DeckUhid,
                InputPlumberTargetDevice::Keyboard,
            ],
        };
        assert_eq!(
            resolve_targets(Some(&config)),
            Some(
                [
                    InputPlumberTargetDevice::DeckUhid,
                    InputPlumberTargetDevice::Keyboard,
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn resolve_targets_disabled_by_empty_list() {
        let config = InputPlumberConfig {
            target_devices: Vec::new(),
        };
        assert_eq!(resolve_targets(Some(&config)), Option::None);
    }
}
