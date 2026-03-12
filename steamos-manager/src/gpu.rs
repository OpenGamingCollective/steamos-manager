/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, anyhow, bail, ensure};
use async_trait::async_trait;
use num_enum::TryFromPrimitive;
use regex::Regex;
use serde::Deserialize;
use std::fmt::Display;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;
use strum::{Display, EnumString, VariantNames};
use tokio::fs::{self, File, try_exists};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error};

use crate::hardware::{device_config, device_type};
use crate::power::find_hwmon;
use crate::{path, write_synced};

pub(crate) const AMDGPU_HWMON_NAME: &str = "amdgpu";

#[cfg(not(test))]
const DRM_PREFIX: &str = "/sys/class/drm";
#[cfg(test)]
pub const DRM_PREFIX: &str = "drm";

static AMDGPU_POWER_PROFILE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?<value>[0-9]+)\s+(?<name>[0-9A-Za-z_]+)(?<active>\*)?").unwrap()
});
static AMDGPU_CLOCK_LEVELS_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?<index>[0-9]+): (?<value>[0-9]+)Mhz").unwrap());

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum GpuPowerProfile {
    Amdgpu(AmdgpuPowerProfile),
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case")]
#[repr(u32)]
pub enum AmdgpuPowerProfile {
    // Currently firmware exposes these values, though
    // deck doesn't support them yet
    #[strum(serialize = "3d_full_screen")]
    FullScreen = 1,
    Video = 3,
    VR = 4,
    Compute = 5,
    Custom = 6,
    // Currently only capped and uncapped are supported on
    // deck hardware/firmware. Add more later as needed
    Capped = 8,
    Uncapped = 9,
}

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum GpuPerformanceLevel {
    Amdgpu(AmdgpuPerformanceLevel),
    Intel(IntelPerformanceLevel),
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "snake_case")]
pub enum AmdgpuPerformanceLevel {
    Auto,
    Low,
    High,
    Manual,
    ProfilePeak,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone)]
#[strum(serialize_all = "snake_case")]
pub enum IntelPerformanceLevel {
    Auto,
    Manual,
}

#[derive(Deserialize, Display, EnumString, VariantNames, PartialEq, Debug, Clone)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[serde(rename_all = "snake_case")]
pub enum GpuPowerProfileDriverType {
    Amdgpu,
}

#[derive(Deserialize, Display, EnumString, VariantNames, PartialEq, Debug, Clone)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[serde(rename_all = "snake_case")]
pub enum GpuPerformanceLevelDriverType {
    Amdgpu,
    Intel,
}

#[derive(Debug)]
pub(crate) struct AmdgpuPowerProfileDriver {}

#[derive(Debug)]
pub(crate) struct AmdgpuPerformanceLevelDriver {}

#[derive(Debug)]
pub(crate) struct IntelGpuPerformanceLevelDriver {
    card_path: PathBuf,
    config: IntelGpuConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct IntelGpuConfig {
    min_freq: &'static str,
    max_freq: &'static str,
    range_min: &'static str,
    range_max: &'static str,
}

#[async_trait]
pub(crate) trait GpuPowerProfileDriver: Send + Sync {
    fn power_profile_from_str(&self, value: &str) -> Result<GpuPowerProfile>;
    async fn get_available_power_profiles(&self) -> Result<Vec<(u32, String)>>;
    async fn get_power_profile(&self) -> Result<GpuPowerProfile>;
    async fn set_power_profile(&self, value: GpuPowerProfile) -> Result<()>;
}

#[async_trait]
pub(crate) trait GpuPerformanceLevelDriver: Send + Sync {
    fn performance_level_from_str(&self, value: &str) -> Result<GpuPerformanceLevel>;
    async fn get_available_performance_levels(&self) -> Result<Vec<GpuPerformanceLevel>>;
    async fn get_performance_level(&self) -> Result<GpuPerformanceLevel>;
    async fn set_performance_level(&self, level: GpuPerformanceLevel) -> Result<()>;

