/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::Result;
use tokio::spawn;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};
use zbus::Connection;
use zbus::fdo::{InterfacesAdded, ObjectManagerProxy};
use zbus::names::OwnedInterfaceName;
use zbus::proxy::CacheProperties;
use zbus::zvariant::ObjectPath;

use crate::Service;
use crate::hardware::{InputPlumberConfig, InputPlumberTargetDevice, device_config};

#[zbus::proxy(
    interface = "org.shadowblip.Input.CompositeDevice",
    default_service = "org.shadowblip.InputPlumber"
)]
trait CompositeDevice {
    #[zbus(property)]
    fn target_devices(&self) -> Result<Vec<String>>;

    async fn set_target_devices(&self, devices: &[&str]) -> Result<()>;
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

#[cfg(test)]
mod test {
    use super::*;

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
