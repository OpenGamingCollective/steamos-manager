/*
 * Copyright © 2026 Nilesh Chakraborty
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::Result;
use std::future::Future;
use tokio_stream::StreamExt;
use tracing::{debug, error, info};
use zbus::Connection;

use crate::Service;
use crate::gpu::{GpuPerformanceLevelDriverType, reset_amdgpu_dpm_on_resume};
use crate::hardware::device_config;
use crate::power::set_platform_profile;
use crate::systemd::LogindManagerProxy;

pub(crate) struct SleepResumeService {
    connection: Connection,
}

impl SleepResumeService {
    pub(crate) fn init(connection: Connection) -> Self {
        Self { connection }
    }

    async fn handle_resume(&self) -> Result<()> {
        info!("System resumed from sleep: restoring hardware power states");

        let config = device_config().await.unwrap_or(None);

        // 1. Ensure required platform profile is re-activated if configured
        if let Some(ref config) = config {
            if let Some(ref perf_config) = config.performance_profile {
                if let Err(e) = set_platform_profile(
                    &perf_config.platform_profile_name,
                    &perf_config.suggested_default,
                )
                .await
                {
                    debug!("Failed to restore platform profile on resume: {e}");
                }
            }
        }

        // 2. Clear uninitialized AMD GPU telemetry and 600MHz DPM clamp (skip on Intel / non-AMD)
        let is_amd_gpu = match config.as_ref().and_then(|c| c.gpu_performance.as_ref()) {
            Some(gpu_conf) => gpu_conf.driver == GpuPerformanceLevelDriverType::Amdgpu,
            None => true, // Fallback: probe sysfs if no explicit device configuration
        };

        if is_amd_gpu {
            if let Err(e) = reset_amdgpu_dpm_on_resume().await {
                debug!("AMD GPU DPM reset on resume: {e}");
            }
        }

        Ok(())
    }
}

impl Service for SleepResumeService {
    const NAME: &'static str = "SleepResumeService";

    fn run(&mut self) -> impl Future<Output = Result<()>> + Send {
        async move {
            let proxy = LogindManagerProxy::new(&self.connection).await?;
            let mut stream = proxy.receive_prepare_for_sleep().await?;
            info!("SleepResumeService listening for systemd-logind PrepareForSleep signals");

            while let Some(signal) = stream.next().await {
                match signal.args() {
                    Ok(args) if !args.start() => {
                        // start() == false indicates system resumed from sleep
                        if let Err(e) = self.handle_resume().await {
                            error!("Error handling post-resume hardware recovery: {e}");
                        }
                    }
                    Ok(_) => {
                        debug!("System preparing for sleep");
                    }
                    Err(e) => {
                        error!("Error parsing PrepareForSleep signal: {e}");
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::gpu::{AMDGPU_HWMON_NAME, AmdgpuPerformanceLevelDriver};
    use crate::hardware::{DeviceConfig, GpuPerformanceConfig, PerformanceProfileConfig};
    use crate::path;
    use crate::power::{PLATFORM_PROFILE_PREFIX, find_hwmon};
    use crate::testing;
    use crate::write_synced;
    use tokio::fs::{create_dir_all, read_to_string};

    #[tokio::test]
    async fn test_handle_resume_restores_platform_profile_and_dpm() {
        let mut handle = testing::start();
        let connection = handle.new_dbus().await.expect("new_dbus");

        let mut config = DeviceConfig::default();
        config.performance_profile = Some(PerformanceProfileConfig {
            platform_profile_name: String::from("platform-profile0"),
            suggested_default: String::from("performance"),
        });
        config.gpu_performance = Some(GpuPerformanceConfig {
            driver: GpuPerformanceLevelDriverType::Amdgpu,
            clocks: None,
        });
        handle.test.set_device_config(config).await;

        let profile_base = path(PLATFORM_PROFILE_PREFIX).join("platform-profile0");
        create_dir_all(&profile_base).await.unwrap();
        write_synced(profile_base.join("name"), b"platform-profile0\n")
            .await
            .unwrap();
        write_synced(profile_base.join("profile"), b"quiet\n")
            .await
            .unwrap();

        crate::gpu::test::setup_amdgpu().await.unwrap();
        let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
        let dpm_path = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);
        tokio::fs::write(&dpm_path, "low\n").await.unwrap();

        let service = SleepResumeService::init(connection);
        service.handle_resume().await.expect("handle_resume");

        // Profile should be restored to performance
        let current_profile = read_to_string(profile_base.join("profile"))
            .await
            .unwrap();
        assert_eq!(current_profile, "performance\n");

        // AMD GPU DPM should have cycled back to auto
        let dpm_level = read_to_string(&dpm_path).await.unwrap();
        assert_eq!(dpm_level, "auto");
    }

    #[tokio::test]
    async fn test_handle_resume_skips_amdgpu_on_intel_device() {
        let mut handle = testing::start();
        let connection = handle.new_dbus().await.expect("new_dbus");

        let mut config = DeviceConfig::default();
        config.performance_profile = Some(PerformanceProfileConfig {
            platform_profile_name: String::from("platform-profile0"),
            suggested_default: String::from("performance"),
        });
        config.gpu_performance = Some(GpuPerformanceConfig {
            driver: GpuPerformanceLevelDriverType::Intel,
            clocks: None,
        });
        handle.test.set_device_config(config).await;

        let profile_base = path(PLATFORM_PROFILE_PREFIX).join("platform-profile0");
        create_dir_all(&profile_base).await.unwrap();
        write_synced(profile_base.join("name"), b"platform-profile0\n")
            .await
            .unwrap();
        write_synced(profile_base.join("profile"), b"balanced\n")
            .await
            .unwrap();

        // Do not setup amdgpu; on Intel Arc / non-AMD device it should gracefully skip DPM reset
        let service = SleepResumeService::init(connection);
        service
            .handle_resume()
            .await
            .expect("handle_resume on Intel device");

        // Profile should still be restored to performance
        let current_profile = read_to_string(profile_base.join("profile"))
            .await
            .unwrap();
        assert_eq!(current_profile, "performance\n");
    }
}