    async fn get_clocks_range(&self) -> Result<RangeInclusive<u32>>;
    async fn get_clocks(&self) -> Result<u32>;
    async fn set_clocks(&self, clocks: u32) -> Result<()>;
}

pub(crate) async fn gpu_power_profile_driver() -> Result<Box<dyn GpuPowerProfileDriver>> {
    let config = device_config().await?;
    let config = config
        .as_ref()
        .and_then(|config| config.gpu_power_profile.as_ref())
        .ok_or(anyhow!("No GPU power profile driver configured"))?;

    Ok(match &config.driver {
        GpuPowerProfileDriverType::Amdgpu => Box::new(AmdgpuPowerProfileDriver {}),
    })
}

async fn detect_gpu_type() -> Result<GpuPerformanceLevelDriverType> {
    if find_hwmon(AMDGPU_HWMON_NAME).await.is_ok() {
        debug!("Auto-detected AMD GPU");
        return Ok(GpuPerformanceLevelDriverType::Amdgpu);
    }

    if IntelGpuPerformanceLevelDriver::detect_gpu_info()
        .await
        .is_ok()
    {
        debug!("Auto-detected Intel GPU");
        return Ok(GpuPerformanceLevelDriverType::Intel);
    }

    bail!("No supported GPU detected automatically")
}

pub(crate) async fn gpu_performance_level_driver() -> Result<Box<dyn GpuPerformanceLevelDriver>> {
    let config = device_config().await?;

    let driver_type =
        if let Some(gpu_config) = config.as_ref().and_then(|c| c.gpu_performance.as_ref()) {
            gpu_config.driver.clone()
        } else {
            debug!("No GPU performance config found, auto-detecting GPU type");
            detect_gpu_type().await?
        };

    Ok(match driver_type {
        GpuPerformanceLevelDriverType::Amdgpu => Box::new(AmdgpuPerformanceLevelDriver {}),
        GpuPerformanceLevelDriverType::Intel => {
            Box::new(IntelGpuPerformanceLevelDriver::new().await?)
        }
    })
}

impl Display for GpuPerformanceLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            GpuPerformanceLevel::Amdgpu(v) => write!(f, "{v}"),
            GpuPerformanceLevel::Intel(v) => write!(f, "{v}"),
        }
    }
}

impl Display for GpuPowerProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            GpuPowerProfile::Amdgpu(v) => write!(f, "{v}"),
        }
    }
}

trait AmdgpuGpuPerfDriver {
    async fn read_sysfs_contents<S: AsRef<Path>>(suffix: S) -> Result<String> {
        // Read a given suffix for the GPU
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        fs::read_to_string(base.join(suffix.as_ref()))
            .await
            .map_err(|message| anyhow!("Error opening sysfs file for reading {message}"))
    }

    async fn write_sysfs_contents<S: AsRef<Path>>(suffix: S, data: &[u8]) -> Result<()> {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        Ok(write_synced(base.join(suffix), data)
            .await
            .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?)
    }
}

impl AmdgpuPowerProfileDriver {
    const POWER_PROFILE_SUFFIX: &str = "device/pp_power_profile_mode";
}

impl AmdgpuGpuPerfDriver for AmdgpuPowerProfileDriver {}

#[async_trait]
impl GpuPowerProfileDriver for AmdgpuPowerProfileDriver {
    fn power_profile_from_str(&self, value: &str) -> Result<GpuPowerProfile> {
        Ok(GpuPowerProfile::Amdgpu(AmdgpuPowerProfile::from_str(
            value,
        )?))
    }

    async fn get_power_profile(&self) -> Result<GpuPowerProfile> {
        // check which profile is current and return if possible
        let contents = Self::read_sysfs_contents(Self::POWER_PROFILE_SUFFIX).await?;

        // NOTE: We don't filter based on deck here because the sysfs
        // firmware support setting the value to no-op values.
        let lines = contents.lines();
        for line in lines {
            let Some(caps) = AMDGPU_POWER_PROFILE_REGEX.captures(line) else {
                continue;
            };

            let name = &caps["name"].to_lowercase();
            if caps.name("active").is_some() {
                match AmdgpuPowerProfile::from_str(name.as_str()) {
                    Ok(v) => {
                        return Ok(GpuPowerProfile::Amdgpu(v));
                    }
                    Err(e) => bail!("Unable to parse value for GPU power profile: {e}"),
                }
            }
        }
        bail!("Unable to determine current GPU power profile");
    }

    async fn get_available_power_profiles(&self) -> Result<Vec<(u32, String)>> {
        let contents = Self::read_sysfs_contents(Self::POWER_PROFILE_SUFFIX).await?;
        let deck = device_type().await.unwrap_or_default() == "steam_deck";

        let mut map = Vec::new();
        let lines = contents.lines();
        for line in lines {
            let Some(caps) = AMDGPU_POWER_PROFILE_REGEX.captures(line) else {
                continue;
            };
            let value: u32 = caps["value"].parse().map_err(|message| {
                anyhow!("Unable to parse value for GPU power profile: {message}")
            })?;
            let name = &caps["name"];
            if deck {
                // Deck is designed to operate in one of the CAPPED or UNCAPPED power profiles,
                // the other profiles aren't correctly tuned for the hardware.
                if value == AmdgpuPowerProfile::Capped as u32
                    || value == AmdgpuPowerProfile::Uncapped as u32
                {
                    map.push((value, name.to_string()));
                } else {
                    // Got unsupported value, so don't include it
                }
            } else {
                // Do basic validation to ensure our enum is up to date?
                map.push((value, name.to_string()));
            }
        }
        Ok(map)
    }

