/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, anyhow, bail, ensure};
use glob::Pattern;
use linux_cec::VendorId;
use num_enum::TryFromPrimitive;
use serde::Deserialize;
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::path::Path;
use std::str::FromStr;
use strum::{Display, EnumString};
use tokio::fs::{read_dir, read_to_string};
#[cfg(not(test))]
use tokio::sync::OnceCell;
use tracing::{debug, error};
use zbus::Connection;

use crate::cec::HdmiCecHardware;
use crate::gpu::{GpuPerformanceLevelDriverType, GpuPowerProfileDriverType};
use crate::path;
use crate::platform::{ServiceConfig, platform_config};
use crate::power::{
    BATTERY_DEFAULT_SUGGESTED_MINIMUM_LIMIT, BatteryChargeLimitMethod, TdpLimitingMethod,
    find_hwmon,
};
use crate::process::{run_script, script_exit_code};
use crate::systemd::{JobMode, SystemdUnit};
use crate::write_synced;

#[cfg(not(test))]
static DEVICE_CONFIG: OnceCell<Option<DeviceConfig>> = OnceCell::const_new();

const SYS_VENDOR_PATH: &str = "/sys/class/dmi/id/sys_vendor";
const BOARD_NAME_PATH: &str = "/sys/class/dmi/id/board_name";
const PRODUCT_NAME_PATH: &str = "/sys/class/dmi/id/product_name";
#[cfg(not(test))]
const DEVICE_CONFIG_PATH: &str = "/usr/share/steamos-manager/devices";
#[cfg(test)]
const DEVICE_CONFIG_PATH: &str = "../data/devices";

#[derive(Display, EnumString, PartialEq, Debug, Default, Copy, Clone)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub(crate) enum SteamDeckVariant {
    #[default]
    Unknown,
    Jupiter,
    Galileo,
}

#[derive(Debug, Clone, Copy)]
enum MatchType {
    Exact,
    Wildcard,
    Both,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(ascii_case_insensitive)]
#[repr(u32)]
pub enum ECLoggingState {
    #[strum(
        to_string = "disabled",
        serialize = "off",
        serialize = "disable",
        serialize = "0"
    )]
    Disabled = 0,
    #[strum(
        to_string = "enabled",
        serialize = "on",
        serialize = "enable",
        serialize = "1"
    )]
    Enabled = 1,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(ascii_case_insensitive)]
#[repr(u32)]
pub enum FanControlState {
    #[strum(to_string = "BIOS")]
    Bios = 0,
    #[strum(to_string = "OS")]
    Os = 1,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(ascii_case_insensitive)]
#[repr(u32)]
pub enum FactoryResetKind {
    User = 1,
    OS = 2,
    All = 3,
}

#[derive(Display, EnumString, strum::AsRefStr, Deserialize, Debug, Clone, PartialEq, Copy)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub(crate) enum InputPlumberTargetDevice {
    DeckUhid,
    Keyboard,
    Mouse,
}

#[derive(Clone, Default, Deserialize, Debug)]
#[serde(default)]
pub struct DeviceConfig {
    pub(crate) device: Vec<DeviceMatch>,
    pub(crate) tdp_limit: Option<TdpLimitConfig>,
    pub(crate) fan_speed: Option<FanSpeedConfig>,
    pub(crate) gpu_performance: Option<GpuPerformanceConfig>,
    pub(crate) gpu_power_profile: Option<GpuPowerProfileConfig>,
    pub(crate) battery_charge_limit: Option<BatteryChargeLimitConfig>,
    pub(crate) performance_profile: Option<PerformanceProfileConfig>,
    pub(crate) inputplumber: Option<InputPlumberConfig>,
    pub(crate) cec_hw: Option<HdmiCecConfig>,
    pub(crate) firmware_debug: Option<ServiceConfig>,
}

