/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 * Copyright © 2024 Igalia S.L.
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Error, Result};
use async_trait::async_trait;
use cecd_proxy::Config1Proxy;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::try_exists;
use tokio::select;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::{JoinHandle, spawn};
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};
use zbus::fdo::{self, DBusProxy};
use zbus::message::Header;
use zbus::names::{BusName, OwnedBusName, UniqueName};
use zbus::object_server::{Interface, SignalEmitter};
use zbus::proxy::{Builder, CacheProperties, PropertyStream};
use zbus::zvariant::{Fd, OwnedObjectPath};
use zbus::{Connection, ObjectServer, Proxy, interface, zvariant};

use steamos_manager_macros::{RemoteManager, remote};

use crate::cec::{CecdService, HdmiCecControl, HdmiCecState};
use crate::daemon::DaemonCommand;
use crate::daemon::user::Command;
use crate::error::{to_zbus_error, to_zbus_fdo_error, zbus_to_zbus_fdo};
use crate::gpu::{
    GpuPerformanceLevelDriver, GpuPowerProfileDriver, gpu_performance_level_driver,
    gpu_power_profile_driver,
};
use crate::hardware::{
    SteamDeckVariant, device_config, device_type, device_variant, steam_deck_variant,
};
use crate::inputplumber::InterceptModeService;
use crate::job::JobManagerCommand;
use crate::manager::root::{RootManagerProxy, SteamOSManagerSignals};
use crate::manager::{MANAGER_PATH, RemoteInterface, RemoteInterfaceConfig, RemoteOwner};
use crate::path;
use crate::platform::platform_config;
use crate::power::{
    BATTERY_DEFAULT_SUGGESTED_MINIMUM_LIMIT, CpuSchedulerManager, TdpManagerCommand,
    get_available_cpu_scaling_governors, get_available_platform_profiles, get_cpu_boost_state,
    get_cpu_scaling_governor, get_max_charge_level, get_platform_profile, register_tdp_limit1,
    unregister_tdp_limit1,
};
use crate::proxy::{
    BatteryChargeLimit1Proxy, CpuBoost1Proxy, FactoryReset1Proxy, FanControl1Proxy,
    GpuPerformanceLevel1Proxy, GpuPowerProfile1Proxy, PerformanceProfile1Proxy, Storage1Proxy,
    TdpLimit1Proxy, UpdateBios1Proxy, UpdateDock1Proxy,
};
use crate::screenreader::{OrcaManager, ScreenReaderAction, ScreenReaderMode};
use crate::session::{
    LoginMode, SessionManager, SessionManagerMessage, SessionManagerService, is_session_managed,
    valid_desktop_sessions,
};
use crate::wifi::{
    WifiBackend, get_wifi_backend, get_wifi_power_management_state, list_wifi_interfaces,
};
use crate::{SerialOrderValidator, Service};

macro_rules! method {
    ($self:expr, $method:expr, $($args:expr),+) => {
        $self.proxy
            .call($method, &($($args,)*))
            .await
            .map_err(zbus_to_zbus_fdo)
    };
    ($self:expr, $method:expr) => {
        $self.proxy
            .call($method, &())
            .await
            .map_err(zbus_to_zbus_fdo)
    };
}

macro_rules! job_method {
    ($self:expr, $method:expr, $($args:expr),+) => {
        {
            let (tx, rx) = oneshot::channel();
            $self.job_manager.send(JobManagerCommand::MirrorJob {
                connection: $self.proxy.connection().clone(),
                path: method!($self, $method, $($args),+)?,
                reply: tx,
            }).map_err(to_zbus_fdo_error)?;
            rx.await.map_err(to_zbus_fdo_error)?
        }
    };
    ($self:expr, $method:expr) => {
        {
            let (tx, rx) = oneshot::channel();
            $self.job_manager.send(JobManagerCommand::MirrorJob {
                connection: $self.proxy.connection().clone(),
                path: method!($self, $method)?,
                reply: tx,
            }).map_err(to_zbus_fdo_error)?;
            rx.await.map_err(to_zbus_fdo_error)?
        }
    };
}

macro_rules! getter {
    ($self:expr, $prop:expr) => {
        $self
            .proxy
            .get_property($prop)
            .await
            .map_err(zbus_to_zbus_fdo)
    };
}

macro_rules! setter {
    ($self:expr, $prop:expr, $value:expr) => {
        $self
            .proxy
            .set_property($prop, $value)
            .await
            .map_err(|e| zbus::Error::FDO(Box::new(e)))
    };
}

struct AmbientLightSensor1 {
    proxy: Proxy<'static>,
}

struct BatteryChargeLimit1 {
    proxy: Proxy<'static>,
    order: SerialOrderValidator,
}

struct CpuBoost1 {
    proxy: Proxy<'static>,
}

struct CpuScaling1 {
    proxy: Proxy<'static>,
    order: SerialOrderValidator,
}

struct CpuScheduler1 {
    proxy: Proxy<'static>,
    order: SerialOrderValidator,
}

struct FactoryReset1 {
    proxy: Proxy<'static>,
}

struct FanControl1 {
    proxy: Proxy<'static>,
}

struct FirmwareDebug1 {
    proxy: Proxy<'static>,
}

struct GpuPerformanceLevel1 {
    proxy: Proxy<'static>,
    driver: Box<dyn GpuPerformanceLevelDriver>,
    order: SerialOrderValidator,
}

struct GpuPowerProfile1 {
    proxy: Proxy<'static>,
    driver: Box<dyn GpuPowerProfileDriver>,
    order: SerialOrderValidator,
}

pub(crate) struct TdpLimit1 {
    pub manager: UnboundedSender<TdpManagerCommand>,
    pub order: SerialOrderValidator,
}

struct HdmiCec1 {
    hdmi_cec: Arc<Mutex<HdmiCecControl<'static>>>,
}

struct HdmiCec2 {
    hdmi_cec: Arc<Mutex<HdmiCecControl<'static>>>,
    manager: RootManagerProxy<'static>,
}

struct LowPowerMode1 {
    manager: UnboundedSender<TdpManagerCommand>,
}

struct Manager2 {
    proxy: Proxy<'static>,
    channel: Sender<Command>,
}

struct PerformanceProfile1 {
    proxy: Proxy<'static>,
    tdp_limit_manager: Option<UnboundedSender<TdpManagerCommand>>,
}

#[derive(RemoteManager)]
struct RemoteInterface1 {
    session: Connection,
    system: Connection,
    job_manager: UnboundedSender<JobManagerCommand>,

    #[remote]
    remote_battery_charge_limit1: Option<BatteryChargeLimit1RemoteOwner>,
    #[remote]
    remote_cpu_boost1: Option<CpuBoost1RemoteOwner>,
    #[remote]
    remote_factory_reset1: Option<FactoryReset1RemoteOwner>,
    #[remote]
    remote_fan_control1: Option<FanControl1RemoteOwner>,
    #[remote]
    remote_gpu_performance_level1: Option<GpuPerformanceLevel1RemoteOwner>,
    #[remote]
    remote_gpu_power_profile1: Option<GpuPowerProfile1RemoteOwner>,
    #[remote]
    remote_performance_profile1: Option<PerformanceProfile1RemoteOwner>,
    #[remote]
    remote_storage1: Option<Storage1RemoteOwner>,
    #[remote]
    remote_tdp_limit1: Option<TdpLimit1RemoteOwner>,
    #[context]
    context_tdp_limit1: Option<UnboundedSender<TdpManagerCommand>>,
    #[remote]
    remote_update_bios1: Option<UpdateBios1RemoteOwner>,
    #[remote]
    remote_update_deck1: Option<UpdateDock1RemoteOwner>,
}

struct ScreenReader0 {
    screen_reader: Arc<Mutex<OrcaManager<'static>>>,
}

struct ScreenReader1 {
    screen_reader: Arc<Mutex<OrcaManager<'static>>>,
}

struct SessionManagement1 {
    proxy: Proxy<'static>,
    manager: SessionManager,
}

struct Storage1 {
    proxy: Proxy<'static>,
    job_manager: UnboundedSender<JobManagerCommand>,
}

struct UpdateBios1 {
    proxy: Proxy<'static>,
    job_manager: UnboundedSender<JobManagerCommand>,
}

struct UpdateDock1 {
    proxy: Proxy<'static>,
    job_manager: UnboundedSender<JobManagerCommand>,
}

struct WifiBackend1 {
    proxy: Proxy<'static>,
}

struct WifiDebug1 {
    proxy: Proxy<'static>,
}

struct WifiDebugDump1 {
    proxy: Proxy<'static>,
}

struct WifiPowerManagement1 {
    proxy: Proxy<'static>,
}

pub(crate) struct SignalRelayService {
    proxy: Proxy<'static>,
    config1: Option<Config1Proxy<'static>>,
    session: Connection,
}

pub(crate) struct ScreenReaderSetupService {
    session: Connection,
    channel: broadcast::Receiver<SessionManagerMessage>,
}

