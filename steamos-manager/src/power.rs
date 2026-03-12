/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, anyhow, bail, ensure};
use async_trait::async_trait;
use nix::errno::Errno;
use num_enum::TryFromPrimitive;
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::num::NonZeroU32;
use std::ops::RangeInclusive;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use strum::{Display, EnumIter, EnumString, VariantNames};
use tokio::fs::{self, File, read_dir, read_to_string, try_exists};
use tokio::io::{AsyncWriteExt, ErrorKind, Interest};
use tokio::net::unix::pipe;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tracing::{debug, error, warn};
use zbus::names::OwnedBusName;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, ObjectServer, fdo};

use crate::error::{to_zbus_error, to_zbus_fdo_error};
use crate::gpu::AMDGPU_HWMON_NAME;
use crate::hardware::{FanControlState, device_config};
use crate::manager::MANAGER_PATH;
use crate::manager::root::RootManagerProxy;
use crate::manager::user::TdpLimit1;
use crate::proxy::TdpLimit1Proxy;
use crate::sysfs::{SysfsWritten, find_sysdir, sysfs_queued_write};
use crate::systemd::{EnableState, JobMode, SystemdUnit};
use crate::{SerialOrderValidator, Service, path, write_synced};

#[cfg(not(test))]
const HWMON_PREFIX: &str = "/sys/class/hwmon";
#[cfg(test)]
pub const HWMON_PREFIX: &str = "hwmon";

const CPU_PREFIX: &str = "/sys/devices/system/cpu";
const CPUFREQ_PREFIX: &str = "cpufreq";
const CPUFREQ_BOOST_SUFFIX: &str = "boost";
const INTEL_PSTATE_PREFIX: &str = "intel_pstate";
const INTEL_PSTATE_NO_TURBO_SUFFIX: &str = "no_turbo";

const CPU0_NAME: &str = "policy0";
const CPU_POLICY_NAME: &str = "policy";

const CPU_SCALING_GOVERNOR_SUFFIX: &str = "scaling_governor";
const CPU_SCALING_AVAILABLE_GOVERNORS_SUFFIX: &str = "scaling_available_governors";

const LAVD_PATH: &str = "/usr/bin/scx_lavd";

const PLATFORM_PROFILE_PREFIX: &str = "/sys/class/platform-profile";

const TDP_LIMIT1: &str = "power1_cap";
const TDP_LIMIT2: &str = "power2_cap";

#[cfg(not(test))]
const SB_PATHS: &[&str] = &[
    "/sys/class/power_supply",
    "/sys/bus/platform/drivers/acpi-battery/PNP0C0A:00/firmware_node/power_supply",
    "/sys/bus/acpi/drivers/battery/PNP0C0A:00/power_supply",
];
#[cfg(test)]
const SB_PATHS: &[&str] = &["power_supply", "power_supply_legacy"];
pub const BATTERY_DEFAULT_SUGGESTED_MINIMUM_LIMIT: i32 = 10;
const SB_LIMIT_PATH: &str = "charge_control_end_threshold";

#[derive(Display, EnumString, Hash, Eq, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "lowercase")]
pub enum CPUScalingGovernor {
    Conservative,
    OnDemand,
    UserSpace,
    PowerSave,
    Performance,
    SchedUtil,
}

#[derive(Display, EnumIter, EnumString, Hash, Eq, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "lowercase")]
pub enum CpuScheduler {
    None,
    LAVD,
}

pub(crate) struct CpuSchedulerManager<'dbus> {
    scx_unit: Option<SystemdUnit<'dbus>>,
    current: CpuScheduler,
}

#[derive(PartialEq, Debug, Copy, Clone)]
enum CpuBoostDriver {
    IntelPstate,
    CpuFreq,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(ascii_case_insensitive)]
#[repr(u32)]
pub enum CPUBoostState {
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

#[derive(Deserialize, Display, EnumString, VariantNames, PartialEq, Debug, Clone)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum TdpLimitingMethod {
    AmdgpuHwmon,
    FirmwareAttribute,
    RemoteInterface,
}

#[derive(Debug)]
pub(crate) struct AmdgpuHwmonTdpLimitManager {
    performance_profile: Option<String>,
}

#[derive(Debug)]
pub(crate) struct FirmwareAttributeLimitManager {
    attribute: String,
    performance_profile: Option<String>,
}

#[derive(Debug)]
pub(crate) struct RemoteInterfaceLimitManager<'proxy> {
    connection: Connection,
    proxy: Option<TdpLimit1Proxy<'proxy>>,
}

#[async_trait]
pub(crate) trait TdpLimitManager: Send + Sync {
    async fn get_tdp_limit(&self) -> Result<u32>;
    async fn set_tdp_limit(&self, limit: u32) -> Result<()>;
    async fn get_tdp_limit_range(&self) -> Result<RangeInclusive<u32>>;

    async fn is_active(&self) -> Result<bool> {
        Ok(true)
    }

    fn needs_root(&self) -> bool {
        true
    }

    async fn set_proxy(&mut self, _proxy: Option<(OwnedBusName, OwnedObjectPath)>) -> Result<()> {
        Ok(())
    }
}

pub(crate) async fn tdp_limit_manager(system: &Connection) -> Result<Box<dyn TdpLimitManager>> {
    let config = device_config().await?;
    if let Some(config) = config.as_ref().and_then(|config| config.tdp_limit.as_ref()) {
        Ok(match &config.method {
            TdpLimitingMethod::FirmwareAttribute => {
                let Some(ref firmware_attribute) = config.firmware_attribute else {
                    bail!("Firmware attribute TDP limiting method not configured");
                };
                Box::new(FirmwareAttributeLimitManager {
                    attribute: firmware_attribute.attribute.clone(),
                    performance_profile: firmware_attribute.performance_profile.clone(),
                })
            }
            TdpLimitingMethod::AmdgpuHwmon => Box::new(AmdgpuHwmonTdpLimitManager {
                performance_profile: config.performance_profile.clone(),
            }),
            TdpLimitingMethod::RemoteInterface => Box::new(RemoteInterfaceLimitManager {
                connection: system.clone(),
                proxy: None,
            }),
        })
    } else {
        Ok(Box::new(RemoteInterfaceLimitManager {
            connection: system.clone(),
            proxy: None,
        }))
    }
}

pub(crate) struct TdpManagerService {
    proxy: RootManagerProxy<'static>,
    session: Connection,
    channel: UnboundedReceiver<TdpManagerCommand>,
    download_set: JoinSet<String>,
    download_handles: HashMap<String, u32>,
    download_mode_limit: Option<NonZeroU32>,
    download_mode_fan_speed: Option<NonZeroU32>,
    previous_limit: Option<NonZeroU32>,
    manager: Box<dyn TdpLimitManager>,
    restart_fan_control_service: bool,
}

#[derive(Debug)]
pub(crate) enum TdpManagerCommand {
    SetTdpLimit(u32),
    GetTdpLimit(oneshot::Sender<Result<u32>>),
    GetTdpLimitRange(oneshot::Sender<Result<RangeInclusive<u32>>>),
    IsActive(oneshot::Sender<Result<bool>>),
    UpdateDownloadMode,
    EnterDownloadMode(String, oneshot::Sender<Result<Option<OwnedFd>>>),
    ListDownloadModeHandles(oneshot::Sender<HashMap<String, u32>>),
    SetProxy(
        Option<(OwnedBusName, OwnedObjectPath)>,
        oneshot::Sender<Result<()>>,
    ),
}

#[derive(Deserialize, Display, EnumString, VariantNames, PartialEq, Debug, Clone, Default)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub(crate) enum BatteryChargeLimitMethod {
    #[default]
    AcpiSb,
    HwmonAttribute {
        hwmon: String,
        attribute: String,
    },
}

async fn read_cpu_sysfs_contents<S: AsRef<Path>>(suffix: S) -> Result<String> {
    let base = path(CPU_PREFIX).join(CPUFREQ_PREFIX).join(CPU0_NAME);
    fs::read_to_string(base.join(suffix.as_ref()))
        .await
        .map_err(|message| anyhow!("Error opening sysfs file for reading {message}"))
}

async fn write_cpu_governor_sysfs_contents(contents: String) -> Result<()> {
    // Iterate over all policyX paths
    let mut dir = read_dir(path(CPU_PREFIX).join(CPUFREQ_PREFIX)).await?;
    let mut wrote_stuff = false;
    loop {
        let Some(entry) = dir.next_entry().await? else {
            ensure!(
                wrote_stuff,
                "No data written, unable to find any policyX sysfs paths"
            );
            return Ok(());
        };
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("Unable to convert path to string"))?;
        if !file_name.starts_with(CPU_POLICY_NAME) {
            continue;
        }
        let base = entry.path();
        // Write contents to each one
        wrote_stuff = true;
        write_synced(base.join(CPU_SCALING_GOVERNOR_SUFFIX), contents.as_bytes())
            .await
            .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
    }
}