#[derive(Clone, Default, Deserialize, Debug)]
pub(crate) struct BatteryChargeLimitConfig {
    #[serde(default = "battery_charge_limit_minimum_default")]
    pub suggested_minimum_limit: i32,
    pub method: BatteryChargeLimitMethod,
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct DeviceMatch {
    pub dmi: Option<DmiMatch>,
    pub device: String,
    pub variant: String,
    pub friendly_name: Option<String>,
    pub oui: Option<VendorId>,
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct DmiMatch {
    pub sys_vendor: String,
    pub board_name: Option<String>,
    pub product_name: Option<String>,
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct FirmwareAttributeConfig {
    pub attribute: String,
    pub performance_profile: Option<String>,
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct GpuPerformanceConfig {
    pub driver: GpuPerformanceLevelDriverType,
    pub clocks: Option<RangeConfig<u32>>,
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct GpuPowerProfileConfig {
    pub driver: GpuPowerProfileDriverType,
}

#[derive(Clone, Default, Deserialize, Debug)]
#[serde(default)]
pub(crate) struct HdmiCecConfig {
    pub hardware: Option<HdmiCecHardware>,
}

#[derive(Clone, Deserialize, Debug, Default)]
#[serde(default)]
pub(crate) struct InputPlumberConfig {
    pub target_devices: Vec<InputPlumberTargetDevice>,
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct PerformanceProfileConfig {
    pub suggested_default: String,
    pub platform_profile_name: String,
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct RangeConfig<T: Clone> {
    pub min: T,
    pub max: T,
}

impl<T> Copy for RangeConfig<T> where T: Copy {}

impl<T: Clone> RangeConfig<T> {
    #[allow(unused)]
    pub(crate) fn new(min: T, max: T) -> RangeConfig<T> {
        RangeConfig { min, max }
    }
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct TdpLimitConfig {
    pub method: TdpLimitingMethod,
    pub range: Option<RangeConfig<u32>>,
    pub download_mode_limit: Option<NonZeroU32>,
    pub firmware_attribute: Option<FirmwareAttributeConfig>,
    pub performance_profile: Option<String>,
}

#[derive(Clone, Deserialize, Debug)]
pub(crate) struct FanSpeedConfig {
    pub hwmon: String,
    pub attribute: String,
    pub download_mode_fan_speed: Option<NonZeroU32>,
}

async fn try_read_to_string<S: AsRef<Path>>(path: S) -> std::io::Result<Option<String>> {
    match read_to_string(path.as_ref()).await {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl DeviceConfig {
    /// Check if pattern contains wildcard characters
    fn is_wildcard_pattern(pattern: &str) -> bool {
        pattern.contains('*') || pattern.contains('?')
    }

    /// Check if single device entry is exact match (no wildcards)
    fn is_exact_device_match(device: &DeviceMatch) -> bool {
        if let Some(dmi) = &device.dmi {
            !Self::is_wildcard_pattern(&dmi.sys_vendor)
                && !dmi
                    .board_name
                    .as_ref()
                    .map_or(false, |s| Self::is_wildcard_pattern(s))
                && !dmi
                    .product_name
                    .as_ref()
                    .map_or(false, |s| Self::is_wildcard_pattern(s))
        } else {
            false
        }
    }

    /// Use glob for wildcard matching
    fn dmi_glob_match(pattern: &str, text: &str) -> bool {
        match Pattern::new(pattern) {
            Ok(glob_pattern) => {
                let result = glob_pattern.matches(text);
                tracing::trace!("Pattern '{}' matches '{}': {}", pattern, text, result);
                result
            }
            Err(e) => {
                tracing::warn!("Invalid glob pattern '{}': {}", pattern, e);
                pattern == text // Fallback to exact match
            }
        }
    }

    pub(crate) async fn device_match(&self) -> Result<Option<&'_ DeviceMatch>> {
        self.device_match_by_type(MatchType::Both).await
    }

    /// Execute only exact device matching across all device entries
    pub(crate) async fn exact_device_match(&self) -> Result<Option<&'_ DeviceMatch>> {
        self.device_match_by_type(MatchType::Exact).await
    }

    /// Execute only wildcard device matching across all device entries
    pub(crate) async fn wildcard_device_match(&self) -> Result<Option<&'_ DeviceMatch>> {
        self.device_match_by_type(MatchType::Wildcard).await
    }

    /// Unified device matching logic with configurable match type
    async fn device_match_by_type(&self, match_type: MatchType) -> Result<Option<&'_ DeviceMatch>> {
        let Some(sys_vendor) = try_read_to_string(path(SYS_VENDOR_PATH)).await? else {
            return Ok(None);
        };
        let sys_vendor = sys_vendor.trim_end();
        let board_name = try_read_to_string(path(BOARD_NAME_PATH)).await?;
        let board_name = board_name.as_ref().map(|name| name.trim_end());
        let product_name = try_read_to_string(path(PRODUCT_NAME_PATH)).await?;
        let product_name = product_name.as_ref().map(|name| name.trim_end());

        tracing::info!(
            "DMI Info - sys_vendor: '{}', board_name: '{}', product_name: '{}'",
            sys_vendor,
            board_name.unwrap_or("None"),
            product_name.unwrap_or("None")
        );

        // Determine which phases to run based on match_type
        let run_exact = matches!(match_type, MatchType::Exact | MatchType::Both);
        let run_wildcard = matches!(match_type, MatchType::Wildcard | MatchType::Both);

        // Phase 1: Exact matching (if needed)
        if run_exact {
            for device in &self.device {
                if Self::is_exact_device_match(device) {
                    if let Some(dmi) = &device.dmi {
                        tracing::debug!(
                            "Testing exact match for device: {} ({})",
                            device.device,
                            device.variant
                        );

                        if dmi.sys_vendor != sys_vendor {
                            continue;
                        }
                        if board_name.is_some() && board_name == dmi.board_name.as_deref() {
                            tracing::info!(
                                "Exact match found: {} ({}) via board_name",
                                device.device,
                                device.variant
                            );
                            return Ok(Some(device));
                        }
                        if product_name.is_some() && product_name == dmi.product_name.as_deref() {
                            tracing::info!(
                                "Exact match found: {} ({}) via product_name",
                                device.device,
                                device.variant
                            );
                            return Ok(Some(device));
                        }
                    }
                }
            }
        }

        // Phase 2: Wildcard matching (if needed)
        if run_wildcard {
            for device in self.device.iter() {
                if !Self::is_exact_device_match(device) {
                    if let Some(dmi) = &device.dmi {
                        tracing::debug!(
                            "Testing wildcard match for device: {} ({})",
                            device.device,
                            device.variant
                        );

                        if !Self::dmi_glob_match(&dmi.sys_vendor, sys_vendor) {
                            continue;
                        }
                        if let (Some(pattern), Some(value)) = (dmi.board_name.as_deref(), board_name) {
                            if Self::dmi_glob_match(pattern, value) {
                                tracing::info!(
                                    "Wildcard match found: {} ({}) via board_name pattern '{}'",
                                    device.device,
                                    device.variant,
                                    pattern
                                );
                                return Ok(Some(device));
                            }
                        }
                        if let (Some(pattern), Some(value)) = (dmi.product_name.as_deref(), product_name) {
                            if Self::dmi_glob_match(pattern, value) {
                                tracing::info!(
                                    "Wildcard match found: {} ({}) via product_name pattern '{}'",
                                    device.device,
                                    device.variant,
                                    pattern
                                );
                                return Ok(Some(device));
                            }
                        }
                    }
                }
            }
        }

        // Only log warning for full device_match (Both mode)
        if matches!(match_type, MatchType::Both) {
            tracing::warn!(
                "No device match found for sys_vendor='{}', board_name='{}', product_name='{}'",
                sys_vendor,
                board_name.unwrap_or("None"),
                product_name.unwrap_or("None")
            );
        }
        Ok(None)
    }

    async fn load() -> Result<Option<DeviceConfig>> {
        // 1. Collect all configuration files first
        let mut configs = Vec::new();
        let mut dir = read_dir(DEVICE_CONFIG_PATH)
            .await
            .inspect_err(|err| error!("Failed to scan device configs: {err}"))?;
        while let Some(config) = dir.next_entry().await? {
            let path = config.path();
            if let Some(ext) = path.extension() {
                if ext != "toml" {
                    continue;
                }
            } else {
                continue;
            }
            let config = match DeviceConfig::read_from_path(&path).await {
                Ok(config) => config,
                Err(e) => {
                    error!("Failed to read config file {}: {e}", path.display());
                    continue;
                }
            };
            configs.push(config);
        }

        // 2. Phase 1: Global exact matching priority
        for config in &configs {
            if config.exact_device_match().await?.is_some() {
                return Ok(Some(config.clone()));
            }
        }

        // 3. Phase 2: Global wildcard matching
        let mut catch_all = None;
        for config in &configs {
            let Some(device) = config.wildcard_device_match().await? else {
                continue;
            };
            if device.dmi.as_ref().is_none_or(|dmi| dmi.sys_vendor == "*") {
                catch_all.get_or_insert(config);
            } else {
                return Ok(Some(config.clone()));
            }
        }

        Ok(catch_all.cloned())
    }

    async fn read_from_path(path: impl AsRef<Path>) -> Result<DeviceConfig> {
        let config = read_to_string(&path).await?;
        Ok(toml::from_str(config.as_ref())?)
    }

    pub async fn load_from_path(path: impl AsRef<Path>) -> Result<()> {
        let config = DeviceConfig::read_from_path(&path).await.inspect_err(|e| {
            error!(
                "Failed to read config file {}: {e}",
                path.as_ref().display()
            )
        })?;
        #[cfg(not(test))]
        DEVICE_CONFIG
            .set(Some(config))
            .map_err(|_| anyhow!("Device config already configured"))?;
        #[cfg(test)]
        {
            let test = crate::testing::current();
            let mut device_config = test.device_config.lock().await;
            if device_config.is_some() {
                return Err(anyhow!("Device config already configured"));
            }
            *device_config = Some(config);
        }
        Ok(())
    }
}

#[cfg(not(test))]
pub(crate) async fn device_config() -> Result<&'static Option<DeviceConfig>> {
    DEVICE_CONFIG.get_or_try_init(DeviceConfig::load).await
}

#[cfg(test)]
pub(crate) async fn device_config() -> Result<Option<DeviceConfig>> {
    let test = crate::testing::current();
    let config = (*test.device_config.lock().await).clone();
    Ok(config)
}

pub(crate) async fn steam_deck_variant() -> Result<SteamDeckVariant> {
    let Some(sys_vendor) = try_read_to_string(path(SYS_VENDOR_PATH)).await? else {
        return Ok(SteamDeckVariant::Unknown);
    };
    if sys_vendor.trim_end() != "Valve" {
        return Ok(SteamDeckVariant::Unknown);
    }
    let board_name = read_to_string(path(BOARD_NAME_PATH)).await?;
    Ok(SteamDeckVariant::from_str(board_name.trim_end()).unwrap_or_default())
}

pub(crate) async fn device_type() -> Result<String> {
    Ok(device_variant().await?.0)
}

pub(crate) async fn device_variant() -> Result<(String, String)> {
    let Some(device) = device_config().await? else {
        return Ok((String::from("unknown"), String::from("unknown")));
    };
    let Some(device) = device.device_match().await? else {
        return Ok((String::from("unknown"), String::from("unknown")));
    };
    Ok((device.device.to_string(), device.variant.to_string()))
}

pub(crate) struct FanControl {
    connection: Connection,
}

impl FanControl {
    pub fn new(connection: Connection) -> FanControl {
        FanControl { connection }
    }

    pub async fn get_state(&self) -> Result<FanControlState> {
        let config = platform_config().await?;
        match config
            .as_ref()
            .and_then(|config| config.fan_control.as_ref())
        {
            Some(ServiceConfig::Systemd(service)) => {
                let jupiter_fan_control = SystemdUnit::new(&self.connection, service).await?;
                let active = jupiter_fan_control.active().await?;
                Ok(if active.is_active() {
                    FanControlState::Os
                } else {
                    FanControlState::Bios
                })
            }
            Some(ServiceConfig::Script {
                start: _,
                stop: _,
                status,
            }) => {
                let res = script_exit_code(&status.script, &status.script_args).await?;
                ensure!(res >= 0, "Script exited abnormally");
                Ok(FanControlState::try_from(res as u32)?)
            }
            None => bail!("Fan control not configured"),
        }
    }

    pub async fn set_state(&self, state: FanControlState) -> Result<()> {
        // Run what steamos-polkit-helpers/jupiter-fan-control does
        let config = platform_config().await?;
        match config
            .as_ref()
            .and_then(|config| config.fan_control.as_ref())
        {
            Some(ServiceConfig::Systemd(service)) => {
                let jupiter_fan_control = SystemdUnit::new(&self.connection, service).await?;
                match state {
                    FanControlState::Os => jupiter_fan_control.start(JobMode::Fail).await?,
                    FanControlState::Bios => jupiter_fan_control.stop(JobMode::Fail).await?,
                };
            }
            Some(ServiceConfig::Script {
                start,
                stop,
                status: _,
            }) => match state {
                FanControlState::Os => run_script(&start.script, &start.script_args).await?,
                FanControlState::Bios => run_script(&stop.script, &stop.script_args).await?,
            },
            None => bail!("Fan control not configured"),
        }
        Ok(())
    }

    pub async fn set_speed(&self, rpm: u32) -> Result<()> {
        let config = device_config().await?;
        let Some(config) = config.as_ref().and_then(|config| config.fan_speed.as_ref()) else {
            bail!("Fan speed not configured");
        };
        let base = find_hwmon(&config.hwmon).await?;
        let path = base.join(&config.attribute);
        debug!("Writing fan speed {rpm} to {}", path.display());
        Ok(write_synced(path, rpm.to_string().as_bytes()).await?)
    }
}

fn battery_charge_limit_minimum_default() -> i32 {
    BATTERY_DEFAULT_SUGGESTED_MINIMUM_LIMIT
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::error::to_zbus_fdo_error;
    use crate::platform::{PlatformConfig, ServiceConfig};
    use crate::{enum_on_off, enum_roundtrip, testing};
    use std::time::Duration;
    use tokio::fs::{create_dir_all, read_to_string, write};
    use tokio::time::sleep;
    use zbus::fdo;
    use zbus::zvariant::{ObjectPath, OwnedObjectPath};

    const STEAMDECK_HWMON: &str = "hwmon3";

    pub(crate) async fn fake_model(model: SteamDeckVariant) -> Result<()> {
        create_dir_all(path("/sys/class/dmi/id")).await?;
        match model {
            SteamDeckVariant::Unknown => {
                write(path(SYS_VENDOR_PATH), "LENOVO\n").await?;
                write(path(BOARD_NAME_PATH), "INVALID\n").await?;
                write(path(PRODUCT_NAME_PATH), "INVALID\n").await?;
            }
            SteamDeckVariant::Jupiter => {
                write(path(SYS_VENDOR_PATH), "Valve\n").await?;
                write(path(BOARD_NAME_PATH), "Jupiter\n").await?;
                write(path(PRODUCT_NAME_PATH), "Jupiter\n").await?;
            }
            SteamDeckVariant::Galileo => {
                write(path(SYS_VENDOR_PATH), "Valve\n").await?;
                write(path(BOARD_NAME_PATH), "Galileo\n").await?;
                write(path(PRODUCT_NAME_PATH), "Galileo\n").await?;
            }
        }
        if let Some(config) = DeviceConfig::load().await? {
            testing::current().set_device_config(config).await;
        }
        Ok(())
    }

    async fn setup_board(
        sys_vendor: &str,
        board_name: &str,
        product_name: &str,
    ) -> Result<testing::TestHandle> {
        let h = testing::start();

        create_dir_all(path("/sys/class/dmi/id")).await?;
        write(path(SYS_VENDOR_PATH), sys_vendor).await?;
        write(path(BOARD_NAME_PATH), board_name).await?;
        write(path(PRODUCT_NAME_PATH), product_name).await?;
        if let Some(config) = DeviceConfig::load().await? {
            h.test.set_device_config(config).await;
        }
        Ok(h)
    }

    #[tokio::test]
    async fn board_lookup_missing() {
        let _h = testing::start();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("unknown"), String::from("unknown"))
        );
    }

    #[tokio::test]
    async fn board_lookup_invalid() {
        let _h = setup_board("ASUSTeK COMPUTER INC.\n", "INVALID\n", "INVALID\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("unknown"), String::from("unknown"))
        );
    }

    #[tokio::test]
    async fn board_lookup_rog_ally() {
        let _h = setup_board("ASUSTeK COMPUTER INC.\n", "RC71L\n", "INVALID\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("rog_ally"), String::from("RC71L"))
        );
    }

    #[tokio::test]
    async fn board_lookup_rog_ally_x() {
        let _h = setup_board("ASUSTeK COMPUTER INC.\n", "RC72LA\n", "INVALID\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("rog_ally_x"), String::from("RC72LA"))
        );
    }

    #[tokio::test]
    async fn board_lookup_rog_xbox_ally() {
        let _h = setup_board("ASUSTeK COMPUTER INC.\n", "RC73YA\n", "INVALID\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("rog_xbox_ally"), String::from("RC73YA"))
        );
    }

    #[tokio::test]
    async fn board_lookup_rog_xbox_ally_x() {
        let _h = setup_board("ASUSTeK COMPUTER INC.\n", "RC73XA\n", "INVALID\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("rog_xbox_ally_x"), String::from("RC73XA"))
        );
    }

    #[tokio::test]
    async fn board_lookup_legion_go() {
        let _h = setup_board("LENOVO\n", "INVALID\n", "83E1\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("legion_go"), String::from("83E1"))
        );
    }

    #[tokio::test]
    async fn board_lookup_legion_go_s_83l3() {
        let _h = setup_board("LENOVO\n", "INVALID\n", "83L3\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("legion_go_s"), String::from("83L3"))
        );
    }

    #[tokio::test]
    async fn board_lookup_legion_go_s_83n6() {
        let _h = setup_board("LENOVO\n", "INVALID\n", "83N6\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("legion_go_s"), String::from("83N6"))
        );
    }

    #[tokio::test]
    async fn board_lookup_legion_go_s_83q2() {
        let _h = setup_board("LENOVO\n", "INVALID\n", "83Q2\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("legion_go_s"), String::from("83Q2"))
        );
    }

    #[tokio::test]
    async fn board_lookup_legion_go_s_83q3() {
        let _h = setup_board("LENOVO\n", "INVALID\n", "83Q3\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("legion_go_s"), String::from("83Q3"))
        );
    }

    #[tokio::test]
    async fn board_lookup_legion_go_2_83n0() {
        let _h = setup_board("LENOVO\n", "INVALID\n", "83N0\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("legion_go_2"), String::from("83N0"))
        );
    }

    #[tokio::test]
    async fn board_lookup_legion_go_2_83n1() {
        let _h = setup_board("LENOVO\n", "INVALID\n", "83N1\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("legion_go_2"), String::from("83N1"))
        );
    }

    #[tokio::test]
    async fn board_lookup_msi_claw_a1m() {
        let _h = setup_board(
            "Micro-Star International Co., Ltd.\n",
            "MS-1T41\n",
            "Claw A1M\n",
        )
        .await
        .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("claw"), String::from("Claw A1M"))
        );
    }

    #[tokio::test]
    async fn board_lookup_msi_claw7_a2vm() {
        let _h = setup_board(
            "Micro-Star International Co., Ltd.\n",
            "MS-1T42\n",
            "Claw 7 AI+ A2VM\n",
        )
        .await
        .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("claw"), String::from("Claw 7 AI+ A2VM"))
        );
    }

    #[tokio::test]
    async fn board_lookup_msi_claw8_a2vm() {
        let _h = setup_board(
            "Micro-Star International Co., Ltd.\n",
            "MS-1T52\n",
            "Claw 8 AI+ A2VM\n",
        )
        .await
        .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("claw"), String::from("Claw 8 AI+ A2VM"))
        );
    }

    #[tokio::test]
    async fn board_lookup_steam_deck_jupiter() {
        let _h = setup_board("Valve\n", "Jupiter\n", "Jupiter\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Jupiter
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("steam_deck"), String::from("Jupiter"))
        );
    }

    #[tokio::test]
    async fn board_lookup_steam_deck_galileo() {
        let _h = setup_board("Valve\n", "Galileo\n", "Galileo\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Galileo
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("steam_deck"), String::from("Galileo"))
        );
    }

    #[tokio::test]
    async fn board_lookup_steam_machine_fremont() {
        let _h = setup_board("Valve\n", "Fremont\n", "Fremont\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("steam_machine"), String::from("Fremont"))
        );
    }

    #[tokio::test]
    async fn board_lookup_invalid_valve() {
        let _h = setup_board("Valve\n", "Neptune\n", "Neptune\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("unknown"), String::from("unknown"))
        );
    }

    #[tokio::test]
    async fn board_lookup_zotac_gaming_zone_g0a1w() {
        let _h = setup_board("ZOTAC\n", "G0A1W\n", "INVALID\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("zotac_gaming_zone"), String::from("G0A1W"))
        );
    }

    #[tokio::test]
    async fn board_lookup_zotac_gaming_zone_g1a1w() {
        let _h = setup_board("ZOTAC\n", "G1A1W\n", "INVALID\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("zotac_gaming_zone"), String::from("G1A1W"))
        );
    }

    #[tokio::test]
    async fn board_lookup_bc250() {
        let _h = setup_board("Default string\n", "AMD BC-250\n", "AMD BC-250\n")
            .await
            .unwrap();
        assert_eq!(
            steam_deck_variant().await.unwrap(),
            SteamDeckVariant::Unknown
        );
        assert_eq!(
            device_variant().await.unwrap(),
            (String::from("bc250"), String::from("BC-250"))
        );
    }

    #[test]
    fn ec_logging_state_roundtrip() {
        enum_roundtrip!(ECLoggingState {
            0: u32 = Disabled,
            1: u32 = Enabled,
            "disabled": str = Disabled,
            "enabled": str = Enabled,
        });
        enum_on_off!(ECLoggingState => (Enabled, Disabled));
        assert!(ECLoggingState::try_from(2).is_err());
        assert!(ECLoggingState::from_str("enabld").is_err());
        assert!(ECLoggingState::from_str("ENABLED").is_ok());
        assert!(ECLoggingState::from_str("DiSaBlEd").is_ok());
    }

    #[tokio::test]
    async fn test_wildcard_detection() {
        assert!(DeviceConfig::is_wildcard_pattern("ASUS*"));
        assert!(DeviceConfig::is_wildcard_pattern("*TeK"));
        assert!(DeviceConfig::is_wildcard_pattern("ASUS?"));
        assert!(!DeviceConfig::is_wildcard_pattern("ASUSTeK COMPUTER INC."));
    }

    #[tokio::test]
    async fn test_glob_matching() {
        assert!(DeviceConfig::dmi_glob_match(
            "ASUS*",
            "ASUSTeK COMPUTER INC."
        ));
        assert!(DeviceConfig::dmi_glob_match("ASUS*", "ASUS"));
        assert!(!DeviceConfig::dmi_glob_match("ASUS*", "Valve"));
        assert!(DeviceConfig::dmi_glob_match("*", "anything"));
        assert!(DeviceConfig::dmi_glob_match("?????", "ZOTAC"));
        assert!(!DeviceConfig::dmi_glob_match("????", "ZOTAC"));
    }

    #[tokio::test]
    async fn test_exact_match_priority_over_wildcard_across_files() {
        let _h = setup_board(
            "Micro-Star International Co., Ltd.\n", 
            "MS-1T52\n", 
            "Claw 8 AI+ A2VM\n"
        ).await.unwrap();

        // This test verifies that existing MSI Claw config takes priority over any generic config
        let (device, variant) = device_variant().await.unwrap();
        
        // Should match exact config from existing msi-claw-series.toml instead of generic
        // This proves cross-config exact matching priority works
        assert_eq!(device, "claw8_a2vm");
        assert_eq!(variant, "Claw 8 AI+ A2VM");
    }

    #[tokio::test]
    async fn test_fallback_to_wildcard_when_no_exact_match() {
        let _h = setup_board(
            "Unknown Vendor\n",
            "Unknown Board\n",
            "Unknown Product\n" 
        ).await.unwrap();

        // No need to setup configs - use existing ones in data/devices/
        // This tests true fallback behavior with real config files
        
        let (device, variant) = device_variant().await.unwrap();
        
        // Should fallback to wildcard matching when no exact match exists
        // Should match generic.toml since no other config will match "Unknown Vendor"
        assert_eq!(device, "generic");
        assert_eq!(variant, "Generic");
    }

    #[test]
    fn fan_control_state_roundtrip() {
        enum_roundtrip!(FanControlState {
            0: u32 = Bios,
            1: u32 = Os,
            "BIOS": str = Bios,
            "OS": str = Os,
        });
        assert_eq!(
            FanControlState::from_str("os").unwrap(),
            FanControlState::Os
        );
        assert_eq!(
            FanControlState::from_str("bios").unwrap(),
            FanControlState::Bios
        );
        assert!(FanControlState::try_from(2).is_err());
        assert!(FanControlState::from_str("on").is_err());
    }

    #[test]
    fn inputplumber_target_devices_parse() {
        let config: InputPlumberConfig =
            toml::from_str(r#"target_devices = ["deck-uhid", "keyboard", "mouse"]"#).unwrap();
        assert_eq!(
            config.target_devices,
            [
                InputPlumberTargetDevice::DeckUhid,
                InputPlumberTargetDevice::Keyboard,
                InputPlumberTargetDevice::Mouse,
            ]
        );
        assert!(toml::from_str::<InputPlumberConfig>(r#"target_devices = ["none"]"#).is_err());

        let config: InputPlumberConfig = toml::from_str("").unwrap();
        assert!(config.target_devices.is_empty());
    }

    #[derive(Default)]
    struct MockUnit {
        active: bool,
    }

    #[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
    impl MockUnit {
        #[zbus(property)]
        fn active_state(&self) -> fdo::Result<String> {
            if self.active {
                Ok(String::from("active"))
            } else {
                Ok(String::from("inactive"))
            }
        }

        async fn start(&mut self, mode: &str) -> fdo::Result<OwnedObjectPath> {
            if JobMode::from_str(mode).is_err() {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            self.active = true;
            let path = ObjectPath::try_from("/start/0").map_err(to_zbus_fdo_error)?;
            Ok(path.into())
        }

        async fn stop(&mut self, mode: &str) -> fdo::Result<OwnedObjectPath> {
            if JobMode::from_str(mode).is_err() {
                return Err(to_zbus_fdo_error("Invalid mode"));
            }
            self.active = false;
            let path = ObjectPath::try_from("/stop/0").map_err(to_zbus_fdo_error)?;
            Ok(path.into())
        }
    }

    #[tokio::test]
    async fn test_fan_control() {
        let mut h = testing::start();
        let unit = MockUnit::default();
        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        connection
            .object_server()
            .at(
                "/org/freedesktop/systemd1/unit/jupiter_2dfan_2dcontrol_2eservice",
                unit,
            )
            .await
            .expect("at");

        sleep(Duration::from_millis(10)).await;

        let platform_config = PlatformConfig {
            fan_control: Some(ServiceConfig::Systemd(String::from(
                "jupiter-fan-control.service",
            ))),
            ..PlatformConfig::default()
        };
        h.test.set_platform_config(platform_config).await;

        let fan_control = FanControl::new(connection);
        assert_eq!(
            fan_control.get_state().await.unwrap(),
            FanControlState::Bios
        );
        assert!(fan_control.set_state(FanControlState::Os).await.is_ok());
        assert_eq!(fan_control.get_state().await.unwrap(), FanControlState::Os);
        assert!(fan_control.set_state(FanControlState::Bios).await.is_ok());
        assert_eq!(
            fan_control.get_state().await.unwrap(),
            FanControlState::Bios
        );
    }

    #[tokio::test]
    async fn test_fan_speed() {
        let mut h = testing::start();
        let connection = h.new_dbus().await.expect("dbus");

        let config = DeviceConfig {
            fan_speed: Some(FanSpeedConfig {
                hwmon: String::from("steamdeck_hwmon"),
                attribute: String::from("fan1_target"),
                download_mode_fan_speed: None,
            }),
            ..DeviceConfig::default()
        };
        h.test.set_device_config(config).await;

        let base = path(crate::power::HWMON_PREFIX).join(STEAMDECK_HWMON);
        create_dir_all(&base).await.expect("create_dir_all");
        write(base.join("name"), "steamdeck_hwmon\n")
            .await
            .expect("write");
        let fan_control = FanControl::new(connection);
        fan_control.set_speed(2000).await.expect("set_speed");
        assert_eq!(
            read_to_string(base.join("fan1_target"))
                .await
                .expect("read"),
            "2000"
        );
    }
}