    async fn set_power_profile(&self, value: GpuPowerProfile) -> Result<()> {
        #[allow(irrefutable_let_patterns)] // Remove when more values are added
        let GpuPowerProfile::Amdgpu(value) = value else {
            bail!("This is not an amdgpu-compatible profile");
        };
        let profile = (value as u32).to_string();
        Self::write_sysfs_contents(Self::POWER_PROFILE_SUFFIX, profile.as_bytes()).await
    }
}

impl AmdgpuPerformanceLevelDriver {
    const CLOCKS_SUFFIX: &str = "device/pp_od_clk_voltage";
    const CLOCK_LEVELS_SUFFIX: &str = "device/pp_dpm_sclk";
    const PERFORMANCE_LEVEL_SUFFIX: &str = "device/power_dpm_force_performance_level";
}

impl AmdgpuGpuPerfDriver for AmdgpuPerformanceLevelDriver {}

#[async_trait]
impl GpuPerformanceLevelDriver for AmdgpuPerformanceLevelDriver {
    fn performance_level_from_str(&self, value: &str) -> Result<GpuPerformanceLevel> {
        Ok(GpuPerformanceLevel::Amdgpu(
            AmdgpuPerformanceLevel::from_str(value)?,
        ))
    }

    async fn get_available_performance_levels(&self) -> Result<Vec<GpuPerformanceLevel>> {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        if try_exists(base.join(Self::PERFORMANCE_LEVEL_SUFFIX)).await? {
            Ok(vec![
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Auto),
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Low),
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::High),
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Manual),
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::ProfilePeak),
            ])
        } else {
            Ok(Vec::new())
        }
    }

    async fn get_performance_level(&self) -> Result<GpuPerformanceLevel> {
        let level = Self::read_sysfs_contents(Self::PERFORMANCE_LEVEL_SUFFIX).await?;
        Ok(GpuPerformanceLevel::Amdgpu(
            AmdgpuPerformanceLevel::from_str(level.trim())?,
        ))
    }

    async fn set_performance_level(&self, level: GpuPerformanceLevel) -> Result<()> {
        #[allow(irrefutable_let_patterns)] // Remove when more values are added
        let GpuPerformanceLevel::Amdgpu(level) = level else {
            bail!("This is not an amdgpu-compatible performance level");
        };
        let level: String = level.to_string();
        Self::write_sysfs_contents(Self::PERFORMANCE_LEVEL_SUFFIX, level.as_bytes()).await
    }

    async fn get_clocks_range(&self) -> Result<RangeInclusive<u32>> {
        if let Some(range) = device_config()
            .await?
            .as_ref()
            .and_then(|config| config.gpu_performance.as_ref())
            .and_then(|config| config.clocks)
        {
            return Ok(range.min..=range.max);
        }
        let contents = Self::read_sysfs_contents(Self::CLOCK_LEVELS_SUFFIX).await?;
        let lines = contents.lines();
        let mut min = 1_000_000;
        let mut max = 0;

        for line in lines {
            let Some(caps) = AMDGPU_CLOCK_LEVELS_REGEX.captures(line) else {
                continue;
            };
            let value: u32 = caps["value"].parse().map_err(|message| {
                anyhow!("Unable to parse value for GPU power profile: {message}")
            })?;
            if value < min {
                min = value;
            }
            if value > max {
                max = value;
            }
        }

        ensure!(min <= max, "Could not read any clocks");
        Ok(min..=max)
    }

    async fn set_clocks(&self, clocks: u32) -> Result<()> {
        // Set GPU clocks to given value valid
        // Only used when GPU Performance Level is manual, but write whenever called.
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        let mut myfile = File::create(base.join(Self::CLOCKS_SUFFIX))
            .await
            .inspect_err(|message| error!("Error opening sysfs file for writing: {message}"))?;

        let data = format!("s 0 {clocks}\n");
        myfile
            .write(data.as_bytes())
            .await
            .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
        myfile.flush().await?;

        let data = format!("s 1 {clocks}\n");
        myfile
            .write(data.as_bytes())
            .await
            .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
        myfile.flush().await?;

        myfile
            .write("c\n".as_bytes())
            .await
            .inspect_err(|message| error!("Error writing to sysfs file: {message}"))?;
        myfile.flush().await?;

        Ok(())
    }

    async fn get_clocks(&self) -> Result<u32> {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await?;
        let clocks_file = File::open(base.join(Self::CLOCKS_SUFFIX)).await?;
        let mut reader = BufReader::new(clocks_file);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                break;
            }
            if line != "OD_SCLK:\n" {
                continue;
            }

            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                break;
            }
            let mhz = match line.split_whitespace().nth(1) {
                Some(mhz) if mhz.ends_with("Mhz") => mhz.trim_end_matches("Mhz"),
                _ => break,
            };

            return Ok(mhz.parse()?);
        }
        Ok(0)
    }
}