pub(crate) async fn get_available_cpu_scaling_governors() -> Result<Vec<CPUScalingGovernor>> {
    let contents = read_cpu_sysfs_contents(CPU_SCALING_AVAILABLE_GOVERNORS_SUFFIX).await?;
    // Get the list of supported governors from cpu0
    let mut result = Vec::new();

    let words = contents.split_whitespace();
    for word in words {
        match CPUScalingGovernor::from_str(word) {
            Ok(governor) => result.push(governor),
            Err(message) => warn!("Error parsing governor {message}"),
        }
    }

    Ok(result)
}

pub(crate) async fn get_cpu_scaling_governor() -> Result<CPUScalingGovernor> {
    // get the current governor from cpu0 (assume all others are the same)
    let contents = read_cpu_sysfs_contents(CPU_SCALING_GOVERNOR_SUFFIX).await?;

    let contents = contents.trim();
    CPUScalingGovernor::from_str(contents).map_err(|message| {
        anyhow!(
            "Error converting CPU scaling governor sysfs file contents to enumeration: {message}"
        )
    })
}

pub(crate) async fn set_cpu_scaling_governor(governor: CPUScalingGovernor) -> Result<()> {
    // Set the given governor on all cpus
    let name = governor.to_string();
    write_cpu_governor_sysfs_contents(name).await
}

impl<'dbus> CpuSchedulerManager<'dbus> {
    pub async fn new(connection: &Connection) -> Result<CpuSchedulerManager<'dbus>> {
        // Try to create a SystemdUnit for scx.service; if systemd isn't available in the
        // test DBus environment, treat the service as not installed instead of failing.
        let scx_unit = match SystemdUnit::new(connection, "scx.service").await {
            Ok(u) => Some(u),
            Err(e) => {
                warn!("Could not create SystemdUnit for scx.service: {e}");
                None
            }
        };

        let current = if let Some(ref u) = scx_unit {
            match u.enabled().await {
                Ok(EnableState::Enabled) => CpuScheduler::LAVD,
                _ => CpuScheduler::None,
            }
        } else {
            CpuScheduler::None
        };

        Ok(CpuSchedulerManager { scx_unit, current })
    }

    pub async fn is_supported() -> Result<bool> {
        if try_exists(path(LAVD_PATH)).await? {
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) async fn get_available_cpu_schedulers(&self) -> Result<Vec<CpuScheduler>> {
        let mut list = vec![CpuScheduler::None];
        if self.scx_unit.is_some() {
            list.push(CpuScheduler::LAVD);
        }
        Ok(list)
    }

    pub(crate) async fn get_cpu_scheduler(&self) -> Result<CpuScheduler> {
        Ok(self.current)
    }

    pub(crate) async fn set_cpu_scheduler(&mut self, scheduler: CpuScheduler) -> Result<()> {
        if self.current == scheduler {
            return Ok(());
        }

        match scheduler {
            CpuScheduler::None => {
                // Stop the scx service if it's installed
                if let Some(unit) = &self.scx_unit {
                    unit.stop(JobMode::Fail).await?;
                }
                self.current = CpuScheduler::None;
            }
            CpuScheduler::LAVD => {
                // Start the scx service if it's installed
                if let Some(unit) = &self.scx_unit {
                    unit.start(JobMode::Fail).await?;
                    self.current = CpuScheduler::LAVD;
                } else {
                    // service not present; remain at None
                    bail!("Cannot set CPU scheduler to LAVD; scx.service not installed");
                }
            }
        }
        Ok(())
    }
}

async fn find_cpu_boost_driver() -> Result<(PathBuf, CpuBoostDriver)> {
    // Try cpufreq path first
    let cpufreq_path = path(CPU_PREFIX)
        .join(CPUFREQ_PREFIX)
        .join(CPUFREQ_BOOST_SUFFIX);
    if try_exists(&cpufreq_path).await? {
        return Ok((cpufreq_path, CpuBoostDriver::CpuFreq));
    }

    // Try intel_pstate path next
    let intel_pstate_path = path(CPU_PREFIX)
        .join(INTEL_PSTATE_PREFIX)
        .join(INTEL_PSTATE_NO_TURBO_SUFFIX);
    if try_exists(&intel_pstate_path).await? {
        return Ok((intel_pstate_path, CpuBoostDriver::IntelPstate));
    }

    bail!("Could not find CPU boost sysfs path");
}

pub(crate) async fn get_cpu_boost_state() -> Result<CPUBoostState> {
    let (path, driver) = find_cpu_boost_driver().await?;
    let contents = fs::read_to_string(&path)
        .await
        .map_err(|message| anyhow!("Error opening CPU boost sysfs file for reading: {message}"))?;
    match driver {
        CpuBoostDriver::CpuFreq => match contents.trim() {
            // cpufreq's boost property is standard
            // 1 means boost is enabled, 0 means boost is disabled
            "1" => Ok(CPUBoostState::Enabled),
            "0" => Ok(CPUBoostState::Disabled),
            _ => Err(anyhow!("Invalid cpufreq boost state: {contents}")),
        },
        CpuBoostDriver::IntelPstate => match contents.trim() {
            // intel_pstate's no_turbo property is inverted
            // 0 means boost is enabled, 1 means boost is disabled
            "0" => Ok(CPUBoostState::Enabled),
            "1" => Ok(CPUBoostState::Disabled),
            _ => Err(anyhow!("Invalid intel_pstate boost state: {contents}")),
        },
    }
}

pub(crate) async fn set_cpu_boost_state(state: CPUBoostState) -> Result<()> {
    let (path, driver) = find_cpu_boost_driver().await?;
    let contents = match (driver, state) {
        (CpuBoostDriver::CpuFreq, CPUBoostState::Enabled) => "1",
        (CpuBoostDriver::CpuFreq, CPUBoostState::Disabled) => "0",
        (CpuBoostDriver::IntelPstate, CPUBoostState::Enabled) => "0",
        (CpuBoostDriver::IntelPstate, CPUBoostState::Disabled) => "1",
    };
    Ok(write_synced(path, contents.as_bytes())
        .await
        .inspect_err(|message| error!("Error writing to CPU boost sysfs file: {message}"))?)
}

pub(crate) async fn find_hwmon(hwmon: &str) -> Result<PathBuf> {
    find_sysdir(path(HWMON_PREFIX), hwmon).await
}

async fn find_platform_profile(name: &str) -> Result<PathBuf> {
    find_sysdir(path(PLATFORM_PROFILE_PREFIX), name).await
}

#[async_trait]
impl TdpLimitManager for AmdgpuHwmonTdpLimitManager {
    async fn get_tdp_limit(&self) -> Result<u32> {
        ensure!(self.is_active().await?, "TDP limiting not active");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        let power1cap = fs::read_to_string(base.join(TDP_LIMIT1)).await?;
        let power1cap: u32 = power1cap.trim_end().parse()?;
        Ok(power1cap / 1_000_000)
    }

    async fn set_tdp_limit(&self, limit: u32) -> Result<()> {
        ensure!(self.is_active().await?, "TDP limiting not active");
        ensure!(
            self.get_tdp_limit_range().await?.contains(&limit),
            "Invalid limit"
        );

        let data = format!("{limit}000000");

        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        write_synced(base.join(TDP_LIMIT1), data.as_bytes())
            .await
            .inspect_err(|message| {
                error!("Error opening sysfs power1_cap file for writing TDP limits {message}");
            })?;

        if let Ok(mut power2file) = File::create(base.join(TDP_LIMIT2)).await {
            power2file
                .write(data.as_bytes())
                .await
                .inspect_err(|message| error!("Error writing to power2_cap file: {message}"))?;
            power2file.flush().await?;
        }
        Ok(())
    }

    async fn get_tdp_limit_range(&self) -> Result<RangeInclusive<u32>> {
        let config = device_config().await?;
        let config = config
            .as_ref()
            .and_then(|config| config.tdp_limit.as_ref())
            .ok_or(anyhow!("No TDP limit configured"))?;

        if let Some(range) = config.range {
            return Ok(range.min..=range.max);
        }
        bail!("No TDP limit range configured");
    }

