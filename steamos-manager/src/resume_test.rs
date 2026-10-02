/*
 * Copyright © 2026 Nilesh Chakraborty
 *
 * SPDX-License-Identifier: MIT
 */

use super::*;
use crate::gpu::{
    AMDGPU_HWMON_NAME, AmdgpuPerformanceLevelDriver, GpuPerformanceLevelDriverType,
    reset_amdgpu_dpm_on_resume,
};
use crate::hardware::{DeviceConfig, GpuPerformanceConfig, PerformanceProfileConfig};
use crate::path;
use crate::power::{PLATFORM_PROFILE_PREFIX, find_hwmon};
use crate::testing;
use crate::write_synced;
use tokio::fs::{create_dir_all, read_to_string, write};

#[tokio::test]
async fn test_reset_amdgpu_dpm_on_resume_cycles_performance_level() {
    let _h = testing::start();
    crate::gpu::test::setup_amdgpu().await.expect("setup_amdgpu");
    let base = find_hwmon(AMDGPU_HWMON_NAME).await.unwrap();
    let filename = base.join(AmdgpuPerformanceLevelDriver::PERFORMANCE_LEVEL_SUFFIX);
    write(filename.as_path(), "low\n").await.expect("write");

    reset_amdgpu_dpm_on_resume().await.expect("reset_amdgpu_dpm_on_resume");

    let level = read_to_string(filename.as_path()).await.expect("read");
    assert_eq!(level, "auto");
}

#[tokio::test]
async fn test_reset_amdgpu_dpm_on_resume_noop_without_amdgpu() {
    let _h = testing::start();
    // Do not setup amdgpu; verify that non-AMD systems (e.g. Intel Arc / Nvidia) gracefully succeed
    reset_amdgpu_dpm_on_resume()
        .await
        .expect("should gracefully no-op without amdgpu");
}

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
    write(&dpm_path, "low\n").await.unwrap();

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