impl IntelGpuConfig {
    const I915: Self = Self {
        min_freq: "device/gt_min_freq_mhz",
        max_freq: "device/gt_max_freq_mhz",
        range_min: "device/gt_RPn_freq_mhz",
        range_max: "device/gt_RP0_freq_mhz",
    };

    const XE: Self = Self {
        // gt0 = graphics engine, gt1 = media engine
        // we only care about gt0 for performance levels
        min_freq: "device/tile0/gt0/freq0/min_freq",
        max_freq: "device/tile0/gt0/freq0/max_freq",
        // use RPe and RPa for efficient power ranges
        range_min: "device/tile0/gt0/freq0/rpe_freq",
        range_max: "device/tile0/gt0/freq0/rpa_freq",
    };
}

impl IntelGpuPerformanceLevelDriver {
    pub async fn new() -> Result<Self> {
        let (card_path, config) = Self::detect_gpu_info().await?;
        Ok(Self { card_path, config })
    }

    // DG2 cards and below are compatible with both i915 (default)
    // and Xe (experimental), so we should check both i915 and Xe paths
    async fn detect_gpu_info() -> Result<(PathBuf, IntelGpuConfig)> {
        let drm_path = path(DRM_PREFIX);
        let mut dir = fs::read_dir(&drm_path).await?;
        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str());

            let Some(name) = file_name else {
                continue;
            };
            if name.starts_with("card") && name != "card-" {
                // Check for i915
                let i915_path = path.join(IntelGpuConfig::I915.min_freq);
                if try_exists(&i915_path).await? {
                    debug!("Found i915 GPU at {name}");
                    return Ok((path, IntelGpuConfig::I915));
                }

                // Check for Xe
                let xe_path = path.join(IntelGpuConfig::XE.min_freq);
                if try_exists(&xe_path).await? {
                    debug!("Found Xe GPU at {name}");
                    return Ok((path, IntelGpuConfig::XE));
                }
            }
        }
        bail!("No Intel GPU found")
    }

    async fn read_sysfs_contents<S: AsRef<Path>>(&self, suffix: S) -> Result<String> {
        let path = self.card_path.join(suffix.as_ref());
        fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow!("Error reading Intel GPU sysfs file: {e}"))
    }

    async fn write_sysfs_contents<S: AsRef<Path>>(&self, suffix: S, data: &[u8]) -> Result<()> {
        let path = self.card_path.join(suffix.as_ref());
        Ok(write_synced(path, data).await?)
    }

    async fn read_freq(&self, freq_path: &str) -> Result<u32> {
        let val_str = self.read_sysfs_contents(freq_path).await?;
        val_str
            .trim()
            .parse::<u32>()
            .map_err(|e| anyhow!("Unable to parse Intel GPU frequency: {e}"))
    }

    async fn write_freq(&self, freq_path: &str, clocks: u32) -> Result<()> {
        self.write_sysfs_contents(freq_path, clocks.to_string().as_bytes())
            .await
    }
}

#[async_trait]
impl GpuPerformanceLevelDriver for IntelGpuPerformanceLevelDriver {
    fn performance_level_from_str(&self, value: &str) -> Result<GpuPerformanceLevel> {
        Ok(GpuPerformanceLevel::Intel(IntelPerformanceLevel::from_str(
            value,
        )?))
    }

