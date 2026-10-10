/*
 * Copyright © 2026 Nilesh Chakraborty
 *
 * SPDX-License-Identifier: MIT
 */

use super::*;
use crate::hardware::{
    DeviceConfig, FirmwareAttributeConfig, PerformanceProfileConfig, PerformanceProfileMethod,
    RangeConfig, TdpLimitConfig,
};
use crate::path;
use crate::testing;
use tokio::fs::{create_dir_all, read_to_string, write};

#[tokio::test]
async fn test_firmware_attribute_tdp_limiter_auto_activates_profile() {
    let mut h = testing::start();
    setup().await.expect("setup");

    let connection = h.new_dbus().await.expect("new_dbus");
    let mut config = DeviceConfig::default();
    config.tdp_limit = Some(TdpLimitConfig {
        method: TdpLimitingMethod::FirmwareAttribute,
        range: Some(RangeConfig { min: 3, max: 25 }),
        download_mode_limit: None,
        firmware_attribute: Some(FirmwareAttributeConfig {
            attribute: String::from("tdp0"),
            performance_profile: Some(String::from("performance")),
        }),
        performance_profile: None,
    });
    config.performance_profile = Some(PerformanceProfileConfig {
        method: PerformanceProfileMethod::PlatformProfile,
        platform_profile_name: Some(String::from("platform-profile0")),
        suggested_default: String::from("performance"),
    });
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
    write_synced(spl_base.join("max_value"), b"30\n")
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
    write_synced(platform_profile_base.join("profile"), b"balanced\n")
        .await
        .unwrap();

    let manager = tdp_limit_manager(&connection).await.unwrap();

    // While in 'balanced', is_active() is false because it requires 'performance'
    assert_eq!(manager.is_active().await.unwrap(), false);

    // Setting TDP limit must automatically switch profile to 'performance' and succeed
    manager.set_tdp_limit(20).await.unwrap();
    assert_eq!(manager.is_active().await.unwrap(), true);
    assert_eq!(manager.get_tdp_limit().await.unwrap(), 20);
    assert_eq!(
        read_to_string(platform_profile_base.join("profile"))
            .await
            .unwrap(),
        "performance"
    );
    assert_eq!(
        read_to_string(spl_base.join("current_value"))
            .await
            .unwrap(),
        "20"
    );
}

#[tokio::test]
async fn test_gpu_hwmon_tdp_limiter_auto_activates_profile() {
    let mut handle = testing::start();
    setup().await.expect("setup");

    let connection = handle.new_dbus().await.expect("new_dbus");
    let mut config = DeviceConfig::default();
    config.tdp_limit = Some(TdpLimitConfig {
        method: TdpLimitingMethod::AmdgpuHwmon,
        range: Some(RangeConfig { min: 3, max: 25 }),
        download_mode_limit: None,
        firmware_attribute: None,
        performance_profile: Some(String::from("performance")),
    });
    config.performance_profile = Some(PerformanceProfileConfig {
        method: PerformanceProfileMethod::PlatformProfile,
        platform_profile_name: Some(String::from("platform-profile0")),
        suggested_default: String::from("performance"),
    });
    handle.test.set_device_config(config).await;

    let hwmon = path(HWMON_PREFIX).join("hwmon5");
    write(hwmon.join(TDP_LIMIT1), "10000000\n").await.expect("write");

    let platform_profile_base = path(PLATFORM_PROFILE_PREFIX).join("platform-profile0");
    create_dir_all(&platform_profile_base).await.unwrap();
    write_synced(platform_profile_base.join("name"), b"platform-profile0\n")
        .await
        .unwrap();
    write_synced(platform_profile_base.join("profile"), b"quiet\n")
        .await
        .unwrap();

    let manager = tdp_limit_manager(&connection).await.unwrap();

    // While in 'quiet', is_active() is false
    assert_eq!(manager.is_active().await.unwrap(), false);

    // Setting TDP limit must automatically switch profile to 'performance' and succeed
    manager.set_tdp_limit(20).await.unwrap();
    assert_eq!(manager.is_active().await.unwrap(), true);
    assert_eq!(manager.get_tdp_limit().await.unwrap(), 20);
    assert_eq!(
        read_to_string(platform_profile_base.join("profile"))
            .await
            .unwrap(),
        "performance"
    );
    let power1_cap = read_to_string(hwmon.join(TDP_LIMIT1))
        .await
        .expect("power1_cap");
    assert_eq!(power1_cap, "20000000");
}