    async fn is_active(&self) -> Result<bool> {
        let Some(ref performance_profile) = self.performance_profile else {
            return Ok(true);
        };
        let config = device_config().await?;
        if let Some(config) = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
        {
            Ok(get_platform_profile(&config.platform_profile_name).await? == *performance_profile)
        } else {
            Ok(true)
        }
    }
}

impl FirmwareAttributeLimitManager {
    const PREFIX: &str = "/sys/class/firmware-attributes";
    const SPL_SUFFIX: &str = "ppt_pl1_spl";
    const SPPT_SUFFIX: &str = "ppt_pl2_sppt";
    const FPPT_SUFFIX: &str = "ppt_pl3_fppt";
}

#[async_trait]
impl TdpLimitManager for FirmwareAttributeLimitManager {
    async fn get_tdp_limit(&self) -> Result<u32> {
        ensure!(self.is_active().await?, "TDP limiting not active");
        let base = path(Self::PREFIX).join(&self.attribute).join("attributes");

        fs::read_to_string(base.join(Self::SPL_SUFFIX).join("current_value"))
            .await
            .map_err(|message| anyhow!("Error reading sysfs: {message}"))?
            .trim()
            .parse()
            .map_err(|e| anyhow!("Error parsing value: {e}"))
    }

    async fn set_tdp_limit(&self, limit: u32) -> Result<()> {
        ensure!(self.is_active().await?, "TDP limiting not active");
        ensure!(
            self.get_tdp_limit_range().await?.contains(&limit),
            "Invalid limit"
        );

        let base = path(Self::PREFIX).join(&self.attribute).join("attributes");

        let sppt_min = fs::read_to_string(base.join(Self::SPPT_SUFFIX).join("min_value"))
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let fppt_min = fs::read_to_string(base.join(Self::FPPT_SUFFIX).join("min_value"))
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);

        let spl_value = limit;
        let sppt_value = limit.max(sppt_min);
        let fppt_value = limit.max(fppt_min);

        write_synced(
            base.join(Self::SPL_SUFFIX).join("current_value"),
            spl_value.to_string().as_bytes(),
        )
        .await
        .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
        write_synced(
            base.join(Self::SPPT_SUFFIX).join("current_value"),
            sppt_value.to_string().as_bytes(),
        )
        .await
        .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
        write_synced(
            base.join(Self::FPPT_SUFFIX).join("current_value"),
            fppt_value.to_string().as_bytes(),
        )
        .await
        .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
        Ok(())
    }

    async fn get_tdp_limit_range(&self) -> Result<RangeInclusive<u32>> {
        let base = path(Self::PREFIX)
            .join(&self.attribute)
            .join("attributes")
            .join(Self::SPL_SUFFIX);

        let min: u32 = fs::read_to_string(base.join("min_value"))
            .await
            .map_err(|message| anyhow!("Error reading sysfs: {message}"))?
            .trim()
            .parse()
            .map_err(|e| anyhow!("Error parsing value: {e}"))?;
        let max: u32 = fs::read_to_string(base.join("max_value"))
            .await
            .map_err(|message| anyhow!("Error reading sysfs: {message}"))?
            .trim()
            .parse()
            .map_err(|e| anyhow!("Error parsing value: {e}"))?;
        Ok(min..=max)
    }

    async fn is_active(&self) -> Result<bool> {
        let Some(ref performance_profile) = self.performance_profile else {
            return Ok(true);
        };
        let config = device_config().await?;
        if let Some(config) = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
        {
            Ok(get_platform_profile(&config.platform_profile_name).await? == *performance_profile)
        } else {
            Ok(true)
        }
    }
}

#[async_trait]
impl<'proxy> TdpLimitManager for RemoteInterfaceLimitManager<'proxy> {
    async fn get_tdp_limit(&self) -> Result<u32> {
        let proxy = self
            .proxy
            .as_ref()
            .ok_or(anyhow!("No remote TDP manager"))?;
        Ok(proxy.tdp_limit().await?)
    }

    async fn set_tdp_limit(&self, limit: u32) -> Result<()> {
        let proxy = self
            .proxy
            .as_ref()
            .ok_or(anyhow!("No remote TDP manager"))?;
        Ok(proxy.set_tdp_limit(limit).await?)
    }

    async fn get_tdp_limit_range(&self) -> Result<RangeInclusive<u32>> {
        let proxy = self
            .proxy
            .as_ref()
            .ok_or(anyhow!("No remote TDP manager"))?;
        let min = proxy.tdp_limit_min().await?;
        let max = proxy.tdp_limit_max().await?;
        Ok(min..=max)
    }

    async fn is_active(&self) -> Result<bool> {
        Ok(self.proxy.is_some())
    }

    fn needs_root(&self) -> bool {
        false
    }

    async fn set_proxy(&mut self, proxy: Option<(OwnedBusName, OwnedObjectPath)>) -> Result<()> {
        self.proxy = if let Some((destination, path)) = proxy {
            Some(
                TdpLimit1Proxy::builder(&self.connection)
                    .path(path)?
                    .destination(destination)?
                    .build()
                    .await?,
            )
        } else {
            None
        };
        Ok(())
    }
}