    async fn get_available_performance_levels(&self) -> Result<Vec<GpuPerformanceLevel>> {
        Ok(vec![
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Auto),
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual),
        ])
    }

    async fn get_performance_level(&self) -> Result<GpuPerformanceLevel> {
        // Auto mode: min_freq < max_freq (hardware manages frequency scaling)
        // Manual mode: min_freq == max_freq (locked to specific frequency)
        let min_freq = self.read_freq(self.config.min_freq).await?;
        let max_freq = self.read_freq(self.config.max_freq).await?;

        let performance_level = if min_freq < max_freq {
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Auto)
        } else {
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual)
        };

        Ok(performance_level)
    }

    async fn set_performance_level(&self, level: GpuPerformanceLevel) -> Result<()> {
        let GpuPerformanceLevel::Intel(level) = level else {
            bail!("This is not an Intel-compatible performance level");
        };

        match level {
            IntelPerformanceLevel::Auto => {
                // For Auto mode, we need to set min and max back to hardware range
                let range_min = self.read_freq(self.config.range_min).await?;
                let range_max = self.read_freq(self.config.range_max).await?;

                self.write_freq(self.config.min_freq, range_min).await?;
                self.write_freq(self.config.max_freq, range_max).await
            }
            IntelPerformanceLevel::Manual => {
                // For Manual mode, we need to ensure min_freq == max_freq
                let range_min = self.read_freq(self.config.range_min).await?;
                let range_max = self.read_freq(self.config.range_max).await?;
                let mean_freq = (range_min + range_max) / 2;

                self.write_freq(self.config.min_freq, mean_freq).await?;
                self.write_freq(self.config.max_freq, mean_freq).await
            }
        }
    }

    async fn get_clocks_range(&self) -> Result<RangeInclusive<u32>> {
        if let Some(range) = device_config()
            .await?
            .as_ref()
            .and_then(|config| config.gpu_performance.as_ref())
            .and_then(|config| config.clocks)
        {
            return Ok(range.min..=range.max);
        }

        let min = self.read_freq(self.config.range_min).await?;
        let max = self.read_freq(self.config.range_max).await?;

        ensure!(min <= max, "Invalid GPU frequency range");
        Ok(min..=max)
    }

    async fn get_clocks(&self) -> Result<u32> {
        self.read_freq(self.config.min_freq).await
    }

    async fn set_clocks(&self, clocks: u32) -> Result<()> {
        let current_level = self.get_performance_level().await?;

        if current_level == GpuPerformanceLevel::Intel(IntelPerformanceLevel::Auto) {
            return Ok(());
        } else {
            self.write_freq(self.config.min_freq, clocks).await?;
            self.write_freq(self.config.max_freq, clocks).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use crate::hardware::SteamDeckVariant;
    use crate::hardware::test::fake_model;
    use crate::power::HWMON_PREFIX;
    use crate::{enum_roundtrip, path, testing};
    use tokio::fs::{create_dir_all, read_to_string, write};

    #[derive(Debug, Default, Clone)]
    pub struct Nodes {
        amdgpu: bool,
        performance_level: bool,
        power_profile: bool,
    }

    impl Nodes {
        pub fn all() -> Nodes {
            Nodes {
                amdgpu: true,
                performance_level: true,
                power_profile: true,
            }
        }

        pub fn none() -> Nodes {
            Nodes::default()
        }
    }

    pub async fn setup_amdgpu() -> Result<()> {
        // Use hwmon5 just as a test. We needed a subfolder of HWMON_PREFIX
        // and this is as good as any.
        let base = path(HWMON_PREFIX).join("hwmon5");
        let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);
        // Creates hwmon path, including device subpath
        create_dir_all(filename.parent().unwrap()).await?;
        // Writes name file as addgpu so find_hwmon() will find it.
        write_synced(base.join("name"), AMDGPU_HWMON_NAME.as_bytes()).await?;
        Ok(())
    }

    pub async fn setup_intel_i915() -> Result<()> {
        let drm_path = path(DRM_PREFIX);
        create_dir_all(&drm_path).await?;

        let card_path = drm_path.join("card0");
        let device_path = card_path.join("device");

        create_dir_all(&device_path).await?;

        write(device_path.join("gt_min_freq_mhz"), "100").await?;
        write(device_path.join("gt_max_freq_mhz"), "1100").await?;
        write(device_path.join("gt_RPn_freq_mhz"), "100").await?;
        write(device_path.join("gt_RP0_freq_mhz"), "1100").await?;

        Ok(())
    }

    pub async fn setup_intel_xe() -> Result<()> {
        let drm_path = path(DRM_PREFIX);
        create_dir_all(&drm_path).await?;

        let card_path = drm_path.join("card0");
        let device_path = card_path.join("device");
        let freq_path = device_path.join("tile0/gt0/freq0");

        create_dir_all(&freq_path).await?;

        write(freq_path.join("min_freq"), "300").await?;
        write(freq_path.join("max_freq"), "1200").await?;
        write(freq_path.join("rpe_freq"), "300").await?;
        write(freq_path.join("rpa_freq"), "1200").await?;

        Ok(())
    }

    pub async fn create_nodes(which: &Nodes) -> Result<()> {
        if which.amdgpu {
            setup_amdgpu().await?;
            let base = find_hwmon(AMDGPU_HWMON_NAME).await?;

            if which.performance_level {
                let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);
                write(filename.as_path(), "auto\n").await?;
            }

            if which.power_profile {
                let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
                let contents = " 1 3D_FULL_SCREEN
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";
                write(filename.as_path(), contents).await?;
            }
        }

        Ok(())
    }

    pub async fn write_clocks(mhz: u32) {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::CLOCKS_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = format!(
            "OD_SCLK:
0:       {mhz}Mhz
1:       {mhz}Mhz
OD_RANGE:
SCLK:     200Mhz       1600Mhz
CCLK:    1400Mhz       3500Mhz
CCLK_RANGE in Core0:
0:       1400Mhz
1:       3500Mhz\n"
        );

        write(filename.as_path(), contents).await.expect("write");
    }

    pub async fn read_clocks() -> Result<String, std::io::Error> {
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        read_to_string(base.join(AmdgpuPerformanceLevelDriver::CLOCKS_SUFFIX)).await
    }

    pub fn format_clocks(mhz: u32) -> String {
        format!("s 0 {mhz}\ns 1 {mhz}\nc\n")
    }

    #[tokio::test]
    async fn test_get_gpu_performance_level() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        setup_amdgpu().await.expect("setup_amdgpu");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);
        assert!(driver.get_performance_level().await.is_err());

        write(filename.as_path(), "auto\n").await.expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Auto)
        );

        write(filename.as_path(), "low\n").await.expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Low)
        );

        write(filename.as_path(), "high\n").await.expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::High)
        );

        write(filename.as_path(), "manual\n").await.expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Manual)
        );

        write(filename.as_path(), "profile_peak\n")
            .await
            .expect("write");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::ProfilePeak)
        );

        write(filename.as_path(), "nothing\n").await.expect("write");
        assert!(driver.get_performance_level().await.is_err());
    }

    #[tokio::test]
    async fn test_set_gpu_performance_level() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        setup_amdgpu().await.expect("setup_amdgpu");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);

        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Auto))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "auto"
        );
        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Low))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "low"
        );
        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::High))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "high"
        );
        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Manual))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "manual"
        );
        driver
            .set_performance_level(GpuPerformanceLevel::Amdgpu(
                AmdgpuPerformanceLevel::ProfilePeak,
            ))
            .await
            .expect("set");
        assert_eq!(
            read_to_string(filename.as_path()).await.unwrap().trim(),
            "profile_peak"
        );
    }

    #[tokio::test]
    async fn test_get_amdgpu_gpu_clocks() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        assert!(driver.get_clocks().await.is_err());
        setup_amdgpu().await.expect("setup_amdgpu");

        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::CLOCKS_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");
        write(filename.as_path(), b"").await.expect("write");

        assert_eq!(driver.get_clocks().await.unwrap(), 0);
        write_clocks(1600).await;

        assert_eq!(driver.get_clocks().await.unwrap(), 1600);
    }

    #[tokio::test]
    async fn test_set_amdgpu_gpu_clocks() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        assert!(driver.set_clocks(1600).await.is_err());
        setup_amdgpu().await.expect("setup_amdgpu");

        assert!(driver.set_clocks(200).await.is_ok());

        assert_eq!(read_clocks().await.unwrap(), format_clocks(200));

        assert!(driver.set_clocks(1600).await.is_ok());
        assert_eq!(read_clocks().await.unwrap(), format_clocks(1600));
    }

    #[tokio::test]
    async fn test_get_amdgpu_gpu_clocks_range() {
        let _h = testing::start();
        let driver = AmdgpuPerformanceLevelDriver {};

        setup_amdgpu().await.expect("setup_amdgpu");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPerformanceLevelDriver::CLOCK_LEVELS_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        assert!(driver.get_clocks_range().await.is_err());

        write(filename.as_path(), &[] as &[u8; 0])
            .await
            .expect("write");
        assert!(driver.get_clocks_range().await.is_err());

        let contents = "0: 200Mhz *
1: 1100Mhz
2: 1600Mhz";
        write(filename.as_path(), contents).await.expect("write");
        assert_eq!(driver.get_clocks_range().await.unwrap(), 200..=1600);

        let contents = "0: 1600Mhz *
1: 200Mhz
2: 1100Mhz";
        write(filename.as_path(), contents).await.expect("write");
        assert_eq!(driver.get_clocks_range().await.unwrap(), 200..=1600);
    }

    #[test]
    fn amdgpu_gpu_power_profile_roundtrip() {
        enum_roundtrip!(AmdgpuPowerProfile {
            1: u32 = FullScreen,
            3: u32 = Video,
            4: u32 = VR,
            5: u32 = Compute,
            6: u32 = Custom,
            8: u32 = Capped,
            9: u32 = Uncapped,
            "3d_full_screen": str = FullScreen,
            "video": str = Video,
            "vr": str = VR,
            "compute": str = Compute,
            "custom": str = Custom,
            "capped": str = Capped,
            "uncapped": str = Uncapped,
        });
        assert!(AmdgpuPowerProfile::try_from(0).is_err());
        assert!(AmdgpuPowerProfile::try_from(2).is_err());
        assert!(AmdgpuPowerProfile::try_from(10).is_err());
        assert!(AmdgpuPowerProfile::from_str("fullscreen").is_err());
    }

    #[test]
    fn amdgpu_gpu_performance_level_roundtrip() {
        enum_roundtrip!(AmdgpuPerformanceLevel {
            "auto": str = Auto,
            "low": str = Low,
            "high": str = High,
            "manual": str = Manual,
            "profile_peak": str = ProfilePeak,
        });
        assert!(AmdgpuPerformanceLevel::from_str("peak_performance").is_err());
    }

    #[tokio::test]
    async fn read_amdgpu_power_profiles() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup_amdgpu().await.expect("setup_amdgpu");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");

        let profiles = driver.get_available_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (
                    AmdgpuPowerProfile::FullScreen as u32,
                    String::from("3D_FULL_SCREEN")
                ),
                (AmdgpuPowerProfile::Video as u32, String::from("VIDEO")),
                (AmdgpuPowerProfile::VR as u32, String::from("VR")),
                (AmdgpuPowerProfile::Compute as u32, String::from("COMPUTE")),
                (AmdgpuPowerProfile::Custom as u32, String::from("CUSTOM")),
                (AmdgpuPowerProfile::Capped as u32, String::from("CAPPED")),
                (
                    AmdgpuPowerProfile::Uncapped as u32,
                    String::from("UNCAPPED")
                )
            ]
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");

        let profiles = driver.get_available_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (AmdgpuPowerProfile::Capped as u32, String::from("CAPPED")),
                (
                    AmdgpuPowerProfile::Uncapped as u32,
                    String::from("UNCAPPED")
                )
            ]
        );
    }

    #[tokio::test]
    async fn read_amdgpu_unknown_power_profiles() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup_amdgpu().await.expect("setup_amdgpu");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 2            CGA
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");

        let profiles = driver.get_available_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (
                    AmdgpuPowerProfile::FullScreen as u32,
                    String::from("3D_FULL_SCREEN")
                ),
                (2, String::from("CGA")),
                (AmdgpuPowerProfile::Video as u32, String::from("VIDEO")),
                (AmdgpuPowerProfile::VR as u32, String::from("VR")),
                (AmdgpuPowerProfile::Compute as u32, String::from("COMPUTE")),
                (AmdgpuPowerProfile::Custom as u32, String::from("CUSTOM")),
                (AmdgpuPowerProfile::Capped as u32, String::from("CAPPED")),
                (
                    AmdgpuPowerProfile::Uncapped as u32,
                    String::from("UNCAPPED")
                )
            ]
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");

        let profiles = driver.get_available_power_profiles().await.expect("get");
        assert_eq!(
            profiles,
            &[
                (AmdgpuPowerProfile::Capped as u32, String::from("CAPPED")),
                (
                    AmdgpuPowerProfile::Uncapped as u32,
                    String::from("UNCAPPED")
                )
            ]
        );
    }

    #[tokio::test]
    async fn read_amdgpu_power_profile() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup_amdgpu().await.expect("setup_amdgpu");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 3          VIDEO*
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");
        assert_eq!(
            driver.get_power_profile().await.expect("get"),
            GpuPowerProfile::Amdgpu(AmdgpuPowerProfile::Video)
        );

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert_eq!(
            driver.get_power_profile().await.expect("get"),
            GpuPowerProfile::Amdgpu(AmdgpuPowerProfile::Video)
        );
    }

    #[tokio::test]
    async fn read_amdgpu_no_power_profile() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup_amdgpu().await.expect("setup_amdgpu");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 3          VIDEO
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");
        assert!(driver.get_power_profile().await.is_err());

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert!(driver.get_power_profile().await.is_err());
    }

    #[tokio::test]
    async fn read_amdgpu_unknown_power_profile() {
        let _h = testing::start();
        let driver = AmdgpuPowerProfileDriver {};

        setup_amdgpu().await.expect("setup_amdgpu");
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let filename = base.join(AmdgpuPowerProfileDriver::POWER_PROFILE_SUFFIX);
        create_dir_all(filename.parent().unwrap())
            .await
            .expect("create_dir_all");

        let contents = " 1 3D_FULL_SCREEN
 2            CGA*
 3          VIDEO
 4             VR
 5        COMPUTE
 6         CUSTOM
 8         CAPPED
 9       UNCAPPED";

        write(filename.as_path(), contents).await.expect("write");

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");
        assert!(driver.get_power_profile().await.is_err());

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert!(driver.get_power_profile().await.is_err());
    }

    #[tokio::test]
    async fn test_get_intel_i915_gpu_performance_level_auto() {
        let _h = testing::start();

        setup_intel_i915().await.expect("setup_intel_i915");

        let driver = IntelGpuPerformanceLevelDriver::new()
            .await
            .expect("Intel i915 driver creation");

        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.min_freq),
            "100",
        )
        .await
        .expect("write min_freq");
        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.max_freq),
            "1100",
        )
        .await
        .expect("write max_freq");

        let level = driver
            .get_performance_level()
            .await
            .expect("get performance level");

        assert_eq!(
            level,
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Auto)
        );
    }

    #[tokio::test]
    async fn test_get_intel_i915_gpu_performance_level_manual() {
        let _h = testing::start();

        setup_intel_i915().await.expect("setup_intel_i915");

        let driver = IntelGpuPerformanceLevelDriver::new()
            .await
            .expect("Intel i915 driver creation");

        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.min_freq),
            "600",
        )
        .await
        .expect("write min_freq");
        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.max_freq),
            "600",
        )
        .await
        .expect("write max_freq");

        let level = driver
            .get_performance_level()
            .await
            .expect("get performance level");

        assert_eq!(
            level,
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual)
        );
    }

    #[tokio::test]
    async fn test_set_intel_i915_gpu_performance_level() {
        let _h = testing::start();

        setup_intel_i915().await.expect("setup_intel_i915");

        let driver = IntelGpuPerformanceLevelDriver::new()
            .await
            .expect("Intel i915 driver creation");

        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.min_freq),
            "100",
        )
        .await
        .expect("write min_freq");
        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.max_freq),
            "1100",
        )
        .await
        .expect("write max_freq");

        driver
            .set_performance_level(GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual))
            .await
            .expect("set performance level");

        let min_freq = read_to_string(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.min_freq),
        )
        .await
        .expect("read min_freq")
        .trim()
        .to_string();
        let max_freq = read_to_string(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.max_freq),
        )
        .await
        .expect("read max_freq")
        .trim()
        .to_string();

        assert_eq!(
            min_freq, max_freq,
            "min_freq and max_freq should be equal in Manual mode"
        );
    }

    #[tokio::test]
    async fn test_get_intel_xe_gpu_performance_level_auto() {
        let _h = testing::start();

        setup_intel_xe().await.expect("setup_intel_xe");

        let driver = IntelGpuPerformanceLevelDriver::new()
            .await
            .expect("Intel Xe driver creation");

        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::XE.min_freq),
            "300",
        )
        .await
        .expect("write min_freq");
        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::XE.max_freq),
            "1200",
        )
        .await
        .expect("write max_freq");

        let level = driver
            .get_performance_level()
            .await
            .expect("get_performance_level");

        assert_eq!(
            level,
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Auto)
        );
    }

    #[tokio::test]
    async fn test_get_intel_xe_gpu_performance_level_manual() {
        let _h = testing::start();

        setup_intel_xe().await.expect("setup_intel_xe");

        let driver = IntelGpuPerformanceLevelDriver::new()
            .await
            .expect("Intel Xe driver creation");

        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::XE.min_freq),
            "800",
        )
        .await
        .expect("write min_freq");
        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::XE.max_freq),
            "800",
        )
        .await
        .expect("write max_freq");

        let level = driver
            .get_performance_level()
            .await
            .expect("get_performance_level");

        assert_eq!(
            level,
            GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual)
        );
    }

    #[tokio::test]
    async fn test_set_intel_xe_gpu_performance_level() {
        let _h = testing::start();

        setup_intel_xe().await.expect("setup_intel_xe");

        let driver = IntelGpuPerformanceLevelDriver::new()
            .await
            .expect("Intel Xe driver creation");

        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::XE.min_freq),
            "300",
        )
        .await
        .expect("write min_freq");
        write(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::XE.max_freq),
            "1200",
        )
        .await
        .expect("write max_freq");

        driver
            .set_performance_level(GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual))
            .await
            .expect("set_performance_level");

        let min_freq = read_to_string(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::XE.min_freq),
        )
        .await
        .expect("read min_freq")
        .trim()
        .to_string();
        let max_freq = read_to_string(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::XE.max_freq),
        )
        .await
        .expect("read max_freq")
        .trim()
        .to_string();

        assert_eq!(
            min_freq, max_freq,
            "min_freq and max_freq should be equal in Manual mode"
        );
    }

    #[tokio::test]
    async fn test_intel_gpu_clocks_range() {
        let _h = testing::start();

        setup_intel_i915().await.expect("setup_intel_i915");

        let driver = IntelGpuPerformanceLevelDriver::new()
            .await
            .expect("Intel driver creation");

        let range = driver.get_clocks_range().await.expect("get_clocks_range");
        assert_eq!(range, 100..=1100);

        let clocks = driver.get_clocks().await.expect("get_clocks");
        assert_eq!(clocks, 100);
    }

    #[tokio::test]
    async fn test_intel_gpu_set_clocks() {
        let _h = testing::start();

        setup_intel_i915().await.expect("setup_intel_i915");

        let driver = IntelGpuPerformanceLevelDriver::new()
            .await
            .expect("Intel driver creation");

        driver
            .set_performance_level(GpuPerformanceLevel::Intel(IntelPerformanceLevel::Manual))
            .await
            .expect("set manual mode");

        driver.set_clocks(800).await.expect("set_clocks");

        let min_freq = read_to_string(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.min_freq),
        )
        .await
        .expect("read min_freq")
        .trim()
        .parse::<u32>()
        .expect("parse min_freq");
        let max_freq = read_to_string(
            path(DRM_PREFIX)
                .join("card0")
                .join(IntelGpuConfig::I915.max_freq),
        )
        .await
        .expect("read max_freq")
        .trim()
        .parse::<u32>()
        .expect("parse max_freq");
        assert_eq!(min_freq, 800);
        assert_eq!(max_freq, 800);
    }
}