#[interface(name = "com.steampowered.SteamOSManager1.AmbientLightSensor1")]
impl AmbientLightSensor1 {
    #[zbus(property(emits_changed_signal = "false"))]
    async fn als_calibration_gain(&self) -> fdo::Result<Vec<f64>> {
        getter!(self, "AlsCalibrationGain")
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.BatteryChargeLimit1")]
impl BatteryChargeLimit1 {
    #[zbus(property)]
    async fn max_charge_level(&self) -> fdo::Result<i32> {
        let level = get_max_charge_level().await.map_err(to_zbus_fdo_error)?;
        if level <= 0 { Ok(-1) } else { Ok(level) }
    }

    #[zbus(property)]
    async fn set_max_charge_level(
        &mut self,
        limit: i32,
        #[zbus(header)] header: Option<Header<'_>>,
    ) -> fdo::Result<()> {
        if header
            .map(|header| !self.order.check_header(header))
            .unwrap_or(true)
        {
            debug!("set MaxChargeLevel: discarding out of order serial");
            return Ok(());
        }
        Ok(self.proxy.call("SetMaxChargeLevel", &(limit)).await?)
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn suggested_minimum_limit(&self) -> i32 {
        let Ok(Some(config)) = device_config().await else {
            return BATTERY_DEFAULT_SUGGESTED_MINIMUM_LIMIT;
        };
        let Some(ref config) = config.battery_charge_limit else {
            return BATTERY_DEFAULT_SUGGESTED_MINIMUM_LIMIT;
        };
        config.suggested_minimum_limit
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.CpuBoost1")]
impl CpuBoost1 {
    #[zbus(property)]
    async fn cpu_boost_state(&self) -> fdo::Result<u32> {
        match get_cpu_boost_state().await {
            Ok(state) => Ok(state as u32),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[zbus(property)]
    async fn set_cpu_boost_state(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self
            .proxy
            .call("SetCpuBoostState", &(state))
            .await
            .map_err(to_zbus_fdo_error)?;
        self.cpu_boost_state_changed(&ctx).await
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.CpuScaling1")]
impl CpuScaling1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_cpu_scaling_governors(&self) -> fdo::Result<Vec<String>> {
        let governors = get_available_cpu_scaling_governors()
            .await
            .map_err(to_zbus_fdo_error)?;
        Ok(governors.into_iter().map(|g| g.to_string()).collect())
    }

    #[zbus(property)]
    async fn cpu_scaling_governor(&self) -> fdo::Result<String> {
        let governor = get_cpu_scaling_governor()
            .await
            .map_err(to_zbus_fdo_error)?;
        Ok(governor.to_string())
    }

    #[zbus(property)]
    async fn set_cpu_scaling_governor(
        &mut self,
        governor: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
        #[zbus(header)] header: Option<Header<'_>>,
    ) -> fdo::Result<()> {
        if header
            .map(|header| !self.order.check_header(header))
            .unwrap_or(true)
        {
            debug!("set CpuScalingGovernor: discarding out of order serial");
            return Ok(());
        }
        let _: () = self
            .proxy
            .call("SetCpuScalingGovernor", &(governor))
            .await?;
        Ok(self.cpu_scaling_governor_changed(&ctx).await?)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.CpuScheduler1")]
impl CpuScheduler1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_cpu_schedulers(&self) -> fdo::Result<Vec<String>> {
        getter!(self, "AvailableCpuSchedulers")
    }

    #[zbus(property)]
    async fn cpu_scheduler(&self) -> fdo::Result<String> {
        getter!(self, "CpuScheduler")
    }

    #[zbus(property)]
    async fn set_cpu_scheduler(
        &mut self,
        scheduler: String,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
        #[zbus(header)] header: Option<Header<'_>>,
    ) -> fdo::Result<()> {
        if header
            .map(|header| !self.order.check_header(header))
            .unwrap_or(true)
        {
            debug!("set CpuScheduler: discarding out of order serial");
            return Ok(());
        }
        let _: () = setter!(self, "CpuScheduler", scheduler)?;
        Ok(self.cpu_scheduler_changed(&ctx).await?)
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.FactoryReset1")]
impl FactoryReset1 {
    async fn prepare_factory_reset(&self, flags: u32) -> fdo::Result<u32> {
        method!(self, "PrepareFactoryReset", flags)
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.FanControl1")]
impl FanControl1 {
    #[zbus(property)]
    async fn fan_control_state(&self) -> fdo::Result<u32> {
        getter!(self, "FanControlState")
    }

    #[zbus(property)]
    async fn set_fan_control_state(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = setter!(self, "FanControlState", state)?;
        self.fan_control_state_changed(&ctx).await
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.FirmwareDebug1")]
impl FirmwareDebug1 {
    #[zbus(property)]
    async fn ec_logging(&self) -> fdo::Result<u32> {
        getter!(self, "EcLogging")
    }

    #[zbus(property)]
    async fn set_ec_logging(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = setter!(self, "EcLogging", state)?;
        self.ec_logging_changed(&ctx).await
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.GpuPerformanceLevel1")]
impl GpuPerformanceLevel1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_gpu_performance_levels(&self) -> fdo::Result<Vec<String>> {
        self.driver
            .get_available_performance_levels()
            .await
            .inspect_err(|message| error!("Error getting GPU performance levels: {message}"))
            .map(|levels| levels.into_iter().map(|level| level.to_string()).collect())
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn gpu_performance_level(&self) -> fdo::Result<String> {
        match self.driver.get_performance_level().await {
            Ok(level) => Ok(level.to_string()),
            Err(e) => {
                error!("Error getting GPU performance level: {e}");
                Err(to_zbus_fdo_error(e))
            }
        }
    }

    #[zbus(property)]
    async fn set_gpu_performance_level(
        &mut self,
        level: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
        #[zbus(header)] header: Option<Header<'_>>,
    ) -> fdo::Result<()> {
        if header
            .map(|header| !self.order.check_header(header))
            .unwrap_or(true)
        {
            debug!("set GpuPerformanceLevel: discarding out of order serial");
            return Ok(());
        }
        let _: () = self.proxy.call("SetGpuPerformanceLevel", &(level)).await?;
        Ok(self.gpu_performance_level_changed(&ctx).await?)
    }

    #[zbus(property)]
    async fn manual_gpu_clock(&self) -> fdo::Result<u32> {
        self.driver
            .get_clocks()
            .await
            .inspect_err(|message| error!("Error getting manual GPU clock: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_manual_gpu_clock(
        &mut self,
        clocks: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
        #[zbus(header)] header: Option<Header<'_>>,
    ) -> fdo::Result<()> {
        if header
            .map(|header| !self.order.check_header(header))
            .unwrap_or(true)
        {
            debug!("set ManualGpuClock: discarding out of order serial");
            return Ok(());
        }
        let _: () = self.proxy.call("SetManualGpuClock", &(clocks)).await?;
        Ok(self.manual_gpu_clock_changed(&ctx).await?)
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn manual_gpu_clock_min(&self) -> fdo::Result<u32> {
        Ok(*self
            .driver
            .get_clocks_range()
            .await
            .map_err(to_zbus_fdo_error)?
            .start())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn manual_gpu_clock_max(&self) -> fdo::Result<u32> {
        Ok(*self
            .driver
            .get_clocks_range()
            .await
            .map_err(to_zbus_fdo_error)?
            .end())
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.GpuPowerProfile1")]
impl GpuPowerProfile1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_gpu_power_profiles(&self) -> fdo::Result<Vec<String>> {
        let (_, names): (Vec<u32>, Vec<String>) = self
            .driver
            .get_available_power_profiles()
            .await
            .map_err(to_zbus_fdo_error)?
            .into_iter()
            .unzip();
        Ok(names)
    }

    #[zbus(property)]
    async fn gpu_power_profile(&self) -> fdo::Result<String> {
        match self.driver.get_power_profile().await {
            Ok(profile) => Ok(profile.to_string()),
            Err(e) => {
                error!("Error getting GPU power profile: {e}");
                Err(to_zbus_fdo_error(e))
            }
        }
    }

    #[zbus(property)]
    async fn set_gpu_power_profile(
        &mut self,
        profile: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
        #[zbus(header)] header: Option<Header<'_>>,
    ) -> fdo::Result<()> {
        if header
            .map(|header| !self.order.check_header(header))
            .unwrap_or(true)
        {
            debug!("set GpuPowerProfile: discarding out of order serial");
            return Ok(());
        }
        let _: () = self.proxy.call("SetGpuPowerProfile", &(profile)).await?;
        Ok(self.gpu_power_profile_changed(&ctx).await?)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.HdmiCec1")]
impl HdmiCec1 {
    #[zbus(property)]
    async fn hdmi_cec_state(&self) -> fdo::Result<u32> {
        match self.hdmi_cec.lock().await.get_enabled_state().await {
            Ok(state) => Ok(state as u32),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[zbus(property)]
    async fn set_hdmi_cec_state(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let state = match HdmiCecState::try_from(state) {
            Ok(state) => state,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string()).into()),
        };
        let _: () = self
            .hdmi_cec
            .lock()
            .await
            .set_enabled_state(state)
            .await
            .inspect_err(|message| error!("Error setting CEC state: {message}"))
            .map_err(to_zbus_error)?;
        self.hdmi_cec_state_changed(&ctx).await
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.HdmiCec2")]
impl HdmiCec2 {
    #[zbus(property)]
    async fn enable_control(&self) -> fdo::Result<bool> {
        self.hdmi_cec
            .lock()
            .await
            .get_enable_control()
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_enable_control(&self, enable: bool) -> fdo::Result<()> {
        self.hdmi_cec
            .lock()
            .await
            .set_enable_control(enable)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn suspend_tv(&self) -> fdo::Result<bool> {
        self.hdmi_cec
            .lock()
            .await
            .get_suspend_tv()
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_suspend_tv(&self, enable: bool) -> fdo::Result<()> {
        self.hdmi_cec
            .lock()
            .await
            .set_suspend_tv(enable)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn suspend_device(&self) -> fdo::Result<bool> {
        self.hdmi_cec
            .lock()
            .await
            .get_suspend_device()
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_suspend_device(&self, enable: bool) -> fdo::Result<()> {
        self.hdmi_cec
            .lock()
            .await
            .set_suspend_device(enable)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn wake_tv(&self) -> fdo::Result<bool> {
        self.hdmi_cec
            .lock()
            .await
            .get_wake_tv()
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_wake_tv(&self, enable: bool) -> fdo::Result<()> {
        self.hdmi_cec
            .lock()
            .await
            .set_wake_tv(enable)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn wake_device(&self) -> fdo::Result<bool> {
        if self
            .manager
            .hdmi_cec_can_awaken()
            .await
            .map_err(to_zbus_fdo_error)?
        {
            self.manager
                .hdmi_cec_awaken()
                .await
                .map_err(to_zbus_fdo_error)
        } else {
            Ok(false)
        }
    }

    #[zbus(property)]
    async fn set_wake_device(&self, enable: bool) -> fdo::Result<()> {
        self.manager
            .set_hdmi_cec_awaken(enable)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn wake_device_supported(&self) -> fdo::Result<bool> {
        self.manager
            .hdmi_cec_can_awaken()
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn make_active(&self) -> fdo::Result<()> {
        self.hdmi_cec
            .lock()
            .await
            .wake_tv()
            .await
            .map_err(to_zbus_fdo_error)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.LowPowerMode1")]
impl LowPowerMode1 {
    async fn enter_download_mode(&self, identifier: &str) -> fdo::Result<Fd<'_>> {
        let (tx, rx) = oneshot::channel();
        self.manager
            .send(TdpManagerCommand::EnterDownloadMode(
                identifier.to_string(),
                tx,
            ))
            .map_err(|_| {
                fdo::Error::Failed(String::from("Failed to obtain download mode handle"))
            })?;
        Ok(rx
            .await
            .map_err(to_zbus_fdo_error)?
            .map_err(to_zbus_fdo_error)?
            .ok_or(fdo::Error::Failed(String::from(
                "Download mode not configured",
            )))?
            .into())
    }

    async fn list_download_mode_handles(&self) -> fdo::Result<HashMap<String, u32>> {
        let (tx, rx) = oneshot::channel();
        self.manager
            .send(TdpManagerCommand::ListDownloadModeHandles(tx))
            .map_err(|_| {
                fdo::Error::Failed(String::from("Failed to obtain download mode handle list"))
            })?;
        rx.await.map_err(to_zbus_fdo_error)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.Manager2")]
impl Manager2 {
    async fn reload_config(&self) -> fdo::Result<()> {
        self.channel
            .send(DaemonCommand::ReadConfig)
            .await
            .inspect_err(|message| error!("Error sending ReadConfig command: {message}"))
            .map_err(to_zbus_fdo_error)?;
        method!(self, "ReloadConfig")
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn device_model(&self) -> fdo::Result<(String, String)> {
        let (device, variant) = device_variant().await.map_err(to_zbus_fdo_error)?;
        Ok((device.to_string(), variant))
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.PerformanceProfile1")]
impl PerformanceProfile1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_performance_profiles(&self) -> fdo::Result<Vec<String>> {
        let config = device_config().await.map_err(to_zbus_fdo_error)?;
        let config = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
            .ok_or(fdo::Error::Failed(String::from(
                "No performance platform-profile configured",
            )))?;
        get_available_platform_profiles(&config.platform_profile_name)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn performance_profile(&self) -> fdo::Result<String> {
        let config = device_config().await.map_err(to_zbus_fdo_error)?;
        let config = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
            .ok_or(fdo::Error::Failed(String::from(
                "No performance platform-profile configured",
            )))?;
        get_platform_profile(&config.platform_profile_name)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_performance_profile(
        &self,
        profile: &str,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self.proxy.call("SetPerformanceProfile", &(profile)).await?;
        self.performance_profile_changed(&ctx).await?;
        let connection = connection.clone();
        if let Some(manager) = self.tdp_limit_manager.as_ref() {
            let manager = manager.clone();
            let _ = manager.send(TdpManagerCommand::UpdateDownloadMode);
            spawn(async move {
                let (tx, rx) = oneshot::channel();
                manager.send(TdpManagerCommand::IsActive(tx))?;
                if rx.await?? {
                    let tdp_limit = TdpLimit1 {
                        manager,
                        order: SerialOrderValidator::default(),
                    };
                    connection
                        .object_server()
                        .at(MANAGER_PATH, tdp_limit)
                        .await?;
                } else {
                    connection
                        .object_server()
                        .remove::<TdpLimit1, _>(MANAGER_PATH)
                        .await?;
                }
                Ok::<(), Error>(())
            });
        }
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn suggested_default_performance_profile(&self) -> fdo::Result<String> {
        let config = device_config().await.map_err(to_zbus_fdo_error)?;
        let config = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
            .ok_or(fdo::Error::Failed(String::from(
                "No performance platform-profile configured",
            )))?;
        Ok(config.suggested_default.to_string())
    }
}

impl RemoteInterface1 {
    fn new(
        session: Connection,
        system: Connection,
        job_manager: UnboundedSender<JobManagerCommand>,
    ) -> RemoteInterface1 {
        RemoteInterface1 {
            session,
            system,
            job_manager,
            remote_battery_charge_limit1: None,
            remote_cpu_boost1: None,
            remote_factory_reset1: None,
            remote_fan_control1: None,
            remote_gpu_performance_level1: None,
            remote_gpu_power_profile1: None,
            remote_performance_profile1: None,
            remote_storage1: None,
            remote_tdp_limit1: None,
            context_tdp_limit1: None,
            remote_update_bios1: None,
            remote_update_deck1: None,
        }
    }
}

impl ScreenReader0 {
    async fn new(screen_reader: Arc<Mutex<OrcaManager<'static>>>) -> Result<ScreenReader0> {
        Ok(ScreenReader0 { screen_reader })
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.ScreenReader0")]
impl ScreenReader0 {
    #[zbus(property)]
    async fn enabled(&self) -> bool {
        self.screen_reader.lock().await.enabled()
    }

    #[zbus(property)]
    async fn set_enabled(&mut self, enabled: bool) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_enabled(enabled)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn rate(&self) -> f64 {
        self.screen_reader.lock().await.rate()
    }

    #[zbus(property)]
    async fn set_rate(&mut self, rate: f64) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_rate(rate)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn pitch(&self) -> f64 {
        self.screen_reader.lock().await.pitch()
    }

    #[zbus(property)]
    async fn set_pitch(&mut self, pitch: f64) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_pitch(pitch)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        self.screen_reader.lock().await.volume()
    }

    #[zbus(property)]
    async fn set_volume(&mut self, volume: f64) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_volume(volume)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn mode(&self) -> u32 {
        self.screen_reader.lock().await.mode() as u32
    }

    #[zbus(property)]
    async fn set_mode(
        &mut self,
        m: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let mode = match ScreenReaderMode::try_from(m) {
            Ok(mode) => mode,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string())),
        };
        self.screen_reader
            .lock()
            .await
            .set_mode(mode)
            .await
            .map_err(to_zbus_fdo_error)?;
        self.mode_changed(&ctx).await.map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn voice(&self) -> String {
        let guard = self.screen_reader.lock().await;
        guard.voice().to_owned()
    }

    #[zbus(property)]
    async fn set_voice(
        &mut self,
        voice: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_voice(voice)
            .await
            .map_err(to_zbus_fdo_error)?;
        self.voice_changed(&ctx).await.map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn voice_locales(&self) -> Vec<String> {
        let guard = self.screen_reader.lock().await;
        guard
            .get_voice_locales()
            .iter()
            .map(ToString::to_string) // clone each &str into a String
            .collect::<Vec<String>>()
    }

    #[zbus(property)]
    async fn voices_for_locale(&self) -> HashMap<String, Vec<String>> {
        self.screen_reader.lock().await.get_voices_map().clone()
    }

    async fn trigger_action(&mut self, a: u32, timestamp: u64) -> fdo::Result<()> {
        let action = match ScreenReaderAction::try_from(a) {
            Ok(action) => action,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string())),
        };
        self.screen_reader
            .lock()
            .await
            .trigger_action(action, timestamp)
            .await
            .map_err(to_zbus_fdo_error)
    }
}

impl ScreenReader1 {
    async fn new(screen_reader: Arc<Mutex<OrcaManager<'static>>>) -> Result<ScreenReader1> {
        Ok(ScreenReader1 { screen_reader })
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.ScreenReader1")]
impl ScreenReader1 {
    #[zbus(property)]
    async fn enabled(&self) -> bool {
        self.screen_reader.lock().await.enabled()
    }

    #[zbus(property)]
    async fn set_enabled(&mut self, enabled: bool) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_enabled(enabled)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn rate(&self) -> f64 {
        self.screen_reader.lock().await.rate()
    }

    #[zbus(property)]
    async fn set_rate(&mut self, rate: f64) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_rate(rate)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn pitch(&self) -> f64 {
        self.screen_reader.lock().await.pitch()
    }

    #[zbus(property)]
    async fn set_pitch(&mut self, pitch: f64) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_pitch(pitch)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        self.screen_reader.lock().await.volume()
    }

    #[zbus(property)]
    async fn set_volume(&mut self, volume: f64) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_volume(volume)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn mode(&self) -> String {
        self.screen_reader.lock().await.mode().to_string()
    }

    #[zbus(property)]
    async fn set_mode(
        &mut self,
        m: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let mode = match ScreenReaderMode::try_from(m) {
            Ok(mode) => mode,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string())),
        };
        self.screen_reader
            .lock()
            .await
            .set_mode(mode)
            .await
            .map_err(to_zbus_fdo_error)?;
        self.mode_changed(&ctx).await.map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn voice_locale(&self) -> String {
        let guard = self.screen_reader.lock().await;
        guard.locale().to_owned()
    }

    #[zbus(property)]
    async fn set_voice_locale(
        &mut self,
        locale: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_locale(locale)
            .map_err(to_zbus_fdo_error)?;
        self.voice_locale_changed(&ctx)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn voice_locales(&self) -> Vec<String> {
        let guard = self.screen_reader.lock().await;
        guard
            .get_voice_locales()
            .iter()
            .map(ToString::to_string) // clone each &str into a String
            .collect::<Vec<String>>()
    }

    #[zbus(property)]
    async fn voice(&self) -> String {
        let guard = self.screen_reader.lock().await;
        guard.voice().to_owned()
    }

    #[zbus(property)]
    async fn set_voice(
        &mut self,
        voice: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.screen_reader
            .lock()
            .await
            .set_voice(voice)
            .await
            .map_err(to_zbus_fdo_error)?;
        self.voice_changed(&ctx).await.map_err(to_zbus_fdo_error)
    }

    async fn get_voices_for_locale(&self, locale: &str) -> fdo::Result<Vec<String>> {
        let guard = self.screen_reader.lock().await;
        let map = guard.get_voices_map();
        Ok(map.get(locale).cloned().unwrap_or_default())
    }

    async fn get_voices(&self) -> fdo::Result<Vec<String>> {
        let guard = self.screen_reader.lock().await;
        Ok(guard.get_voices())
    }

    async fn trigger_action(&mut self, a: &str, timestamp: u64) -> fdo::Result<()> {
        let action = match ScreenReaderAction::try_from(a) {
            Ok(action) => action,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string())),
        };
        self.screen_reader
            .lock()
            .await
            .trigger_action(action, timestamp)
            .await
            .map_err(to_zbus_fdo_error)
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.SessionManagement1")]
impl SessionManagement1 {
    #[zbus(property)]
    async fn default_login_mode(&self) -> fdo::Result<String> {
        Ok(self
            .manager
            .default_login_mode()
            .await
            .map_err(to_zbus_fdo_error)?
            .to_string())
    }

    #[zbus(property)]
    async fn set_default_login_mode(&mut self, login_mode: &str) -> fdo::Result<()> {
        let login_mode = LoginMode::try_from(login_mode).map_err(to_zbus_fdo_error)?;
        self.manager
            .set_default_login_mode(login_mode)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn default_desktop_session(&self) -> fdo::Result<String> {
        self.manager
            .default_desktop_session()
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_default_desktop_session(&mut self, session: &str) -> fdo::Result<()> {
        self.manager
            .set_default_desktop_session(session)
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn switch_to_login_mode(&self, login_mode: &str) -> fdo::Result<()> {
        let login_mode = LoginMode::try_from(login_mode).map_err(to_zbus_fdo_error)?;
        self.manager
            .switch_to_login_mode(login_mode)
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn switch_to_game_mode(&self) -> fdo::Result<()> {
        self.manager
            .switch_to_login_mode(LoginMode::Game)
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn switch_to_desktop_mode(&self) -> fdo::Result<()> {
        self.manager
            .switch_to_login_mode(LoginMode::Desktop)
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn switch_to_desktop_session(&self, session: &str) -> fdo::Result<()> {
        self.manager
            .switch_to_desktop_session(session)
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn valid_desktop_sessions(&self) -> fdo::Result<Vec<String>> {
        valid_desktop_sessions().await.map_err(to_zbus_fdo_error)
    }

    async fn clean_temporary_sessions(&self) -> fdo::Result<()> {
        method!(self, "CleanTemporarySessions")
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.Storage1")]
impl Storage1 {
    async fn format_device(
        &mut self,
        device: &str,
        label: &str,
        validate: bool,
    ) -> fdo::Result<OwnedObjectPath> {
        job_method!(self, "FormatDevice", device, label, validate)
    }

    async fn trim_devices(&mut self) -> fdo::Result<OwnedObjectPath> {
        job_method!(self, "TrimDevices")
    }
}

#[remote(
    name = "com.steampowered.SteamOSManager1.TdpLimit1",
    register = register_tdp_limit1,
    unregister = unregister_tdp_limit1,
    context = Option<UnboundedSender<TdpManagerCommand>>,
)]
impl TdpLimit1 {
    #[zbus(property)]
    async fn tdp_limit(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self
            .manager
            .send(TdpManagerCommand::GetTdpLimit(tx))
            .is_err()
        {
            return 0;
        }
        rx.await.unwrap_or(Ok(0)).unwrap_or(0)
    }

    #[zbus(property)]
    async fn set_tdp_limit(
        &mut self,
        limit: u32,
        #[zbus(header)] header: Option<Header<'_>>,
    ) -> fdo::Result<()> {
        if header
            .map(|header| !self.order.check_header(header))
            .unwrap_or(true)
        {
            debug!("set TdpLimit: discarding out of order serial");
            return Ok(());
        }
        self.manager
            .send(TdpManagerCommand::SetTdpLimit(limit))
            .map_err(|_| fdo::Error::Failed(String::from("Failed to set TDP limit")))
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn tdp_limit_min(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self
            .manager
            .send(TdpManagerCommand::GetTdpLimitRange(tx))
            .is_err()
        {
            return 0;
        }
        if let Ok(range) = rx.await {
            range.map(|r| *r.start()).unwrap_or(0)
        } else {
            0
        }
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn tdp_limit_max(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self
            .manager
            .send(TdpManagerCommand::GetTdpLimitRange(tx))
            .is_err()
        {
            return 0;
        }
        if let Ok(range) = rx.await {
            range.map(|r| *r.end()).unwrap_or(0)
        } else {
            0
        }
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.UpdateBios1")]
impl UpdateBios1 {
    async fn update_bios(&mut self) -> fdo::Result<OwnedObjectPath> {
        job_method!(self, "UpdateBios")
    }
}

#[remote(name = "com.steampowered.SteamOSManager1.UpdateDock1")]
impl UpdateDock1 {
    async fn update_dock(&mut self) -> fdo::Result<OwnedObjectPath> {
        job_method!(self, "UpdateDock")
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.WifiBackend1")]
impl WifiBackend1 {
    #[zbus(property)]
    async fn wifi_backend(&self) -> fdo::Result<String> {
        match get_wifi_backend().await {
            Ok(backend) => Ok(backend.to_string()),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[cfg(feature = "wifi-backend-switching")]
    #[zbus(property)]
    async fn set_wifi_backend(
        &self,
        backend: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let backend = match WifiBackend::try_from(backend) {
            Ok(backend) => backend,
            Err(e) => return Err(fdo::Error::InvalidArgs(e.to_string()).into()),
        };
        let _: () = self.proxy.call("SetWifiBackend", &(backend as u32)).await?;
        self.wifi_backend_changed(&ctx).await
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.WifiDebug1")]
impl WifiDebug1 {
    #[zbus(property)]
    async fn wifi_debug_mode_state(&self) -> fdo::Result<u32> {
        getter!(self, "WifiDebugModeState")
    }

    async fn set_wifi_debug_mode(
        &self,
        mode: u32,
        options: HashMap<&str, zvariant::Value<'_>>,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let _: () = method!(self, "SetWifiDebugMode", mode, options)?;
        self.wifi_debug_mode_state_changed(&ctx)
            .await
            .map_err(zbus_to_zbus_fdo)?;
        Ok(())
    }

    #[zbus(property)]
    async fn wifi_backend(&self) -> fdo::Result<String> {
        match get_wifi_backend().await {
            Ok(backend) => Ok(backend.to_string()),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[cfg(feature = "wifi-backend-switching")]
    #[zbus(property)]
    async fn set_wifi_backend(
        &self,
        backend: &str,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let backend = match WifiBackend::try_from(backend) {
            Ok(backend) => backend,
            Err(e) => return Err(fdo::Error::InvalidArgs(e.to_string()).into()),
        };
        let _: () = self.proxy.call("SetWifiBackend", &(backend as u32)).await?;
        self.wifi_backend_changed(&ctx).await
    }

    async fn capture_debug_trace_output(&self) -> fdo::Result<String> {
        method!(self, "CaptureDebugTraceOutput")
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.WifiDebugDump1")]
impl WifiDebugDump1 {
    async fn generate_debug_dump(&self) -> fdo::Result<String> {
        method!(self, "GenerateDebugDump")
    }
}

#[interface(name = "com.steampowered.SteamOSManager1.WifiPowerManagement1")]
impl WifiPowerManagement1 {
    #[zbus(property)]
    async fn wifi_power_management_state(&self) -> fdo::Result<u32> {
        match get_wifi_power_management_state().await {
            Ok(state) => Ok(state as u32),
            Err(e) => Err(to_zbus_fdo_error(e)),
        }
    }

    #[zbus(property)]
    async fn set_wifi_power_management_state(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let _: () = self
            .proxy
            .call("SetWifiPowerManagementState", &(state))
            .await?;
        self.wifi_power_management_state_changed(&ctx).await
    }
}

async fn register<I: RemoteInterface + Interface>(
    object_server: &ObjectServer,
    remote: I::Remote,
) -> fdo::Result<bool> {
    if object_server.interface::<_, I>(MANAGER_PATH).await.is_ok() {
        return Ok(false);
    }
    if object_server
        .interface::<_, <I as RemoteInterface>::Remote>(MANAGER_PATH)
        .await
        .is_ok()
    {
        return Ok(false);
    }

    Ok(object_server.at(MANAGER_PATH, remote).await?)
}

impl Service for SignalRelayService {
    const NAME: &'static str = "signal-relay";

    async fn run(&mut self) -> Result<()> {
        let mut battery_charge_limit1 = self
            .session
            .object_server()
            .interface::<_, BatteryChargeLimit1>(MANAGER_PATH)
            .await
            .ok();
        let mut hdmi_cec1 = self
            .session
            .object_server()
            .interface::<_, HdmiCec1>(MANAGER_PATH)
            .await
            .ok();
        let mut hdmi_cec2 = self
            .session
            .object_server()
            .interface::<_, HdmiCec2>(MANAGER_PATH)
            .await
            .ok();

        let mut max_charge_level_changed =
            self.proxy.receive_signal("MaxChargeLevelChanged").await?;

        let mut wake_tv_changed;
        let mut suspend_tv_changed;
        let mut allow_standby_changed;
        if let Some(config1) = self.config1.as_ref() {
            wake_tv_changed = Some(config1.receive_wake_tv_changed().await);
            suspend_tv_changed = Some(config1.receive_suspend_tv_changed().await);
            allow_standby_changed = Some(config1.receive_allow_standby_changed().await);
        } else {
            wake_tv_changed = None;
            suspend_tv_changed = None;
            allow_standby_changed = None;
        }

        loop {
            select! {
                _ = max_charge_level_changed.next() => {
                    if let Some(battery_charge_limit1) = battery_charge_limit1.as_mut() {
                        battery_charge_limit1.signal_emitter().max_charge_level_changed().await?;
                    }
                },
                Some(_) = SignalRelayService::expect(wake_tv_changed.as_mut()) => {
                    if let Some(hdmi_cec1) = hdmi_cec1.as_mut() {
                        let emitter = hdmi_cec1.signal_emitter();
                        hdmi_cec1.get().await.hdmi_cec_state_changed(emitter).await?;
                    }
                    if let Some(hdmi_cec2) = hdmi_cec2.as_mut() {
                        let emitter = hdmi_cec2.signal_emitter();
                        hdmi_cec2.get().await.wake_tv_changed(emitter).await?;
                    }
                },
                Some(_) = SignalRelayService::expect(suspend_tv_changed.as_mut()) => {
                    if let Some(hdmi_cec1) = hdmi_cec1.as_mut() {
                        let emitter = hdmi_cec1.signal_emitter();
                        hdmi_cec1.get().await.hdmi_cec_state_changed(emitter).await?;
                    }
                    if let Some(hdmi_cec2) = hdmi_cec2.as_mut() {
                        let emitter = hdmi_cec2.signal_emitter();
                        hdmi_cec2.get().await.suspend_tv_changed(emitter).await?;
                    }
                },
                Some(_) = SignalRelayService::expect(allow_standby_changed.as_mut()) => {
                    if let Some(hdmi_cec1) = hdmi_cec1.as_mut() {
                        let emitter = hdmi_cec1.signal_emitter();
                        hdmi_cec1.get().await.hdmi_cec_state_changed(emitter).await?;
                    }
                    if let Some(hdmi_cec2) = hdmi_cec2.as_mut() {
                        let emitter = hdmi_cec2.signal_emitter();
                        hdmi_cec2.get().await.suspend_device_changed(emitter).await?;
                    }
                },
            }
        }
    }
}

impl SignalRelayService {
    async fn expect<'a, T>(
        signal: Option<&mut PropertyStream<'a, T>>,
    ) -> Option<<PropertyStream<'a, T> as tokio_stream::Stream>::Item>
    where
        PropertyStream<'a, T>: StreamExt,
        T: Unpin,
    {
        if let Some(signal) = signal {
            signal.next().await
        } else {
            // XXX: Is there a better way to handle this?
            sleep(Duration::from_secs(3600 * 24)).await;
            None
        }
    }
}

impl Service for ScreenReaderSetupService {
    const NAME: &'static str = "screenreader-setup";

    async fn run(&mut self) -> Result<()> {
        if !try_exists(path("/usr/bin/orca")).await? {
            return Ok(());
        }

        let object_server = self.session.object_server();
        let orca_manager = OrcaManager::new(&self.session).await?;
        let screen_reader = Arc::new(Mutex::new(orca_manager));

        loop {
            match self.channel.recv().await {
                Ok(SessionManagerMessage::LoginModeChanged(LoginMode::Game)) => {
                    if object_server
                        .interface::<_, ScreenReader0>(MANAGER_PATH)
                        .await
                        .is_ok()
                        || object_server
                            .interface::<_, ScreenReader1>(MANAGER_PATH)
                            .await
                            .is_ok()
                    {
                        continue;
                    }
                    let screen_reader0 = ScreenReader0::new(screen_reader.clone()).await?;
                    let screen_reader1 = ScreenReader1::new(screen_reader.clone()).await?;

                    object_server.at(MANAGER_PATH, screen_reader0).await?;
                    object_server.at(MANAGER_PATH, screen_reader1).await?;
                }
                Ok(SessionManagerMessage::LoginModeChanged(LoginMode::Desktop)) => {
                    let _ = screen_reader
                        .lock()
                        .await
                        .stop_orca()
                        .await
                        .inspect_err(|err| {
                            warn!("Could not stop orca when entering desktop mode: {err}")
                        });
                    let _ = object_server.remove::<ScreenReader0, _>(MANAGER_PATH).await;
                    let _ = object_server.remove::<ScreenReader1, _>(MANAGER_PATH).await;
                }
                Err(RecvError::Closed) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

async fn create_platform_interfaces(
    proxy: &Proxy<'static>,
    object_server: &ObjectServer,
    connection: &Connection,
    job_manager: &UnboundedSender<JobManagerCommand>,
) -> Result<()> {
    let Some(config) = platform_config().await? else {
        return Ok(());
    };

    let factory_reset = FactoryReset1 {
        proxy: proxy.clone(),
    };
    let fan_control = FanControl1 {
        proxy: proxy.clone(),
    };
    let storage = Storage1 {
        proxy: proxy.clone(),
        job_manager: job_manager.clone(),
    };
    let update_bios = UpdateBios1 {
        proxy: proxy.clone(),
        job_manager: job_manager.clone(),
    };
    let update_dock = UpdateDock1 {
        proxy: proxy.clone(),
        job_manager: job_manager.clone(),
    };

    if let Some(config) = config.factory_reset.as_ref() {
        match config.is_valid(true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, factory_reset).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if factory reset config is valid: {e}"),
        }
    }

    if let Some(config) = config.fan_control.as_ref() {
        match config.is_valid(connection, true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, fan_control).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if fan control config is valid: {e}"),
        }
    }

    if let Some(config) = config.storage.as_ref() {
        match config.is_valid(true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, storage).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if storage config is valid: {e}"),
        }
    }

    if let Some(config) = config.update_bios.as_ref() {
        match config.is_valid(true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, update_bios).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if BIOS update config is valid: {e}"),
        }
    }

    if let Some(config) = config.update_dock.as_ref() {
        match config.is_valid(true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, update_dock).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if dock update config is valid: {e}"),
        }
    }

    Ok(())
}

async fn create_device_interfaces(
    proxy: &Proxy<'static>,
    object_server: &ObjectServer,
    system: &Connection,
    tdp_manager: Option<UnboundedSender<TdpManagerCommand>>,
) -> Result<()> {
    let Some(config) = device_config().await? else {
        return Ok(());
    };

    let performance_profile = PerformanceProfile1 {
        proxy: proxy.clone(),
        tdp_limit_manager: tdp_manager.clone(),
    };
    let firmware_debug = FirmwareDebug1 {
        proxy: proxy.clone(),
    };

    if let Some(manager) = tdp_manager {
        let low_power_mode = LowPowerMode1 {
            manager: manager.clone(),
        };
        if config
            .tdp_limit
            .as_ref()
            .and_then(|config| config.download_mode_limit)
            .is_some()
        {
            object_server.at(MANAGER_PATH, low_power_mode).await?;
        }

        let object_server = object_server.clone();
        spawn(async move {
            let (tx, rx) = oneshot::channel();
            manager.send(TdpManagerCommand::IsActive(tx))?;
            if rx.await?? {
                let tdp_limit = TdpLimit1 {
                    manager,
                    order: SerialOrderValidator::default(),
                };
                object_server.at(MANAGER_PATH, tdp_limit).await?;
            }
            Ok::<(), Error>(())
        });
    }

    if let Some(config) = config.performance_profile.as_ref()
        && !get_available_platform_profiles(&config.platform_profile_name)
            .await
            .unwrap_or_default()
            .is_empty()
    {
        object_server.at(MANAGER_PATH, performance_profile).await?;
    }

    if let Some(config) = config.firmware_debug.as_ref() {
        match config.is_valid(system, true).await {
            Ok(true) => {
                object_server.at(MANAGER_PATH, firmware_debug).await?;
            }
            Ok(false) => (),
            Err(e) => error!("Failed to verify if firmware debug config is valid: {e}"),
        }
    }

    Ok(())
}

pub(crate) struct UserServices {
    pub signal_relay: SignalRelayService,
    pub session_manager: Option<SessionManagerService>,
    pub screenreader_setup: Option<ScreenReaderSetupService>,
    pub cecd: Option<CecdService>,
    pub intercept_mode: Option<InterceptModeService>,
}

pub(crate) async fn create_interfaces(
    session: Connection,
    system: Connection,
    daemon: Sender<Command>,
    job_manager: UnboundedSender<JobManagerCommand>,
    tdp_manager: Option<UnboundedSender<TdpManagerCommand>>,
) -> Result<UserServices> {
    let proxy = Builder::<Proxy>::new(&system)
        .destination("com.steampowered.SteamOSManager1")?
        .path("/com/steampowered/SteamOSManager1")?
        .interface("com.steampowered.SteamOSManager1.RootManager")?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let root_manager = RootManagerProxy::new(&system).await?;

    job_manager.send(JobManagerCommand::MirrorConnection(system.clone()))?;

    let als = AmbientLightSensor1 {
        proxy: proxy.clone(),
    };
    let battery_charge_limit = BatteryChargeLimit1 {
        proxy: proxy.clone(),
        order: SerialOrderValidator::default(),
    };
    let cpu_boost = CpuBoost1 {
        proxy: proxy.clone(),
    };
    let cpu_scaling = CpuScaling1 {
        proxy: proxy.clone(),
        order: SerialOrderValidator::default(),
    };
    let cpu_scheduler = CpuScheduler1 {
        proxy: proxy.clone(),
        order: SerialOrderValidator::default(),
    };

    let hdmi_cec = HdmiCecControl::new(&session).await;
    let cecd_service;
    let hdmi_cec_config;
    match hdmi_cec {
        Ok(ref hdmi_cec) => {
            cecd_service = CecdService::new(&system, hdmi_cec).await.ok();
            hdmi_cec_config = Config1Proxy::new(&session).await.ok();
        }
        Err(ref e) => {
            info!("Could not start CEC service: {e}");
            cecd_service = None;
            hdmi_cec_config = None;
        }
    };

    let manager2 = Manager2 {
        proxy: proxy.clone(),
        channel: daemon.clone(),
    };

    let mut remote_interface =
        RemoteInterface1::new(session.clone(), system.clone(), job_manager.clone());
    let remote_config = RemoteInterface1Config::load().await?;

    let session_management = SessionManagement1 {
        proxy: proxy.clone(),
        manager: SessionManager::new(session.clone(), &system, daemon).await?,
    };
    let wifi_backend = WifiBackend1 {
        proxy: proxy.clone(),
    };
    let wifi_power_management = WifiPowerManagement1 {
        proxy: proxy.clone(),
    };

    let object_server = session.object_server();

    if let Err(e) =
        create_device_interfaces(&proxy, object_server, &system, tdp_manager.clone()).await
    {
        error!("Failed to initalize device-specific interfaces: {e}");
    }

    if let Err(e) = create_platform_interfaces(&proxy, object_server, &system, &job_manager).await {
        error!("Failed to initalize platform-specific interfaces: {e}");
    }

    if device_type().await.unwrap_or_default() == "steam_deck" {
        object_server.at(MANAGER_PATH, als).await?;
    }
    object_server.at(MANAGER_PATH, wifi_backend).await?;
    if steam_deck_variant().await.unwrap_or_default() == SteamDeckVariant::Galileo {
        let wifi_debug = WifiDebug1 {
            proxy: proxy.clone(),
        };
        let wifi_debug_dump = WifiDebugDump1 {
            proxy: proxy.clone(),
        };
        object_server.at(MANAGER_PATH, wifi_debug).await?;
        object_server.at(MANAGER_PATH, wifi_debug_dump).await?;
    }

    if get_max_charge_level().await.is_ok() {
        object_server.at(MANAGER_PATH, battery_charge_limit).await?;
    }

    if get_cpu_boost_state().await.is_ok() {
        object_server.at(MANAGER_PATH, cpu_boost).await?;
    }

    object_server.at(MANAGER_PATH, cpu_scaling).await?;
    if CpuSchedulerManager::is_supported().await? {
        object_server.at(MANAGER_PATH, cpu_scheduler).await?;
    }

    match gpu_performance_level_driver().await {
        Ok(driver) => {
            object_server
                .at(
                    MANAGER_PATH,
                    GpuPerformanceLevel1 {
                        proxy: proxy.clone(),
                        driver,
                        order: SerialOrderValidator::default(),
                    },
                )
                .await?;
        }
        Err(e) => warn!("Can't add GpuPerformanceLevel1 interface: {e}"),
    }

    match gpu_power_profile_driver().await {
        Ok(driver) => {
            object_server
                .at(
                    MANAGER_PATH,
                    GpuPowerProfile1 {
                        proxy: proxy.clone(),
                        driver,
                        order: SerialOrderValidator::default(),
                    },
                )
                .await?;
        }
        Err(e) => warn!("Can't add GpuPowerProfile1 interface: {e}"),
    }

    if let Ok(hdmi_cec) = hdmi_cec {
        let hdmi_cec = Arc::new(Mutex::new(hdmi_cec));
        object_server
            .at(
                MANAGER_PATH,
                HdmiCec1 {
                    hdmi_cec: hdmi_cec.clone(),
                },
            )
            .await?;
        object_server
            .at(
                MANAGER_PATH,
                HdmiCec2 {
                    hdmi_cec,
                    manager: root_manager.clone(),
                },
            )
            .await?;
    }

    object_server.at(MANAGER_PATH, manager2).await?;

    let mut session_manager_service = None;
    let mut screenreader_setup_service = None;
    let mut intercept_mode_service = None;
    if is_session_managed().await? {
        match session_management.manager.create_service().await {
            Ok((service, channel)) => {
                session_manager_service = Some(service);
                intercept_mode_service = Some(InterceptModeService {
                    manager: root_manager.clone(),
                    channel: channel.resubscribe(),
                });
                screenreader_setup_service = Some(ScreenReaderSetupService {
                    session: session.clone(),
                    channel,
                });
            }
            Err(e)
                if matches!(
                    e.downcast_ref::<fdo::Error>(),
                    Some(fdo::Error::UnknownObject(_))
                ) => {}
            Err(e) => match e.downcast_ref::<zbus::Error>() {
                Some(zbus::Error::FDO(e)) if matches!(**e, fdo::Error::UnknownObject(_)) => {}
                Some(zbus::Error::MethodError(name, _, _))
                    if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject" => {}
                _ => {
                    error!(
                        "Could not set up session management service; screen reader will not work: {e}"
                    );
                }
            },
        }
        object_server.at(MANAGER_PATH, session_management).await?;
    }

    if !list_wifi_interfaces().await.unwrap_or_default().is_empty() {
        object_server
            .at(MANAGER_PATH, wifi_power_management)
            .await?;
    }

    remote_interface.context_tdp_limit1 = tdp_manager;
    remote_interface.configure(&remote_config).await?;
    object_server.at(MANAGER_PATH, remote_interface).await?;

    Ok(UserServices {
        signal_relay: SignalRelayService {
            proxy,
            session: session.clone(),
            config1: hdmi_cec_config,
        },
        session_manager: session_manager_service,
        screenreader_setup: screenreader_setup_service,
        cecd: cecd_service,
        intercept_mode: intercept_mode_service,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::daemon::channel;
    use crate::daemon::user::{UserCommand, UserContext};
    use crate::gpu::test::Nodes as GpuNodes;
    use crate::gpu::{GpuPerformanceLevelDriverType, GpuPowerProfileDriverType};
    use crate::hardware::test::fake_model;
    use crate::hardware::{
        BatteryChargeLimitConfig, DeviceConfig, DeviceMatch, DmiMatch, FanSpeedConfig,
        GpuPerformanceConfig, GpuPowerProfileConfig, PerformanceProfileConfig, RangeConfig,
        SteamDeckVariant, TdpLimitConfig,
    };
    use crate::platform::{
        FormatDeviceConfig, PlatformConfig, ResetConfig, ScriptConfig, ServiceConfig,
        SessionConfig, StorageConfig,
    };
    use crate::power::test::Nodes as PowerNodes;
    use crate::power::{BatteryChargeLimitMethod, TdpLimitingMethod, TdpManagerService};
    use crate::proxy::{LowPowerMode1Proxy, RemoteInterface1Proxy};
    use crate::session::{SessionManagerState, make_managed};
    use crate::systemd::escape;
    use crate::systemd::test::{MockManager, MockUnit};
    use crate::{path, testing};

    use anyhow::{anyhow, bail, ensure};
    use linux_cec::VendorId;
    use std::num::{NonZero, NonZeroU32};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::time::Duration;
    use tokio::fs::{create_dir_all, set_permissions, write};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use tokio::time::sleep;
    use zbus::object_server::Interface;

    struct TestHandle<S: TestSetup> {
        handle: testing::TestHandle,
        connection: Connection,
        rx_job: UnboundedReceiver<JobManagerCommand>,
        rx_tdp: Option<UnboundedReceiver<TdpManagerCommand>>,
        tx_tdp: Option<UnboundedSender<TdpManagerCommand>>,
        setup: S,
    }

    #[async_trait]
    trait TestSetup {
        async fn setup(
            &mut self,
            handle: &testing::TestHandle,
            connection: &Connection,
        ) -> Result<()>;
    }

    #[derive(Default)]
    struct NopTestSetup;

    #[async_trait]
    impl TestSetup for NopTestSetup {
        async fn setup(&mut self, _: &testing::TestHandle, _: &Connection) -> Result<()> {
            Ok(())
        }
    }

    struct TestConfig<S: TestSetup = NopTestSetup> {
        platform: Option<PlatformConfig>,
        device: Option<DeviceConfig>,
        power_nodes: PowerNodes,
        gpu_nodes: GpuNodes,
        setup: S,
    }

    impl TestConfig {
        fn all() -> TestConfig {
            TestConfig {
                platform: all_platform_config(),
                device: all_device_config(),
                power_nodes: PowerNodes::all(),
                gpu_nodes: GpuNodes::all(),
                setup: NopTestSetup,
            }
        }

        fn none() -> TestConfig {
            TestConfig {
                platform: None,
                device: None,
                power_nodes: PowerNodes::none(),
                gpu_nodes: GpuNodes::none(),
                setup: NopTestSetup,
            }
        }
    }

    impl<S: TestSetup> TestConfig<S> {
        fn only_setup(setup: S) -> TestConfig<S> {
            TestConfig {
                platform: None,
                device: None,
                power_nodes: PowerNodes::none(),
                gpu_nodes: GpuNodes::none(),
                setup,
            }
        }
    }

    fn all_platform_config() -> Option<PlatformConfig> {
        Some(PlatformConfig {
            factory_reset: Some(ResetConfig::default()),
            update_bios: Some(ScriptConfig::default()),
            update_dock: Some(ScriptConfig::default()),
            storage: Some(StorageConfig::default()),
            fan_control: Some(ServiceConfig::Systemd(String::from(
                "jupiter-fan-control.service",
            ))),
            session: Some(SessionConfig::default()),
        })
    }

    fn all_device_config() -> Option<DeviceConfig> {
        Some(DeviceConfig {
            device: vec![DeviceMatch {
                dmi: Some(DmiMatch {
                    sys_vendor: String::from("Valve"),
                    board_name: Some(String::from("Galileo")),
                    product_name: None,
                }),
                device: String::from("steam_deck"),
                variant: String::from("Galileo"),
                friendly_name: Some(String::from("Steam Deck")),
                oui: Some(VendorId::from_str("E0-31-9E").unwrap()),
            }],
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::AmdgpuHwmon,
                range: Some(RangeConfig::new(3, 15)),
                download_mode_limit: NonZeroU32::new(6),
                firmware_attribute: None,
                performance_profile: None,
            }),
            fan_speed: Some(FanSpeedConfig {
                hwmon: String::from("steamdeck_hwmon"),
                attribute: String::from("fan1_target"),
                download_mode_fan_speed: NonZeroU32::new(2000),
            }),
            gpu_performance: Some(GpuPerformanceConfig {
                driver: GpuPerformanceLevelDriverType::Amdgpu,
                clocks: Some(RangeConfig::new(200, 1600)),
            }),
            gpu_power_profile: Some(GpuPowerProfileConfig {
                driver: GpuPowerProfileDriverType::Amdgpu,
            }),
            battery_charge_limit: Some(BatteryChargeLimitConfig {
                suggested_minimum_limit: 10,
                method: BatteryChargeLimitMethod::HwmonAttribute {
                    hwmon: String::from("steamdeck_hwmon"),
                    attribute: String::from("max_battery_charge_level"),
                },
            }),
            performance_profile: Some(PerformanceProfileConfig {
                platform_profile_name: String::from("power-driver"),
                suggested_default: String::from("balanced"),
            }),
            inputplumber: None,
            cec_hw: None,
            firmware_debug: Some(ServiceConfig::Systemd(String::from("ec-log.service"))),
        })
    }

    #[derive(Debug)]
    struct MockCecdConfig1 {}

    #[interface(name = "com.steampowered.CecDaemon1.Config1")]
    impl MockCecdConfig1 {
        #[zbus(property)]
        fn wake_tv(&self) -> bool {
            true
        }
    }

    struct CecdSetup;

    #[async_trait]
    impl TestSetup for CecdSetup {
        async fn setup(&mut self, _: &testing::TestHandle, connection: &Connection) -> Result<()> {
            connection
                .request_name("com.steampowered.CecDaemon1")
                .await?;
            connection
                .object_server()
                .at("/com/steampowered/CecDaemon1/Daemon", MockCecdConfig1 {})
                .await?;
            Ok(())
        }
    }

    struct OrcaSetup;

    #[async_trait]
    impl TestSetup for OrcaSetup {
        async fn setup(&mut self, _: &testing::TestHandle, connection: &Connection) -> Result<()> {
            let object_server = connection.object_server();
            let orca_manager = OrcaManager::new(connection).await?;
            let screen_reader = Arc::new(Mutex::new(orca_manager));

            let screen_reader0 = ScreenReader0::new(screen_reader.clone()).await?;
            let screen_reader1 = ScreenReader1::new(screen_reader.clone()).await?;

            object_server.at(MANAGER_PATH, screen_reader0).await?;
            object_server.at(MANAGER_PATH, screen_reader1).await?;

            Ok(())
        }
    }

    struct ManagedSetup;

    #[async_trait]
    impl TestSetup for ManagedSetup {
        async fn setup(&mut self, _: &testing::TestHandle, _: &Connection) -> Result<()> {
            make_managed().await
        }
    }

    async fn start<S: TestSetup + Sync + Send>(mut config: TestConfig<S>) -> Result<TestHandle<S>> {
        let mut handle = testing::start();
        let (tx_ctx, mut rx_ctx) = channel::<UserContext>();
        let (tx_job, rx_job) = unbounded_channel::<JobManagerCommand>();
        let (tx_tdp, rx_tdp) = {
            if config
                .device
                .as_ref()
                .and_then(|config| config.tdp_limit.as_ref())
                .is_some()
            {
                let (tx_tdp, rx_tdp) = unbounded_channel::<TdpManagerCommand>();
                (Some(tx_tdp), Some(rx_tdp))
            } else {
                (None, None)
            }
        };

        if let Some(ref mut config) = config.platform {
            config.set_test_paths();
        }

        if let Some(config) = config.platform {
            handle.test.set_platform_config(config).await;
        } else {
            handle.test.clear_platform_config().await;
        }
        if let Some(config) = config.device {
            if config
                .device
                .iter()
                .any(|config| config.variant == "Galileo")
            {
                fake_model(SteamDeckVariant::Galileo).await?;
            }
            handle.test.set_device_config(config).await;
        } else {
            handle.test.clear_device_config().await;
        }
        let connection = handle.new_dbus().await?;
        connection.request_name("org.freedesktop.systemd1").await?;
        {
            let object_server = connection.object_server();
            object_server
                .at("/org/freedesktop/systemd1", MockManager::default())
                .await?;

            object_server
                .at(
                    PathBuf::from("/org/freedesktop/systemd1/unit")
                        .join(escape("ec-log.service"))
                        .to_string_lossy(),
                    MockUnit::default(),
                )
                .await?;

            object_server
                .at(
                    PathBuf::from("/org/freedesktop/systemd1/unit")
                        .join(escape("jupiter-fan-control.service"))
                        .to_string_lossy(),
                    MockUnit::default(),
                )
                .await?;
        }
        sleep(Duration::from_millis(10)).await;

        let exe_path = path("exe");
        write(&exe_path, "").await?;
        set_permissions(&exe_path, PermissionsExt::from_mode(0o700)).await?;

        create_dir_all(path("/usr/share/steamos-manager/remotes.d")).await?;

        handle
            .test
            .set_process_cb(|_, _| Ok((0, String::from("Interface wlan0"))))
            .await;
        crate::gpu::test::create_nodes(&config.gpu_nodes).await?;
        crate::power::test::create_nodes(&config.power_nodes).await?;

        config.setup.setup(&handle, &connection).await?;

        create_interfaces(
            connection.clone(),
            connection.clone(),
            tx_ctx,
            tx_job,
            tx_tdp.clone(),
        )
        .await?;

        tokio::spawn(async move {
            while let Some(command) = rx_ctx.recv().await {
                if let DaemonCommand::ContextCommand(UserCommand::GetSessionManagerState(sender)) =
                    command
                {
                    _ = sender.send(SessionManagerState::default())
                }
            }
            Ok::<_, Error>(())
        });

        sleep(Duration::from_millis(1)).await;

        Ok(TestHandle {
            handle,
            connection,
            rx_job,
            rx_tdp,
            tx_tdp,
            setup: config.setup,
        })
    }

    async fn test_interface_matches<I: Interface>(connection: &Connection) -> Result<bool> {
        let remote =
            testing::InterfaceIntrospection::from_remote::<I, _>(connection, MANAGER_PATH).await?;
        let local = testing::InterfaceIntrospection::from_local(
            "../data/interfaces/com.steampowered.SteamOSManager1.xml",
            I::name().to_string(),
        )
        .await?;
        Ok(remote.compare(&local))
    }

    async fn test_interface_missing<I: Interface>(connection: &Connection) -> bool {
        let remote =
            testing::InterfaceIntrospection::from_remote::<I, _>(connection, MANAGER_PATH).await;
        remote.is_err()
    }

    #[tokio::test]
    async fn interface_matches_ambient_light_sensor1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<AmbientLightSensor1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_battery_charge_limit() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<BatteryChargeLimit1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_battery_charge_limit() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<BatteryChargeLimit1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_cpu_boost1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<CpuBoost1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_cpu_boost1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<CpuBoost1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_cpu_scaling1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<CpuScaling1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_cpu_scheduler1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<CpuScheduler1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_cpu_scheduler1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<CpuScheduler1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_factory_reset1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<FactoryReset1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_factory_reset1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<FactoryReset1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_all_factory_reset1() {
        let mut config = all_platform_config().unwrap();
        config.factory_reset.as_mut().unwrap().all = ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        };
        let test = start(TestConfig {
            platform: Some(config),
            ..TestConfig::none()
        })
        .await
        .expect("start");

        assert!(test_interface_missing::<FactoryReset1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_os_factory_reset1() {
        let mut config = all_platform_config().unwrap();
        config.factory_reset.as_mut().unwrap().os = ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        };
        let test = start(TestConfig {
            platform: Some(config),
            ..TestConfig::none()
        })
        .await
        .expect("start");

        assert!(test_interface_missing::<FactoryReset1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_user_factory_reset1() {
        let mut config = TestConfig::none();
        config.platform = all_platform_config();
        config
            .platform
            .as_mut()
            .unwrap()
            .factory_reset
            .as_mut()
            .unwrap()
            .user = ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        };
        let test = start(config).await.expect("start");

        assert!(test_interface_missing::<FactoryReset1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_fan_control1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<FanControl1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_fan_control1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<FanControl1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_firmware_debug1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<FirmwareDebug1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_firmware_debug1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<FirmwareDebug1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_gpu_performance_level1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<GpuPerformanceLevel1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_gpu_performance_level1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<GpuPerformanceLevel1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_gpu_power_profile1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<GpuPowerProfile1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_gpu_power_profile1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<GpuPowerProfile1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_hdmi_cec1() {
        let test = start(TestConfig::only_setup(CecdSetup {}))
            .await
            .expect("start");

        assert!(
            test_interface_matches::<HdmiCec1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_hdmi_cec1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<HdmiCec1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_hdmi_cec2() {
        let test = start(TestConfig::only_setup(CecdSetup {}))
            .await
            .expect("start");

        assert!(
            test_interface_matches::<HdmiCec2>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_hdmi_cec2() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<HdmiCec2>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_low_power_mode1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<LowPowerMode1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_low_power_mode1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<LowPowerMode1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_manager2() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<Manager2>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_performance_profile1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<PerformanceProfile1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_performance_profile1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<PerformanceProfile1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_remote_interface1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<RemoteInterface1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_screen_reader0() {
        let test = start(TestConfig::only_setup(OrcaSetup {}))
            .await
            .expect("start");

        assert!(
            test_interface_matches::<ScreenReader0>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_screen_reader0() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<ScreenReader0>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_screen_reader1() {
        let test = start(TestConfig::only_setup(OrcaSetup {}))
            .await
            .expect("start");

        assert!(
            test_interface_matches::<ScreenReader1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_screen_reader1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<ScreenReader1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_session_management1() {
        let test = start(TestConfig::only_setup(ManagedSetup {}))
            .await
            .expect("start");

        assert!(
            test_interface_matches::<SessionManagement1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_session_management1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<SessionManagement1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_storage1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<Storage1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_storage1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<Storage1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_trim_storage1() {
        let mut config = all_platform_config().unwrap();
        config.storage.as_mut().unwrap().trim_devices = ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        };
        let test = start(TestConfig {
            platform: Some(config),
            device: all_device_config(),
            ..TestConfig::none()
        })
        .await
        .expect("start");

        assert!(test_interface_missing::<Storage1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_format_storage1() {
        let mut config = all_platform_config().unwrap();
        let format_config = FormatDeviceConfig {
            script: PathBuf::from("oxo"),
            ..FormatDeviceConfig::default()
        };
        config.storage.as_mut().unwrap().format_device = format_config;
        let test = start(TestConfig {
            platform: Some(config),
            device: all_device_config(),
            ..TestConfig::none()
        })
        .await
        .expect("start");

        assert!(test_interface_missing::<Storage1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_tdp_limit1() {
        let mut test = start(TestConfig::all()).await.expect("start");

        let TdpManagerCommand::IsActive(reply) =
            test.rx_tdp.as_mut().unwrap().recv().await.unwrap()
        else {
            panic!();
        };
        reply.send(Ok(true)).unwrap();
        sleep(Duration::from_millis(1)).await;

        assert!(
            test_interface_matches::<TdpLimit1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_tdp_limit1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<TdpLimit1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_inactive_tdp_limit1() {
        let mut test = start(TestConfig::all()).await.expect("start");

        let TdpManagerCommand::IsActive(reply) =
            test.rx_tdp.as_mut().unwrap().recv().await.unwrap()
        else {
            panic!();
        };
        reply.send(Ok(false)).unwrap();
        sleep(Duration::from_millis(1)).await;

        assert!(test_interface_missing::<TdpLimit1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_update_bios1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<UpdateBios1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_update_bios1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<UpdateBios1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_update_bios1() {
        let mut config = all_platform_config().unwrap();
        config.update_bios = Some(ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        });
        let test = start(TestConfig {
            platform: Some(config),
            device: all_device_config(),
            ..TestConfig::none()
        })
        .await
        .expect("start");

        assert!(test_interface_missing::<UpdateBios1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_update_dock1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<UpdateDock1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_update_dock1() {
        let test = start(TestConfig::none()).await.unwrap();

        assert!(test_interface_missing::<UpdateDock1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_missing_invalid_update_dock1() {
        let mut config = all_platform_config().unwrap();
        config.update_dock = Some(ScriptConfig {
            script: PathBuf::from("oxo"),
            script_args: Vec::new(),
        });
        let test = start(TestConfig {
            platform: Some(config),
            device: all_device_config(),
            ..TestConfig::none()
        })
        .await
        .expect("start");

        assert!(test_interface_missing::<UpdateDock1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_wifi_backend1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<WifiBackend1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_matches_wifi_debug1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<WifiDebug1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_wifi_debug1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<WifiDebug1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_wifi_debug_dump1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<WifiDebugDump1>(&test.connection)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn interface_missing_wifi_debug_dump1() {
        let test = start(TestConfig::none()).await.expect("start");

        assert!(test_interface_missing::<WifiDebugDump1>(&test.connection).await);
    }

    #[tokio::test]
    async fn interface_matches_wifi_power_management1() {
        let test = start(TestConfig::all()).await.expect("start");

        assert!(
            test_interface_matches::<WifiPowerManagement1>(&test.connection)
                .await
                .unwrap()
        );
    }

    async fn register_remote<I: RemoteInterface + Interface>(
        connection: &Connection,
        new_conn: &Connection,
    ) -> Result<()> {
        let iface = connection
            .object_server()
            .interface::<_, RemoteInterface1>(MANAGER_PATH)
            .await?;
        let signal_emitter = iface.signal_emitter();
        ensure!(
            iface
                .get_mut()
                .await
                .register(
                    &<I as Interface>::name(),
                    OwnedObjectPath::try_from("/foo")?,
                    OwnedBusName::from(BusName::Unique(
                        new_conn.unique_name().unwrap().to_owned().into()
                    )),
                    Some(signal_emitter),
                    true,
                )
                .await?
        );

        Ok(())
    }

    async fn unregister_remote<I: RemoteInterface + Interface>(
        connection: &Connection,
        new_conn: &Connection,
    ) -> Result<()> {
        let iface = connection
            .object_server()
            .interface::<_, RemoteInterface1>(MANAGER_PATH)
            .await?;
        let signal_emitter = iface.signal_emitter();
        iface
            .get_mut()
            .await
            .unregister(
                &<I as Interface>::name(),
                Some(new_conn.unique_name().unwrap()),
                signal_emitter,
                false,
            )
            .await?;

        Ok(())
    }

    async fn test_remote_interface_added<I: RemoteInterface + Interface, S: TestSetup>(
        test: &TestHandle<S>,
        new_conn: &Connection,
    ) -> Result<()> {
        let proxy = RemoteInterface1Proxy::builder(new_conn)
            .destination(
                test.connection
                    .unique_name()
                    .ok_or(anyhow!("no unique name"))?,
            )?
            .build()
            .await?;

        ensure!(test_remote_interface_missing::<I, _>(&proxy, test).await?);

        register_remote::<I>(&test.connection, new_conn).await?;
        sleep(Duration::from_millis(1)).await;

        ensure!(!test_remote_interface_missing::<I, _>(&proxy, test).await?);

        Ok(())
    }

    async fn test_remote_interface_missing<I: RemoteInterface + Interface, S: TestSetup>(
        proxy: &RemoteInterface1Proxy<'_>,
        test: &TestHandle<S>,
    ) -> Result<bool> {
        Ok(!proxy
            .remote_interfaces()
            .await?
            .contains(&<I as Interface>::name().to_string())
            && test_interface_missing::<<I as RemoteInterface>::Remote>(&test.connection).await)
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1() {
        let test = start(TestConfig::none()).await.unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        test_remote_interface_added::<BatteryChargeLimit1, _>(&test, &new_conn)
            .await
            .unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            !test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_dropped() {
        let test = start(TestConfig::none()).await.unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        test_remote_interface_added::<BatteryChargeLimit1, _>(&test, &new_conn)
            .await
            .unwrap();

        drop(new_conn);

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_removed() {
        let test = start(TestConfig::none()).await.unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        test_remote_interface_added::<BatteryChargeLimit1, _>(&test, &new_conn)
            .await
            .unwrap();

        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        unregister_remote::<BatteryChargeLimit1>(&test.connection, &new_conn)
            .await
            .unwrap();

        assert!(
            test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_not_removed() {
        let test = start(TestConfig::none()).await.unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        test_remote_interface_added::<BatteryChargeLimit1, _>(&test, &new_conn)
            .await
            .unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            unregister_remote::<BatteryChargeLimit1>(&test.connection, &new_conn)
                .await
                .is_err()
        );

        assert!(
            !test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    struct RemoteSetup {
        configs: i32,
        name: &'static str,
        remotes: Vec<Connection>,
    }

    impl RemoteSetup {
        fn new(name: &'static str, configs: i32) -> RemoteSetup {
            RemoteSetup {
                configs,
                name,
                remotes: Vec::new(),
            }
        }

        async fn setup_remotes(
            &mut self,
            handle: &testing::TestHandle,
            connection: &Connection,
        ) -> Result<()> {
            let new_conn = handle.new_connection().await?;
            new_conn.request_name("com.steampowered.TestDaemon").await?;
            self.remotes.push(new_conn);
            if self.configs > 0 {
                for c in 1..self.configs {
                    let new_conn = handle.new_connection().await?;
                    new_conn
                        .request_name(format!("com.steampowered.TestDaemon{c}"))
                        .await?;
                    self.remotes.push(new_conn);
                }
            }

            let manager = connection
                .object_server()
                .interface::<_, RemoteInterface1>(MANAGER_PATH)
                .await?;
            {
                let mut manager = manager.get_mut().await;
                if let Some(remote) = manager.remote_battery_charge_limit1.as_mut() {
                    remote.ping_success = true;
                    remote.register().await?;
                }
                if let Some(remote) = manager.remote_tdp_limit1.as_mut() {
                    remote.ping_success = true;
                    remote.register().await?;
                }
            }

            Ok(())
        }
    }

    #[async_trait]
    impl TestSetup for RemoteSetup {
        async fn setup(&mut self, _: &testing::TestHandle, _: &Connection) -> Result<()> {
            write(
                path("/usr/share/steamos-manager/remotes.d/00-test.toml"),
                format!(
                    "[{}]\nbus_name = \"com.steampowered.TestDaemon\"\nobject_path = \"/foo\"",
                    self.name
                )
                .as_bytes(),
            )
            .await?;
            if self.configs > 0 {
                for c in 1..self.configs {
                    write(
                        path(format!("/usr/share/steamos-manager/remotes.d/{c:02}-test.toml")),
                        format!(
                            "[{}]\nbus_name = \"com.steampowered.TestDaemon{c}\"\nobject_path = \"/foo{c}\"",
                            self.name
                        )
                        .as_bytes(),
                    )
                    .await?;
                }
            }

            Ok(())
        }
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_autoadd() {
        let mut test = start(TestConfig::only_setup(RemoteSetup::new(
            "BatteryChargeLimit1",
            1,
        )))
        .await
        .unwrap();

        test.setup
            .setup_remotes(&test.handle, &test.connection)
            .await
            .unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            !test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_autoadd_duplicate() {
        let mut test = start(TestConfig::only_setup(RemoteSetup::new(
            "BatteryChargeLimit1",
            2,
        )))
        .await
        .unwrap();

        test.setup
            .setup_remotes(&test.handle, &test.connection)
            .await
            .unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            !test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );

        let remote_config = RemoteInterface1Config::load().await.unwrap();
        assert_eq!(
            remote_config
                .battery_charge_limit1
                .map(|config| config.object_path.to_string()),
            Some(String::from("/foo1"))
        );
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_autoadd_no_remote() {
        let test = start(TestConfig::only_setup(RemoteSetup::new(
            "BatteryChargeLimit1",
            1,
        )))
        .await
        .unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_autoadd_late_remote() {
        let test = start(TestConfig::only_setup(RemoteSetup::new(
            "BatteryChargeLimit1",
            1,
        )))
        .await
        .unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );

        let manager = test
            .connection
            .object_server()
            .interface::<_, RemoteInterface1>(MANAGER_PATH)
            .await
            .unwrap();
        {
            let mut manager = manager.get_mut().await;
            if let Some(remote) = manager.remote_battery_charge_limit1.as_mut() {
                remote.ping_success = true;
            }
        }
        let new_conn = test.handle.new_connection().await.unwrap();
        new_conn
            .request_name("com.steampowered.TestDaemon")
            .await
            .unwrap();
        sleep(Duration::from_millis(4)).await;

        assert!(
            !test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_autoadd_drop_remote() {
        let mut test = start(TestConfig::only_setup(RemoteSetup::new(
            "BatteryChargeLimit1",
            1,
        )))
        .await
        .unwrap();

        test.setup
            .setup_remotes(&test.handle, &test.connection)
            .await
            .unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            !test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );

        test.setup.remotes.clear();
        sleep(Duration::from_millis(4)).await;

        assert!(
            test_remote_interface_missing::<BatteryChargeLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    #[derive(Debug)]
    struct MockBatteryChargeLimit1 {
        limit: i32,
    }

    #[interface(name = "com.steampowered.SteamOSManager1.BatteryChargeLimit1")]
    impl MockBatteryChargeLimit1 {
        #[zbus(property)]
        async fn max_charge_level(&self) -> i32 {
            self.limit
        }

        #[zbus(property)]
        async fn set_max_charge_level(&mut self, limit: i32) -> () {
            self.limit = limit;
        }

        #[zbus(property(emits_changed_signal = "const"))]
        async fn suggested_minimum_limit(&self) -> i32 {
            BATTERY_DEFAULT_SUGGESTED_MINIMUM_LIMIT
        }
    }

    #[tokio::test]
    async fn remote_battery_charge_limit1_relay() {
        let test = start(TestConfig::none()).await.unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        test_remote_interface_added::<BatteryChargeLimit1, _>(&test, &new_conn)
            .await
            .unwrap();

        let proxy = BatteryChargeLimit1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        let remote = MockBatteryChargeLimit1 { limit: 50 };
        let object_server = new_conn.object_server();
        object_server.at("/foo", remote).await.unwrap();
        let remote = object_server
            .interface::<_, MockBatteryChargeLimit1>("/foo")
            .await
            .unwrap();
        assert_eq!(proxy.max_charge_level().await.unwrap(), 50);
        proxy.set_max_charge_level(20).await.unwrap();
        assert_eq!(remote.get().await.limit, 20);
        assert_eq!(proxy.max_charge_level().await.unwrap(), 20);
    }

    #[derive(Debug)]
    struct MockTdpLimit1 {
        limit: u32,
        min: u32,
        max: u32,
    }

    #[interface(name = "com.steampowered.SteamOSManager1.TdpLimit1")]
    impl MockTdpLimit1 {
        #[zbus(property)]
        async fn tdp_limit(&self) -> u32 {
            self.limit
        }

        #[zbus(property)]
        async fn set_tdp_limit(&mut self, limit: u32) -> fdo::Result<()> {
            if !(self.min..=self.max).contains(&limit) {
                return Err(fdo::Error::InvalidArgs(format!("{limit} out of range")));
            }
            self.limit = limit;
            Ok(())
        }

        #[zbus(property(emits_changed_signal = "const"))]
        async fn tdp_limit_min(&self) -> u32 {
            self.min
        }

        #[zbus(property(emits_changed_signal = "const"))]
        async fn tdp_limit_max(&self) -> u32 {
            self.max
        }
    }

    #[tokio::test]
    async fn remote_tdp_limit1_autoadd() {
        let mut test = start(TestConfig::only_setup(RemoteSetup::new("TdpLimit1", 1)))
            .await
            .unwrap();

        test.setup
            .setup_remotes(&test.handle, &test.connection)
            .await
            .unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        let proxy = RemoteInterface1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(
            !test_remote_interface_missing::<TdpLimit1, _>(&proxy, &test)
                .await
                .unwrap()
        );
    }

    async fn setup_tdp_manager<S: TestSetup>(
        test: &mut TestHandle<S>,
    ) -> Result<JoinHandle<Result<()>>> {
        let tx_tdp;
        let rx_tdp;
        if let Some(rx) = test.rx_tdp.take()
            && let Some(tx) = test.tx_tdp.take()
        {
            rx_tdp = rx;
            tx_tdp = tx;
        } else {
            (tx_tdp, rx_tdp) = unbounded_channel::<TdpManagerCommand>();
        }
        let mut service =
            TdpManagerService::new(rx_tdp, &test.connection, &test.connection).await?;
        let service = spawn(async move { service.run().await });

        let remote_iface = test
            .connection
            .object_server()
            .interface::<_, RemoteInterface1>(MANAGER_PATH)
            .await?;
        remote_iface.get_mut().await.context_tdp_limit1 = Some(tx_tdp);

        Ok(service)
    }

    #[tokio::test]
    async fn remote_tdp_limit1_relay() {
        let mut test = start(TestConfig::none()).await.unwrap();

        let service = setup_tdp_manager(&mut test).await.unwrap();
        let new_conn = test.handle.new_connection().await.unwrap();
        test_remote_interface_added::<TdpLimit1, _>(&test, &new_conn)
            .await
            .unwrap();

        let proxy = TdpLimit1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        let remote = MockTdpLimit1 {
            limit: 10,
            min: 3,
            max: 15,
        };
        let object_server = new_conn.object_server();
        object_server.at("/foo", remote).await.unwrap();
        let remote = object_server
            .interface::<_, MockTdpLimit1>("/foo")
            .await
            .unwrap();
        assert_eq!(proxy.tdp_limit().await.unwrap(), 10);
        assert_eq!(proxy.tdp_limit_min().await.unwrap(), 3);
        assert_eq!(proxy.tdp_limit_max().await.unwrap(), 15);
        proxy.set_tdp_limit(12).await.unwrap();
        assert_eq!(remote.get().await.limit, 12);

        service.abort();
    }

    #[tokio::test]
    async fn remote_tdp_limit1_relay_download_mode() {
        let device = DeviceConfig {
            tdp_limit: Some(TdpLimitConfig {
                method: TdpLimitingMethod::RemoteInterface,
                range: None,
                download_mode_limit: Some(NonZero::new(5).unwrap()),
                firmware_attribute: None,
                performance_profile: None,
            }),
            ..DeviceConfig::default()
        };
        let mut test = start(TestConfig {
            device: Some(device),
            ..TestConfig::none()
        })
        .await
        .unwrap();

        let service = setup_tdp_manager(&mut test).await.unwrap();
        let new_conn = test.handle.new_connection().await.unwrap();
        test_remote_interface_added::<TdpLimit1, _>(&test, &new_conn)
            .await
            .unwrap();
        assert!(!test_interface_missing::<LowPowerMode1>(&test.connection).await);

        let unique_name = test.connection.unique_name().unwrap();
        let proxy = TdpLimit1Proxy::builder(&new_conn)
            .destination(unique_name)
            .unwrap()
            .build()
            .await
            .unwrap();
        let low_power_proxy = LowPowerMode1Proxy::builder(&new_conn)
            .destination(unique_name)
            .unwrap()
            .build()
            .await
            .unwrap();

        let remote = MockTdpLimit1 {
            limit: 10,
            min: 3,
            max: 15,
        };
        let object_server = new_conn.object_server();
        object_server.at("/foo", remote).await.unwrap();
        let remote = object_server
            .interface::<_, MockTdpLimit1>("/foo")
            .await
            .unwrap();
        assert_eq!(proxy.tdp_limit().await.unwrap(), 10);
        let mut receiver = proxy.receive_tdp_limit_changed().await;
        assert_eq!(receiver.next().await.unwrap().get().await.unwrap(), 10);
        let fd = low_power_proxy.enter_download_mode("foo").await.unwrap();
        assert_eq!(receiver.next().await.unwrap().get().await.unwrap(), 5);
        assert_eq!(proxy.tdp_limit().await.unwrap(), 5);
        assert_eq!(remote.get().await.limit, 5);
        drop(fd);
        assert_eq!(receiver.next().await.unwrap().get().await.unwrap(), 10);
        assert_eq!(proxy.tdp_limit().await.unwrap(), 10);
        assert_eq!(remote.get().await.limit, 10);

        service.abort();
    }

    struct MockUpdateBios1 {}

    #[interface(name = "com.steampowered.SteamOSManager1.UpdateBios1")]
    impl MockUpdateBios1 {
        async fn update_bios(&mut self) -> fdo::Result<OwnedObjectPath> {
            OwnedObjectPath::try_from("/job").map_err(to_zbus_fdo_error)
        }
    }

    #[tokio::test]
    async fn remote_update_bios1_relay() {
        let test = start(TestConfig::none()).await.unwrap();

        let new_conn = test.handle.new_connection().await.unwrap();
        test_remote_interface_added::<UpdateBios1, _>(&test, &new_conn)
            .await
            .unwrap();

        let proxy = UpdateBios1Proxy::builder(&new_conn)
            .destination(test.connection.unique_name().unwrap())
            .unwrap()
            .build()
            .await
            .unwrap();

        let remote = MockUpdateBios1 {};
        let object_server = new_conn.object_server();
        object_server.at("/foo", remote).await.unwrap();

        let mut rx_job = test.rx_job;
        let service = spawn(async move {
            let job = rx_job.recv().await;
            ensure!(matches!(job, Some(JobManagerCommand::MirrorConnection(_))));
            let Some(JobManagerCommand::MirrorJob { path, reply, .. }) = rx_job.recv().await else {
                bail!("Unexpected command");
            };
            ensure!(path.as_str() == "/job");
            reply.send(Ok(OwnedObjectPath::try_from("/bar")?)).unwrap();
            Ok(())
        });

        let job = proxy.update_bios().await.unwrap();
        service.await.unwrap().unwrap();
        assert_eq!(job.as_str(), "/bar");
    }
}