async fn find_battery_charge_path() -> Result<PathBuf> {
    let config = device_config().await?;
    let config = config
        .as_ref()
        .and_then(|config| config.battery_charge_limit.as_ref())
        .ok_or(anyhow!("No battery charge limit configured"))?;

    match &config.method {
        BatteryChargeLimitMethod::AcpiSb => {
            for base in SB_PATHS {
                let mut dir = match read_dir(path(base)).await {
                    Ok(dir) => dir,
                    Err(e) if e.kind() == ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                while let Some(entry) = dir.next_entry().await? {
                    let path = entry.path();
                    let path = match read_to_string(path.join("type")).await {
                        Ok(s) if s.trim() == "Battery" => path.join(SB_LIMIT_PATH),
                        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                        _ => continue,
                    };
                    if try_exists(&path).await? {
                        return Ok(path);
                    }
                }
            }
        }
        BatteryChargeLimitMethod::HwmonAttribute { hwmon, attribute } => {
            let base = find_hwmon(hwmon.as_str()).await?;
            let path = base.join(attribute);
            if try_exists(&path).await? {
                return Ok(path);
            }
        }
    }
    bail!("Battery not found");
}

fn parse_max_charge_level(result: std::io::Result<String>) -> Result<i32> {
    match result {
        Ok(s) => s
            .trim()
            .parse()
            .map_err(|e| anyhow!("Error parsing value: {e}")),
        // asus-wmi may return ENODATA until userspace writes a value
        Err(e) if e.raw_os_error().map(Errno::from_raw) == Some(Errno::ENODATA) => Ok(-1),
        Err(e) => Err(anyhow!("Error reading sysfs: {e}")),
    }
}

pub(crate) async fn get_max_charge_level() -> Result<i32> {
    let path = find_battery_charge_path().await?;
    parse_max_charge_level(read_to_string(path).await)
}

pub(crate) async fn set_max_charge_level(limit: i32) -> Result<oneshot::Receiver<SysfsWritten>> {
    ensure!((0..=100).contains(&limit), "Invalid limit");
    let data = limit.to_string();
    let path = find_battery_charge_path().await?;
    sysfs_queued_write(path, data.as_bytes().to_owned()).await
}

pub(crate) async fn get_available_platform_profiles(name: &str) -> Result<Vec<String>> {
    let base = find_platform_profile(name).await?;
    Ok(fs::read_to_string(base.join("choices"))
        .await
        .map_err(|message| anyhow!("Error reading sysfs: {message}"))?
        .trim()
        .split(' ')
        .map(ToString::to_string)
        .collect())
}

pub(crate) async fn get_platform_profile(name: &str) -> Result<String> {
    let base = find_platform_profile(name).await?;
    Ok(fs::read_to_string(base.join("profile"))
        .await
        .map_err(|message| anyhow!("Error reading sysfs: {message}"))?
        .trim()
        .to_string())
}

pub(crate) async fn set_platform_profile(name: &str, profile: &str) -> Result<()> {
    let base = find_platform_profile(name).await?;
    fs::write(base.join("profile"), profile.as_bytes())
        .await
        .map_err(|message| anyhow!("Error writing to sysfs: {message}"))
}

pub(crate) async fn register_tdp_limit1(
    ctx: &mut Option<UnboundedSender<TdpManagerCommand>>,
    proxy: TdpLimit1Proxy<'_>,
    object_server: &ObjectServer,
) -> fdo::Result<()> {
    let Some(sender) = ctx else {
        return Ok(());
    };

    let proxy = proxy.inner();
    let destination = proxy.destination().clone().into();
    let path = proxy.path().clone().into();
    let (tx, rx) = oneshot::channel();
    sender
        .send(TdpManagerCommand::SetProxy(Some((destination, path)), tx))
        .map_err(|_| fdo::Error::Failed(String::from("TDP manager exited prematurely")))?;
    rx.await
        .map_err(to_zbus_fdo_error)?
        .map_err(to_zbus_fdo_error)?;

    let tdp_limit = TdpLimit1 {
        manager: sender.clone(),
        order: SerialOrderValidator::default(),
    };
    object_server.at(MANAGER_PATH, tdp_limit).await?;
    Ok(())
}

pub(crate) async fn unregister_tdp_limit1(
    ctx: Option<&mut Option<UnboundedSender<TdpManagerCommand>>>,
    object_server: &ObjectServer,
) -> zbus::Result<()> {
    object_server.remove::<TdpLimit1, _>(MANAGER_PATH).await?;

    let Some(Some(sender)) = ctx else {
        return Ok(());
    };

    let (tx, rx) = oneshot::channel();
    sender
        .send(TdpManagerCommand::SetProxy(None, tx))
        .map_err(|_| zbus::Error::Failure(String::from("TDP manager exited prematurely")))?;
    rx.await.map_err(to_zbus_error)?.map_err(to_zbus_error)
}

impl TdpManagerService {
    pub async fn new(
        channel: UnboundedReceiver<TdpManagerCommand>,
        system: &Connection,
        session: &Connection,
    ) -> Result<TdpManagerService> {
        let config = device_config().await?;
        let download_mode_limit = config
            .as_ref()
            .and_then(|config| config.tdp_limit.as_ref())
            .and_then(|config| config.download_mode_limit);
        let download_mode_fan_speed = config
            .as_ref()
            .and_then(|config| config.fan_speed.as_ref())
            .and_then(|config| config.download_mode_fan_speed);

        let manager = tdp_limit_manager(system).await?;
        let proxy = RootManagerProxy::new(system).await?;

        Ok(TdpManagerService {
            proxy,
            session: session.clone(),
            channel,
            download_set: JoinSet::new(),
            download_handles: HashMap::new(),
            previous_limit: None,
            download_mode_limit,
            download_mode_fan_speed,
            manager,
            restart_fan_control_service: false,
        })
    }

    async fn update_download_mode(&mut self) -> Result<()> {
        if !self.manager.is_active().await? {
            return Ok(());
        }

        let Some(download_mode_limit) = self.download_mode_limit else {
            return Ok(());
        };

        let Some(current_limit) = NonZeroU32::new(self.manager.get_tdp_limit().await?) else {
            // If current_limit is 0 then the interface is broken, likely because TDP limiting
            // isn't possible with the current power profile or system, so we should just ignore
            // it for now.
            return Ok(());
        };

        if self.download_handles.is_empty() {
            if let Some(previous_limit) = self.previous_limit {
                debug!("Leaving download mode, setting TDP to {previous_limit}");
                self.set_tdp_limit(previous_limit.get()).await?;
                self.previous_limit = None;
            }
            if self.restart_fan_control_service {
                debug!("Leaving download mode, restarting fan control service");
                self.proxy
                    .set_fan_control_state(FanControlState::Os as u32)
                    .await
                    .inspect_err(|e| warn!("Failed to restart fan control service: {e}"))
                    .ok();
                self.restart_fan_control_service = false;
            }
        } else {
            if self.previous_limit.is_none() {
                debug!("Entering download mode, caching TDP limit of {current_limit}");
                self.previous_limit = Some(current_limit);
            }
            if current_limit != download_mode_limit {
                self.set_tdp_limit(download_mode_limit.get()).await?;
            }
            if let Some(fan_rpm) = self.download_mode_fan_speed {
                // Stop fan control service if running before setting a fixed fan speed.
                let state = self
                    .proxy
                    .fan_control_state()
                    .await
                    .inspect_err(|e| warn!("Failed to get fan control state: {e}"))?;

                if state == FanControlState::Os as u32 {
                    debug!("Entering download mode, stopping fan control service");
                    self.proxy
                        .set_fan_control_state(FanControlState::Bios as u32)
                        .await
                        .inspect_err(|e| warn!("Failed to stop fan control service: {e}"))?;
                    self.restart_fan_control_service = true;
                }
                debug!("Setting fan speed to {} RPM", fan_rpm.get());
                self.proxy
                    .set_fan_speed(fan_rpm.get())
                    .await
                    .inspect_err(|e| warn!("Failed to set fan speed: {e}"))?;
            }
        }

        Ok(())
    }

    async fn get_download_mode_handle(
        &mut self,
        identifier: impl AsRef<str>,
    ) -> Result<Option<OwnedFd>> {
        if self.download_mode_limit.is_none() {
            return Ok(None);
        }
        let (send, recv) = pipe::pipe()?;
        let identifier = identifier.as_ref().to_string();
        self.download_handles
            .entry(identifier.clone())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        self.download_set
            .spawn(TdpManagerService::wait_on_handle(recv, identifier));
        self.update_download_mode().await?;
        Ok(Some(send.into_blocking_fd()?))
    }

    async fn wait_on_handle(recv: pipe::Receiver, identifier: String) -> String {
        loop {
            let mut buf = [0; 1024];
            let read = match recv.ready(Interest::READABLE).await {
                Ok(r) if r.is_read_closed() => break,
                Ok(r) if r.is_readable() => recv.try_read(&mut buf),
                Err(e) => Err(e),
                Ok(e) => {
                    warn!("Download mode handle received unexpected event: {e:?}");
                    break;
                }
            };
            if let Err(e) = read {
                warn!("Download mode handle received unexpected error: {e:?}");
                break;
            }
        }
        identifier
    }

    async fn set_tdp_limit(&self, limit: u32) -> Result<()> {
        if self.manager.needs_root() {
            self.proxy
                .set_tdp_limit(limit)
                .await
                .inspect_err(|e| error!("Failed to set TDP limit: {e}"))?;
        } else {
            self.manager.set_tdp_limit(limit).await?;
        }

        let object_server = self.session.object_server().clone();
        tokio::spawn(async move {
            if let Ok(interface) = object_server.interface::<_, TdpLimit1>(MANAGER_PATH).await {
                let ctx = interface.signal_emitter();
                let _ = interface.get().await.tdp_limit_changed(ctx).await;
            }
        });
        Ok(())
    }

    async fn handle_command(&mut self, command: TdpManagerCommand) -> Result<()> {
        match command {
            TdpManagerCommand::SetTdpLimit(limit) => {
                if self.download_handles.is_empty() {
                    self.set_tdp_limit(limit).await?;
                }
            }
            TdpManagerCommand::GetTdpLimit(reply) => {
                let _ = reply.send(self.manager.get_tdp_limit().await);
            }
            TdpManagerCommand::GetTdpLimitRange(reply) => {
                let _ = reply.send(self.manager.get_tdp_limit_range().await);
            }
            TdpManagerCommand::IsActive(reply) => {
                let _ = reply.send(self.manager.is_active().await);
            }
            TdpManagerCommand::UpdateDownloadMode => {
                self.update_download_mode().await?;
            }
            TdpManagerCommand::EnterDownloadMode(identifier, reply) => {
                let fd = self.get_download_mode_handle(identifier).await;
                let _ = reply.send(fd);
            }
            TdpManagerCommand::ListDownloadModeHandles(reply) => {
                let _ = reply.send(self.download_handles.clone());
            }
            TdpManagerCommand::SetProxy(proxy, reply) => {
                let _ = reply.send(self.manager.set_proxy(proxy).await);
            }
        }
        Ok(())
    }
}

impl Service for TdpManagerService {
    const NAME: &'static str = "tdp-manager";

    async fn run(&mut self) -> Result<()> {
        loop {
            if self.download_set.is_empty() {
                let message = match self.channel.recv().await {
                    None => bail!("TDP manager service channel broke"),
                    Some(message) => message,
                };
                let _ = self
                    .handle_command(message)
                    .await
                    .inspect_err(|e| error!("Failed to handle command: {e}"));
            } else {
                tokio::select! {
                    message = self.channel.recv() => {
                        let message = match message {
                            None => bail!("TDP manager service channel broke"),
                            Some(message) => message,
                        };
                        let _ = self.handle_command(message)
                            .await
                            .inspect_err(|e| error!("Failed to handle command: {e}"));
                    },
                    identifier = self.download_set.join_next() => {
                        match identifier {
                            None => (),
                            Some(Ok(identifier)) => {
                                match self.download_handles.entry(identifier) {
                                    Entry::Occupied(e) if e.get() == &1 => {
                                        e.remove();
                                        if self.download_handles.is_empty()
                                            && let Err(e) = self.update_download_mode().await
                                        {
                                            error!("Failed to update download mode: {e}");
                                        }
                                    },
                                    Entry::Occupied(mut e) => *e.get_mut() -= 1,
                                    Entry::Vacant(_) => (),
                                }
                            }
                            Some(Err(e)) => warn!("Failed to get closed download mode handle: {e}"),
                        }
                    },
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::hardware::{
        BatteryChargeLimitConfig, DeviceConfig, FanSpeedConfig, FirmwareAttributeConfig,
        PerformanceProfileConfig, RangeConfig, TdpLimitConfig,
    };
    use crate::{enum_on_off, enum_roundtrip, testing};
    use anyhow::anyhow;
    use std::time::Duration;
    use tokio::fs::{create_dir_all, read_to_string, remove_dir, write};
    use tokio::sync::mpsc::{Sender, channel, unbounded_channel};
    use tokio::time::sleep;
    use zbus::{fdo, interface};

    #[derive(Debug, Default, Clone)]
    pub struct Nodes {
        cpu_boost: bool,
        cpu_scheduler: bool,
        tdp_limit: bool,
    }

    impl Nodes {
        pub fn all() -> Nodes {
            Nodes {
                cpu_boost: true,
                cpu_scheduler: true,
                tdp_limit: true,
            }
        }

        pub fn none() -> Nodes {
            Nodes::default()
        }
    }

    async fn setup() -> Result<()> {
        // Use hwmon5 just as a test. We needed a subfolder of HWMON_PREFIX
        // and this is as good as any.
        let base = path(HWMON_PREFIX).join("hwmon5");
        let filename = base.join("device");
        // Creates hwmon path, including device subpath
        create_dir_all(filename).await?;
        // Writes name file as addgpu so find_hwmon() will find it.
        write_synced(base.join("name"), AMDGPU_HWMON_NAME.as_bytes()).await?;
        Ok(())
    }

    pub async fn create_nodes(which: &Nodes) -> Result<()> {
        setup().await?;
        if which.cpu_boost {
            let base = path(CPU_PREFIX);
            let cpufreq_base = base.join(CPUFREQ_PREFIX);
            create_dir_all(&cpufreq_base).await?;
            write(cpufreq_base.join(CPUFREQ_BOOST_SUFFIX), b"1\n").await?;
        }

        if which.cpu_scheduler {
            let lavd_path = path(LAVD_PATH);
            create_dir_all(lavd_path.parent().unwrap()).await?;
            write(lavd_path, b"").await?;
        }

        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;

        if which.tdp_limit {
            let filename = base.join(TDP_LIMIT1);
            write(filename.as_path(), "15000000\n").await?;
        }

        let base = path(HWMON_PREFIX).join("hwmon6");
        create_dir_all(&base).await?;

        write(base.join("name"), "steamdeck_hwmon\n").await?;

        write(base.join("max_battery_charge_level"), "10\n").await?;

        let base = path(PLATFORM_PROFILE_PREFIX).join("platform-profile0");
        create_dir_all(&base).await?;
        write_synced(base.join("name"), b"power-driver\n").await?;
        write_synced(base.join("choices"), b"a b c\n").await?;

        Ok(())
    }

    #[test]
    fn cpu_governor_roundtrip() {
        enum_roundtrip!(CPUScalingGovernor {
            "conservative": str = Conservative,
            "ondemand": str = OnDemand,
            "userspace": str = UserSpace,
            "powersave": str = PowerSave,
            "performance": str = Performance,
            "schedutil": str = SchedUtil,
        });
        assert!(CPUScalingGovernor::from_str("usersave").is_err());
    }

    #[tokio::test]
    async fn test_gpu_hwmon_get_tdp_limit() {
        let mut handle = testing::start();
        let connection = handle.new_dbus().await.expect("new_dbus");

        let config = DeviceConfig {
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::AmdgpuHwmon,
                range: Some(RangeConfig { min: 3, max: 15 }),
                download_mode_limit: None,
                firmware_attribute: None,
                performance_profile: None,
            }),
            ..DeviceConfig::default()
        };
        handle.test.set_device_config(config).await;
        let manager = tdp_limit_manager(&connection).await.unwrap();

        setup().await.expect("setup");
        let hwmon = path(HWMON_PREFIX);

        assert!(manager.get_tdp_limit().await.is_err());

        write(hwmon.join("hwmon5").join(TDP_LIMIT1), "15000000\n")
            .await
            .expect("write");
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 15);
    }

    #[tokio::test]
    async fn test_gpu_hwmon_set_tdp_limit() {
        let mut handle = testing::start();
        let connection = handle.new_dbus().await.expect("new_dbus");

        let config = DeviceConfig {
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::AmdgpuHwmon,
                range: Some(RangeConfig { min: 3, max: 15 }),
                download_mode_limit: None,
                firmware_attribute: None,
                performance_profile: None,
            }),
            ..DeviceConfig::default()
        };
        handle.test.set_device_config(config).await;
        let manager = tdp_limit_manager(&connection).await.unwrap();

        assert_eq!(
            manager.set_tdp_limit(2).await.unwrap_err().to_string(),
            anyhow!("Invalid limit").to_string()
        );
        assert_eq!(
            manager.set_tdp_limit(20).await.unwrap_err().to_string(),
            anyhow!("Invalid limit").to_string()
        );
        assert!(manager.set_tdp_limit(10).await.is_err());

        let hwmon = path(HWMON_PREFIX);
        assert_eq!(
            manager.set_tdp_limit(10).await.unwrap_err().to_string(),
            anyhow!("No such file or directory (os error 2)").to_string()
        );

        setup().await.expect("setup");
        let hwmon = hwmon.join("hwmon5");
        create_dir_all(hwmon.join(TDP_LIMIT1))
            .await
            .expect("create_dir_all");
        create_dir_all(hwmon.join(TDP_LIMIT2))
            .await
            .expect("create_dir_all");
        assert_eq!(
            manager.set_tdp_limit(10).await.unwrap_err().to_string(),
            anyhow!("Is a directory (os error 21)").to_string()
        );

        remove_dir(hwmon.join(TDP_LIMIT1))
            .await
            .expect("remove_dir");
        write(hwmon.join(TDP_LIMIT1), "0").await.expect("write");
        assert!(manager.set_tdp_limit(10).await.is_ok());
        let power1_cap = read_to_string(hwmon.join(TDP_LIMIT1))
            .await
            .expect("power1_cap");
        assert_eq!(power1_cap, "10000000");

        remove_dir(hwmon.join(TDP_LIMIT2))
            .await
            .expect("remove_dir");
        write(hwmon.join(TDP_LIMIT2), "0").await.expect("write");
        assert!(manager.set_tdp_limit(15).await.is_ok());
        let power1_cap = read_to_string(hwmon.join(TDP_LIMIT1))
            .await
            .expect("power1_cap");
        assert_eq!(power1_cap, "15000000");
        let power2_cap = read_to_string(hwmon.join(TDP_LIMIT2))
            .await
            .expect("power2_cap");
        assert_eq!(power2_cap, "15000000");
    }

    #[test]
    fn cpu_boost_state_roundtrip() {
        enum_roundtrip!(CPUBoostState {
            0: u32 = Disabled,
            1: u32 = Enabled,
            "disabled": str = Disabled,
            "enabled": str = Enabled,
        });
        enum_on_off!(CPUBoostState => (Enabled, Disabled));
        assert!(CPUBoostState::try_from(2).is_err());
        assert!(CPUBoostState::from_str("enabld").is_err());
    }

    #[tokio::test]
    async fn read_cpu_available_governors() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPUFREQ_PREFIX).join(CPU0_NAME);
        create_dir_all(&base).await.expect("create_dir_all");

        let contents = "conservative ondemand userspace powersave performance schedutil";
        write(base.join(CPU_SCALING_AVAILABLE_GOVERNORS_SUFFIX), contents)
            .await
            .expect("write");

        assert_eq!(
            get_available_cpu_scaling_governors().await.unwrap(),
            vec![
                CPUScalingGovernor::Conservative,
                CPUScalingGovernor::OnDemand,
                CPUScalingGovernor::UserSpace,
                CPUScalingGovernor::PowerSave,
                CPUScalingGovernor::Performance,
                CPUScalingGovernor::SchedUtil
            ]
        );
    }

    #[tokio::test]
    async fn read_invalid_cpu_available_governors() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPUFREQ_PREFIX).join(CPU0_NAME);
        create_dir_all(&base).await.expect("create_dir_all");

        let contents =
            "conservative ondemand userspace rescascade powersave performance schedutil\n";
        write(base.join(CPU_SCALING_AVAILABLE_GOVERNORS_SUFFIX), contents)
            .await
            .expect("write");

        assert_eq!(
            get_available_cpu_scaling_governors().await.unwrap(),
            vec![
                CPUScalingGovernor::Conservative,
                CPUScalingGovernor::OnDemand,
                CPUScalingGovernor::UserSpace,
                CPUScalingGovernor::PowerSave,
                CPUScalingGovernor::Performance,
                CPUScalingGovernor::SchedUtil
            ]
        );
    }

    #[tokio::test]
    async fn read_cpu_governor() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPUFREQ_PREFIX).join(CPU0_NAME);
        create_dir_all(&base).await.expect("create_dir_all");

        let contents = "ondemand\n";
        write(base.join(CPU_SCALING_GOVERNOR_SUFFIX), contents)
            .await
            .expect("write");

        assert_eq!(
            get_cpu_scaling_governor().await.unwrap(),
            CPUScalingGovernor::OnDemand
        );
    }

    #[tokio::test]
    async fn read_invalid_cpu_governor() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPUFREQ_PREFIX).join(CPU0_NAME);
        create_dir_all(&base).await.expect("create_dir_all");

        let contents = "rescascade\n";
        write(base.join(CPU_SCALING_GOVERNOR_SUFFIX), contents)
            .await
            .expect("write");

        assert!(get_cpu_scaling_governor().await.is_err());
    }

    #[tokio::test]
    async fn read_cpu_boost_state_cpufreq() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPUFREQ_PREFIX);
        let boost_path = base.join(CPUFREQ_BOOST_SUFFIX);
        create_dir_all(boost_path.parent().unwrap())
            .await
            .expect("create_dir_all");

        write(&boost_path, b"1\n").await.expect("write");
        assert_eq!(get_cpu_boost_state().await.unwrap(), CPUBoostState::Enabled);

        write(&boost_path, b"0\n").await.expect("write");
        assert_eq!(
            get_cpu_boost_state().await.unwrap(),
            CPUBoostState::Disabled
        );
    }

    #[tokio::test]
    async fn read_invalid_cpu_boost_state_cpufreq() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(CPUFREQ_PREFIX);
        let boost_path = base.join(CPUFREQ_BOOST_SUFFIX);
        create_dir_all(boost_path.parent().unwrap())
            .await
            .expect("create_dir_all");

        write(&boost_path, b"2\n").await.expect("write");
        assert!(get_cpu_boost_state().await.is_err());

        tokio::fs::remove_file(&boost_path)
            .await
            .expect("remove_file");
        assert!(get_cpu_boost_state().await.is_err());
    }

    #[tokio::test]
    async fn read_cpu_boost_state_intel_pstate() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(INTEL_PSTATE_PREFIX);
        let no_turbo_path = base.join(INTEL_PSTATE_NO_TURBO_SUFFIX);
        create_dir_all(no_turbo_path.parent().unwrap())
            .await
            .expect("create_dir_all");

        write(&no_turbo_path, b"0\n").await.expect("write");
        assert_eq!(get_cpu_boost_state().await.unwrap(), CPUBoostState::Enabled);

        write(&no_turbo_path, b"1\n").await.expect("write");
        assert_eq!(
            get_cpu_boost_state().await.unwrap(),
            CPUBoostState::Disabled
        );
    }

    #[tokio::test]
    async fn read_invalid_cpu_boost_state_intel_pstate() {
        let _h = testing::start();

        let base = path(CPU_PREFIX).join(INTEL_PSTATE_PREFIX);
        let no_turbo_path = base.join(INTEL_PSTATE_NO_TURBO_SUFFIX);
        create_dir_all(no_turbo_path.parent().unwrap())
            .await
            .expect("create_dir_all");

        write(&no_turbo_path, b"2\n").await.expect("write");
        assert!(get_cpu_boost_state().await.is_err());

        tokio::fs::remove_file(&no_turbo_path)
            .await
            .expect("remove_file");
        assert!(get_cpu_boost_state().await.is_err());
    }

    #[tokio::test]
    async fn read_max_charge_level_acpi_sb() {
        let handle = testing::start();

        let config = DeviceConfig {
            battery_charge_limit: Some(BatteryChargeLimitConfig {
                suggested_minimum_limit: 10,
                method: BatteryChargeLimitMethod::AcpiSb,
            }),
            ..DeviceConfig::default()
        };
        handle.test.set_device_config(config).await;

        let base = path(SB_PATHS[0]).join("BAT1");
        create_dir_all(&base).await.expect("create_dir_all");

        write(base.join("type"), "Battery\n").await.expect("write");

        write(base.join(SB_LIMIT_PATH), "10\n")
            .await
            .expect("write");

        assert_eq!(
            find_battery_charge_path().await.unwrap(),
            path(SB_PATHS[0]).join("BAT1/charge_control_end_threshold")
        );

        assert_eq!(get_max_charge_level().await.unwrap(), 10);

        write(base.join(SB_LIMIT_PATH), "99\n")
            .await
            .expect("write");

        assert_eq!(get_max_charge_level().await.unwrap(), 99);

        assert!(set_max_charge_level(101).await.is_err());
        assert!(set_max_charge_level(-1).await.is_err());
    }

    #[tokio::test]
    async fn read_max_charge_level_acpi_sb_legacy_path() {
        let handle = testing::start();

        let config = DeviceConfig {
            battery_charge_limit: Some(BatteryChargeLimitConfig {
                suggested_minimum_limit: 10,
                method: BatteryChargeLimitMethod::AcpiSb,
            }),
            ..DeviceConfig::default()
        };
        handle.test.set_device_config(config).await;

        let base = path(SB_PATHS[1]).join("BAT1");
        create_dir_all(&base).await.expect("create_dir_all");

        write(base.join("type"), "Battery\n").await.expect("write");

        write(base.join(SB_LIMIT_PATH), "80\n")
            .await
            .expect("write");

        assert_eq!(
            find_battery_charge_path().await.unwrap(),
            path(SB_PATHS[1]).join("BAT1/charge_control_end_threshold")
        );

        assert_eq!(get_max_charge_level().await.unwrap(), 80);
    }

    #[test]
    fn parse_max_charge_level_enodata_is_unknown() {
        let err = std::io::Error::from_raw_os_error(Errno::ENODATA as i32);
        assert_eq!(parse_max_charge_level(Err(err)).unwrap(), -1);
    }

    #[tokio::test]
    async fn read_max_charge_level_hwmmon() {
        let handle = testing::start();

        let config = DeviceConfig {
            battery_charge_limit: Some(BatteryChargeLimitConfig {
                suggested_minimum_limit: 10,
                method: BatteryChargeLimitMethod::HwmonAttribute {
                    hwmon: String::from("steamdeck_hwmon"),
                    attribute: String::from("max_battery_charge_level"),
                },
            }),
            ..DeviceConfig::default()
        };
        handle.test.set_device_config(config).await;

        let base = path(HWMON_PREFIX).join("hwmon6");
        create_dir_all(&base).await.expect("create_dir_all");

        write(base.join("name"), "steamdeck_hwmon\n")
            .await
            .expect("write");

        write(base.join("max_battery_charge_level"), "10\n")
            .await
            .expect("write");

        assert_eq!(
            find_battery_charge_path().await.unwrap(),
            path(HWMON_PREFIX).join("hwmon6/max_battery_charge_level")
        );

        assert_eq!(get_max_charge_level().await.unwrap(), 10);

        write(base.join("max_battery_charge_level"), "99\n")
            .await
            .expect("write");

        assert_eq!(get_max_charge_level().await.unwrap(), 99);

        assert!(set_max_charge_level(101).await.is_err());
        assert!(set_max_charge_level(-1).await.is_err());
    }

    #[tokio::test]
    async fn read_available_performance_profiles() {
        let _h = testing::start();

        assert!(
            get_available_platform_profiles("power-driver")
                .await
                .is_err()
        );

        let base = path(PLATFORM_PROFILE_PREFIX).join("platform-profile0");
        create_dir_all(&base).await.unwrap();
        assert!(
            get_available_platform_profiles("power-driver")
                .await
                .is_err()
        );

        write_synced(base.join("name"), b"power-driver\n")
            .await
            .unwrap();
        assert!(
            get_available_platform_profiles("power-driver")
                .await
                .is_err()
        );

        write_synced(base.join("choices"), b"a b c\n")
            .await
            .unwrap();
        assert_eq!(
            get_available_platform_profiles("power-driver")
                .await
                .unwrap(),
            &["a", "b", "c"]
        );
    }

    struct MockTdpLimit {
        queue: Sender<()>,
    }

    #[interface(name = "com.steampowered.SteamOSManager1.RootManager")]
    impl MockTdpLimit {
        async fn set_tdp_limit(&mut self, limit: u32) -> fdo::Result<()> {
            let hwmon = path(HWMON_PREFIX);
            write(
                hwmon.join("hwmon5").join(TDP_LIMIT1),
                format!("{limit}000000\n"),
            )
            .await
            .expect("write");
            self.queue.send(()).await.map_err(to_zbus_fdo_error)?;
            Ok(())
        }
    }

    struct MockFanControl {
        fan_speed_tx: Sender<u32>,
        fan_control_state_tx: Sender<u32>,
        fan_control_state: u32,
    }

    #[interface(name = "com.steampowered.SteamOSManager1.RootManager")]
    impl MockFanControl {
        async fn set_tdp_limit(&mut self, limit: u32) -> fdo::Result<()> {
            let hwmon = path(HWMON_PREFIX);
            write(
                hwmon.join("hwmon5").join(TDP_LIMIT1),
                format!("{limit}000000\n"),
            )
            .await
            .map_err(to_zbus_fdo_error)?;
            Ok(())
        }

        async fn set_fan_speed(&mut self, rpm: u32) -> fdo::Result<()> {
            self.fan_speed_tx
                .send(rpm)
                .await
                .map_err(to_zbus_fdo_error)?;
            Ok(())
        }

        #[zbus(property(emits_changed_signal = "false"))]
        async fn fan_control_state(&self) -> u32 {
            self.fan_control_state
        }

        #[zbus(property)]
        async fn set_fan_control_state(&mut self, state: u32) -> fdo::Result<()> {
            self.fan_control_state = state;
            self.fan_control_state_tx
                .send(state)
                .await
                .map_err(to_zbus_fdo_error)?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_low_power_lock() {
        let mut h = testing::start();
        setup().await.expect("setup");

        let connection = h.new_dbus().await.expect("new_dbus");
        let (tx, rx) = unbounded_channel();
        let (fin_tx, fin_rx) = oneshot::channel();
        let (start_tx, start_rx) = oneshot::channel();
        let (reply_tx, mut reply_rx) = channel(1);

        let iface = MockTdpLimit { queue: reply_tx };

        let config = DeviceConfig {
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::AmdgpuHwmon,
                range: Some(RangeConfig { min: 3, max: 15 }),
                download_mode_limit: NonZeroU32::new(6),
                firmware_attribute: None,
                performance_profile: None,
            }),
            ..DeviceConfig::default()
        };
        h.test.set_device_config(config).await;
        let manager = tdp_limit_manager(&connection).await.unwrap();

        connection
            .request_name("com.steampowered.SteamOSManager1")
            .await
            .expect("reserve_name");
        let object_server = connection.object_server();
        object_server
            .at("/com/steampowered/SteamOSManager1", iface)
            .await
            .expect("at");

        let mut service = TdpManagerService::new(rx, &connection, &connection)
            .await
            .expect("service");
        let task = tokio::spawn(async move {
            start_tx.send(()).unwrap();
            tokio::select! {
                r = service.run() => r,
                _ = fin_rx => Ok(()),
            }
        });
        start_rx.await.expect("start_rx");

        sleep(Duration::from_millis(1)).await;

        tx.send(TdpManagerCommand::SetTdpLimit(15)).unwrap();
        reply_rx.recv().await;
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 15);

        let (os_tx, os_rx) = oneshot::channel();
        tx.send(TdpManagerCommand::ListDownloadModeHandles(os_tx))
            .unwrap();
        assert!(os_rx.await.unwrap().is_empty());

        let (h_tx, h_rx) = oneshot::channel();
        tx.send(TdpManagerCommand::EnterDownloadMode(
            String::from("test"),
            h_tx,
        ))
        .unwrap();

        {
            let _h = h_rx.await.unwrap().expect("result").expect("handle");
            reply_rx.recv().await;
            assert_eq!(manager.get_tdp_limit().await.unwrap(), 6);

            let (os_tx, os_rx) = oneshot::channel();
            tx.send(TdpManagerCommand::ListDownloadModeHandles(os_tx))
                .unwrap();
            assert_eq!(os_rx.await.unwrap(), [(String::from("test"), 1u32)].into());

            tx.send(TdpManagerCommand::SetTdpLimit(15)).unwrap();
            assert!(tokio::select! {
                _ = reply_rx.recv() => false,
                _ = sleep(Duration::from_millis(2)) => true,
            });
            assert_eq!(manager.get_tdp_limit().await.unwrap(), 6);
        }
        reply_rx.recv().await;
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 15);

        tx.send(TdpManagerCommand::SetTdpLimit(12)).unwrap();
        reply_rx.recv().await;
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 12);

        let (os_tx, os_rx) = oneshot::channel();
        tx.send(TdpManagerCommand::ListDownloadModeHandles(os_tx))
            .unwrap();
        assert!(os_rx.await.unwrap().is_empty());

        fin_tx.send(()).expect("fin");
        task.await.expect("exit").expect("exit2");
    }

    #[tokio::test]
    async fn test_download_mode_fan_speed() {
        let mut h = testing::start();
        setup().await.expect("setup");

        let connection = h.new_dbus().await.expect("new_dbus");
        let (tx, rx) = unbounded_channel();
        let (fin_tx, fin_rx) = oneshot::channel();
        let (start_tx, start_rx) = oneshot::channel();
        let (fan_speed_tx, mut fan_speed_rx) = channel(1);
        let (fan_control_state_tx, mut fan_control_state_rx) = channel(1);

        let iface = MockFanControl {
            fan_speed_tx,
            fan_control_state_tx,
            fan_control_state: FanControlState::Os as u32,
        };

        let config = DeviceConfig {
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::AmdgpuHwmon,
                range: Some(RangeConfig { min: 3, max: 15 }),
                download_mode_limit: NonZeroU32::new(6),
                firmware_attribute: None,
                performance_profile: None,
            }),
            fan_speed: Some(FanSpeedConfig {
                hwmon: String::from("steamdeck_hwmon"),
                attribute: String::from("fan1_target"),
                download_mode_fan_speed: NonZeroU32::new(2000),
            }),
            ..DeviceConfig::default()
        };
        h.test.set_device_config(config).await;

        connection
            .request_name("com.steampowered.SteamOSManager1")
            .await
            .expect("reserve_name");
        let object_server = connection.object_server();
        object_server
            .at("/com/steampowered/SteamOSManager1", iface)
            .await
            .expect("at");

        let mut service = TdpManagerService::new(rx, &connection, &connection)
            .await
            .expect("service");
        let task = tokio::spawn(async move {
            start_tx.send(()).unwrap();
            tokio::select! {
                r = service.run() => r,
                _ = fin_rx => Ok(()),
            }
        });
        start_rx.await.expect("start_rx");

        sleep(Duration::from_millis(1)).await;

        tx.send(TdpManagerCommand::SetTdpLimit(6)).unwrap();

        let (h_tx, h_rx) = oneshot::channel();
        tx.send(TdpManagerCommand::EnterDownloadMode(
            String::from("test"),
            h_tx,
        ))
        .unwrap();

        {
            let _h = h_rx.await.unwrap().expect("result").expect("handle");

            assert_eq!(
                fan_control_state_rx.recv().await.unwrap(),
                FanControlState::Bios as u32
            );
            assert_eq!(fan_speed_rx.recv().await.unwrap(), 2000);
        }

        assert_eq!(
            fan_control_state_rx.recv().await.unwrap(),
            FanControlState::Os as u32
        );

        fin_tx.send(()).expect("fin");
        task.await.expect("exit").expect("exit2");
    }

    #[tokio::test]
    async fn test_disabled_low_power_lock() {
        let mut h = testing::start();
        setup().await.expect("setup");

        let connection = h.new_dbus().await.expect("new_dbus");
        let (tx, rx) = unbounded_channel();
        let (fin_tx, fin_rx) = oneshot::channel();
        let (start_tx, start_rx) = oneshot::channel();
        let (reply_tx, mut reply_rx) = channel(1);

        let iface = MockTdpLimit { queue: reply_tx };

        let config = DeviceConfig {
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::AmdgpuHwmon,
                range: Some(RangeConfig { min: 3, max: 15 }),
                download_mode_limit: None,
                firmware_attribute: None,
                performance_profile: None,
            }),
            ..DeviceConfig::default()
        };
        h.test.set_device_config(config).await;
        let manager = tdp_limit_manager(&connection).await.unwrap();

        connection
            .request_name("com.steampowered.SteamOSManager1")
            .await
            .expect("reserve_name");
        let object_server = connection.object_server();
        object_server
            .at("/com/steampowered/SteamOSManager1", iface)
            .await
            .expect("at");

        let mut service = TdpManagerService::new(rx, &connection, &connection)
            .await
            .expect("service");
        let task = tokio::spawn(async move {
            start_tx.send(()).unwrap();
            tokio::select! {
                r = service.run() => r,
                _ = fin_rx => Ok(()),
            }
        });
        start_rx.await.expect("start_rx");

        sleep(Duration::from_millis(1)).await;

        tx.send(TdpManagerCommand::SetTdpLimit(15)).unwrap();
        reply_rx.recv().await;
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 15);

        let (os_tx, os_rx) = oneshot::channel();
        tx.send(TdpManagerCommand::ListDownloadModeHandles(os_tx))
            .unwrap();
        assert!(os_rx.await.unwrap().is_empty());

        let (h_tx, h_rx) = oneshot::channel();
        tx.send(TdpManagerCommand::EnterDownloadMode(
            String::from("test"),
            h_tx,
        ))
        .unwrap();

        let h = h_rx.await.unwrap().expect("result");
        assert!(h.is_none());

        fin_tx.send(()).expect("fin");
        task.await.expect("exit").expect("exit2");
    }

    #[tokio::test]
    async fn test_firmware_attribute_tdp_limiter() {
        let mut h = testing::start();
        setup().await.expect("setup");

        let connection = h.new_dbus().await.expect("new_dbus");
        let config = DeviceConfig {
            performance_profile: Some(PerformanceProfileConfig {
                platform_profile_name: String::from("platform-profile0"),
                suggested_default: String::from("custom"),
            }),
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::FirmwareAttribute,
                range: Some(RangeConfig { min: 3, max: 15 }),
                download_mode_limit: None,
                firmware_attribute: Some(FirmwareAttributeConfig {
                    attribute: String::from("tdp0"),
                    performance_profile: Some(String::from("custom")),
                }),
                performance_profile: None,
            }),
            ..DeviceConfig::default()
        };
        h.test.set_device_config(config).await;

        let attributes_base = path(FirmwareAttributeLimitManager::PREFIX)
            .join("tdp0")
            .join("attributes");
        let spl_base = attributes_base.join(FirmwareAttributeLimitManager::SPL_SUFFIX);
        let sppt_base = attributes_base.join(FirmwareAttributeLimitManager::SPPT_SUFFIX);
        let fppt_base = attributes_base.join(FirmwareAttributeLimitManager::FPPT_SUFFIX);
        create_dir_all(&spl_base).await.unwrap();
        write_synced(spl_base.join("current_value"), b"10\n")
            .await
            .unwrap();
        create_dir_all(&sppt_base).await.unwrap();
        write_synced(sppt_base.join("current_value"), b"10\n")
            .await
            .unwrap();
        create_dir_all(&fppt_base).await.unwrap();
        write_synced(fppt_base.join("current_value"), b"10\n")
            .await
            .unwrap();

        write_synced(spl_base.join("min_value"), b"6\n")
            .await
            .unwrap();
        write_synced(spl_base.join("max_value"), b"20\n")
            .await
            .unwrap();
        write_synced(sppt_base.join("min_value"), b"8\n")
            .await
            .unwrap();
        write_synced(fppt_base.join("min_value"), b"9\n")
            .await
            .unwrap();

        let platform_profile_base = path(PLATFORM_PROFILE_PREFIX).join("platform-profile0");
        create_dir_all(&platform_profile_base).await.unwrap();
        write_synced(platform_profile_base.join("name"), b"platform-profile0\n")
            .await
            .unwrap();
        write_synced(platform_profile_base.join("profile"), b"custom\n")
            .await
            .unwrap();

        let manager = tdp_limit_manager(&connection).await.unwrap();

        assert!(manager.is_active().await.unwrap());
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 10);

        manager.set_tdp_limit(15).await.unwrap();
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 15);
        assert_eq!(
            read_to_string(spl_base.join("current_value"))
                .await
                .unwrap(),
            "15"
        );
        assert_eq!(
            read_to_string(sppt_base.join("current_value"))
                .await
                .unwrap(),
            "15"
        );
        assert_eq!(
            read_to_string(fppt_base.join("current_value"))
                .await
                .unwrap(),
            "15"
        );

        manager.set_tdp_limit(25).await.unwrap_err();
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 15);

        manager.set_tdp_limit(7).await.unwrap();
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 7);
        assert_eq!(
            read_to_string(spl_base.join("current_value"))
                .await
                .unwrap(),
            "7"
        );
        assert_eq!(
            read_to_string(sppt_base.join("current_value"))
                .await
                .unwrap(),
            "8"
        );
        assert_eq!(
            read_to_string(fppt_base.join("current_value"))
                .await
                .unwrap(),
            "9"
        );

        manager.set_tdp_limit(2).await.unwrap_err();
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 7);

        write_synced(platform_profile_base.join("profile"), b"balanced\n")
            .await
            .unwrap();

        manager.set_tdp_limit(10).await.unwrap_err();
    }

    #[tokio::test]
    async fn test_firmware_attribute_tdp_limiter_no_profile() {
        let mut h = testing::start();
        setup().await.expect("setup");

        let connection = h.new_dbus().await.expect("new_dbus");
        let config = DeviceConfig {
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::FirmwareAttribute,
                range: Some(RangeConfig { min: 3, max: 15 }),
                download_mode_limit: None,
                firmware_attribute: Some(FirmwareAttributeConfig {
                    attribute: String::from("tdp0"),
                    performance_profile: None,
                }),
                performance_profile: None,
            }),
            ..DeviceConfig::default()
        };
        h.test.set_device_config(config).await;

        let attributes_base = path(FirmwareAttributeLimitManager::PREFIX)
            .join("tdp0")
            .join("attributes");
        let spl_base = attributes_base.join(FirmwareAttributeLimitManager::SPL_SUFFIX);
        let sppt_base = attributes_base.join(FirmwareAttributeLimitManager::SPPT_SUFFIX);
        let fppt_base = attributes_base.join(FirmwareAttributeLimitManager::FPPT_SUFFIX);
        create_dir_all(&spl_base).await.unwrap();
        write_synced(spl_base.join("current_value"), b"10\n")
            .await
            .unwrap();
        create_dir_all(&sppt_base).await.unwrap();
        write_synced(sppt_base.join("current_value"), b"10\n")
            .await
            .unwrap();
        create_dir_all(&fppt_base).await.unwrap();
        write_synced(fppt_base.join("current_value"), b"10\n")
            .await
            .unwrap();

        write_synced(spl_base.join("min_value"), b"6\n")
            .await
            .unwrap();
        write_synced(spl_base.join("max_value"), b"20\n")
            .await
            .unwrap();
        write_synced(sppt_base.join("min_value"), b"8\n")
            .await
            .unwrap();
        write_synced(fppt_base.join("min_value"), b"9\n")
            .await
            .unwrap();

        let manager = tdp_limit_manager(&connection).await.unwrap();

        assert!(manager.is_active().await.unwrap());
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 10);

        manager.set_tdp_limit(15).await.unwrap();
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 15);
        assert_eq!(
            read_to_string(spl_base.join("current_value"))
                .await
                .unwrap(),
            "15"
        );
        assert_eq!(
            read_to_string(sppt_base.join("current_value"))
                .await
                .unwrap(),
            "15"
        );
        assert_eq!(
            read_to_string(fppt_base.join("current_value"))
                .await
                .unwrap(),
            "15"
        );

        manager.set_tdp_limit(25).await.unwrap_err();
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 15);

        manager.set_tdp_limit(7).await.unwrap();
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 7);
        assert_eq!(
            read_to_string(spl_base.join("current_value"))
                .await
                .unwrap(),
            "7"
        );
        assert_eq!(
            read_to_string(sppt_base.join("current_value"))
                .await
                .unwrap(),
            "8"
        );
        assert_eq!(
            read_to_string(fppt_base.join("current_value"))
                .await
                .unwrap(),
            "9"
        );

        manager.set_tdp_limit(2).await.unwrap_err();
        assert_eq!(manager.get_tdp_limit().await.unwrap(), 7);
    }
}
