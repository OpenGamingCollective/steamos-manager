/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 * Copyright © 2024 Igalia S.L.
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::ffi::OsStr;
use tokio::fs::File;
use tokio::spawn;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tracing::{debug, error, info};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{self, Fd};
use zbus::{Connection, fdo, interface, proxy};

use crate::cec::{HdmiCecHwController, cec_hw_controller};
use crate::daemon::DaemonCommand;
use crate::daemon::root::{Command, RootCommand};
use crate::error::{to_zbus_error, to_zbus_fdo_error};
use crate::gpu::{
    GpuPerformanceLevelDriver, GpuPowerProfileDriver, gpu_performance_level_driver,
    gpu_power_profile_driver,
};
use crate::hardware::{
    FactoryResetKind, FanControl, FanControlState, SteamDeckVariant, device_config,
    steam_deck_variant,
};
use crate::job::JobManager;
use crate::platform::{ServiceConfig, platform_config};
use crate::power::{
    CPUBoostState, CPUScalingGovernor, CpuScheduler, CpuSchedulerManager, TdpLimitManager,
    set_cpu_boost_state, set_cpu_scaling_governor, set_max_charge_level, set_platform_profile,
    tdp_limit_manager,
};
use crate::process::{run_script, script_exit_code, script_output};
use crate::session::root::{clean_temporary_sessions, set_default_session, set_temporary_session};
use crate::sysfs::SysfsWritten;
use crate::systemd::{JobMode, SystemdUnit};
use crate::wifi::{
    WifiBackend, WifiDebugMode, WifiPowerManagement, extract_wifi_trace, generate_wifi_dump,
    set_wifi_backend, set_wifi_debug_mode, set_wifi_power_management_state,
};
use crate::{SerialOrderValidator, path};

#[derive(PartialEq, Debug, Copy, Clone)]
#[repr(u32)]
enum PrepareFactoryResetResult {
    // NOTE: both old PrepareFactoryReset and new PrepareFactoryResetExt use these
    // result values.
    Unknown = 0,
    RebootRequired = 1,
}

pub struct SteamOSManager {
    connection: Connection,
    channel: Sender<Command>,
    order: SerialOrderValidator,
    wifi_debug_mode: WifiDebugMode,
    fan_control: FanControl,
    tdp_limit_manager: Option<Box<dyn TdpLimitManager>>,
    gpu_performance_level: Option<Box<dyn GpuPerformanceLevelDriver>>,
    gpu_power_profile: Option<Box<dyn GpuPowerProfileDriver>>,
    // Whether we should use trace-cmd or not.
    // True on galileo devices, false otherwise
    should_trace: bool,
    job_manager: JobManager,
    cpu_scheduler_manager: CpuSchedulerManager<'static>,
    cec_hw: Option<Box<dyn HdmiCecHwController>>,
}

impl SteamOSManager {
    pub async fn new(connection: Connection, channel: Sender<Command>) -> Result<Self> {
        Ok(SteamOSManager {
            fan_control: FanControl::new(connection.clone()),
            wifi_debug_mode: WifiDebugMode::Off,
            tdp_limit_manager: tdp_limit_manager(&connection)
                .await
                .inspect_err(|e| info!("Could not set up TDP limiting: {e}"))
                .ok(),
            gpu_performance_level: gpu_performance_level_driver()
                .await
                .inspect_err(|e| info!("Could not set up GPU performance management: {e}"))
                .ok(),
            gpu_power_profile: gpu_power_profile_driver()
                .await
                .inspect_err(|e| info!("Could not set up GPU power profile management: {e}"))
                .ok(),
            should_trace: steam_deck_variant().await.unwrap_or_default()
                == SteamDeckVariant::Galileo,
            job_manager: JobManager::new(connection.clone()).await?,
            cpu_scheduler_manager: CpuSchedulerManager::new(&connection).await?,
            cec_hw: cec_hw_controller()
                .await
                .inspect_err(|e| info!("Could not set up HDMI CEC hardware controller: {e}"))
                .ok(),
            connection,
            channel,
            order: SerialOrderValidator::default(),
        })
    }
}

#[proxy(
    interface = "com.steampowered.SteamOSManager1.RootManager",
    default_service = "com.steampowered.SteamOSManager1",
    default_path = "/com/steampowered/SteamOSManager1"
)]
pub(crate) trait RootManager {
    fn set_tdp_limit(&self, limit: u32) -> zbus::Result<()>;
    fn set_temporary_session(&self, session: &str) -> zbus::Result<()>;
    fn set_default_session(&self, session: &str) -> zbus::Result<()>;
    fn set_fan_speed(&self, rpm: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn fan_control_state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_fan_control_state(&self, state: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn ec_logging(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_ec_logging(&self, state: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn hdmi_cec_can_awaken(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn hdmi_cec_awaken(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_hdmi_cec_awaken(&self, awaken: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn hdmi_cec_phys_addr(&self) -> zbus::Result<u16>;
    #[zbus(property)]
    fn set_hdmi_cec_phys_addr(&self, phys_addr: u16) -> zbus::Result<()>;
}

#[interface(name = "com.steampowered.SteamOSManager1.RootManager", spawn = false)]
impl SteamOSManager {
    async fn prepare_factory_reset(&self, kind: u32) -> fdo::Result<u32> {
        // Run steamos-reset with arguments based on flags passed and return 1 on success
        let config = platform_config().await.map_err(to_zbus_fdo_error)?;
        let Some(config) = config
            .as_ref()
            .and_then(|config| config.factory_reset.as_ref())
        else {
            return Err(fdo::Error::NotSupported(String::from(
                "PrepareFactoryReset is not supported on this platform",
            )));
        };
        let res = match FactoryResetKind::try_from(kind) {
            Ok(FactoryResetKind::User) => {
                run_script(&config.user.script, &config.user.script_args).await
            }
            Ok(FactoryResetKind::OS) => run_script(&config.os.script, &config.os.script_args).await,
            Ok(FactoryResetKind::All) => {
                run_script(&config.all.script, &config.all.script_args).await
            }
            Err(_) => Err(anyhow!(
                "Unable to generate command arguments for steamos-reset-tool script"
            )),
        };
        Ok(match res {
            Ok(()) => PrepareFactoryResetResult::RebootRequired as u32,
            Err(_) => PrepareFactoryResetResult::Unknown as u32,
        })
    }

    async fn set_wifi_power_management_state(&self, state: u32) -> fdo::Result<()> {
        let state = match WifiPowerManagement::try_from(state) {
            Ok(state) => state,
            Err(err) => return Err(to_zbus_fdo_error(err)),
        };
        set_wifi_power_management_state(state, &self.connection)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn fan_control_state(&self) -> fdo::Result<u32> {
        Ok(self
            .fan_control
            .get_state()
            .await
            .map_err(to_zbus_fdo_error)? as u32)
    }

    #[zbus(property)]
    async fn set_fan_control_state(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let state = match FanControlState::try_from(state) {
            Ok(state) => state,
            Err(err) => return Err(fdo::Error::InvalidArgs(err.to_string()).into()),
        };
        // Run what steamos-polkit-helpers/jupiter-fan-control does
        self.fan_control
            .set_state(state)
            .await
            .map_err(to_zbus_error)?;
        self.fan_control_state_changed(&ctx).await
    }

    async fn set_fan_speed(&self, rpm: u32) -> fdo::Result<()> {
        self.fan_control
            .set_speed(rpm)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn ec_logging(&self) -> fdo::Result<u32> {
        let config = device_config().await.map_err(to_zbus_fdo_error)?;
        match config
            .as_ref()
            .and_then(|config| config.firmware_debug.as_ref())
        {
            Some(ServiceConfig::Systemd(service)) => {
                let systemd_service = SystemdUnit::new(&self.connection, service)
                    .await
                    .map_err(to_zbus_fdo_error)?;
                let active = systemd_service.active().await.map_err(to_zbus_fdo_error)?;
                Ok(active.is_active() as u32)
            }
            Some(ServiceConfig::Script { status, .. }) => {
                let res = script_exit_code(&status.script, &status.script_args)
                    .await
                    .map_err(to_zbus_fdo_error)?;
                if res < 0 {
                    return Err(fdo::Error::Failed(String::from("Script exited abnormally")));
                }
                Ok((res > 0) as u32)
            }
            None => Err(fdo::Error::Failed(String::from(
                "Firmware debug not configured",
            ))),
        }
    }

    #[zbus(property)]
    async fn set_ec_logging(
        &self,
        state: u32,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let enable: bool = state > 0;

        let config = device_config().await.map_err(to_zbus_error)?;
        match config
            .as_ref()
            .and_then(|config| config.firmware_debug.as_ref())
        {
            Some(ServiceConfig::Systemd(service)) => {
                let systemd_service = SystemdUnit::new(&self.connection, service)
                    .await
                    .map_err(to_zbus_error)?;
                if enable {
                    systemd_service.start(JobMode::Fail).await
                } else {
                    systemd_service.stop(JobMode::Fail).await
                }
                .map_err(to_zbus_error)?;
            }
            Some(ServiceConfig::Script { start, stop, .. }) => if enable {
                run_script(&start.script, &start.script_args).await
            } else {
                run_script(&stop.script, &stop.script_args).await
            }
            .map_err(to_zbus_error)?,
            None => {
                return Err(zbus::Error::Failure(String::from(
                    "Firmware debug not configured",
                )));
            }
        }

        self.ec_logging_changed(&ctx).await
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn als_calibration_gain(&self) -> Vec<f64> {
        // Run script to get calibration value
        let mut gains = Vec::new();
        let indices: &[&str] = match steam_deck_variant().await {
            Ok(SteamDeckVariant::Jupiter) => &["2"],
            Ok(SteamDeckVariant::Galileo) => &["2", "4"],
            _ => return Vec::new(),
        };
        for index in indices {
            let result = script_output("/usr/bin/dmidecode", &["--oem-string", index]).await;
            gains.push(match result {
                Ok(as_string) => as_string.trim().parse().unwrap_or(-1.0),
                Err(message) => {
                    error!("Unable to run als calibration script: {}", message);
                    -1.0
                }
            });
        }

        gains
    }

    async fn get_als_integration_time_file_descriptor(&self, index: u32) -> fdo::Result<Fd<'_>> {
        // Get the file descriptor for the als integration time sysfs path
        let i0 = match steam_deck_variant().await.map_err(to_zbus_fdo_error)? {
            SteamDeckVariant::Jupiter => 1,
            SteamDeckVariant::Galileo => index,
            SteamDeckVariant::Unknown => {
                return Err(fdo::Error::Failed(String::from("Unknown model")));
            }
        };
        let als_path = path(format!(
            "/sys/devices/platform/AMDI0010:00/i2c-0/i2c-PRP0001:0{i0}/iio:device{index}/in_illuminance_integration_time"
        ));
        let result = File::create(als_path).await;

        match result {
            Ok(f) => Ok(Fd::Owned(std::os::fd::OwnedFd::from(f.into_std().await))),
            Err(message) => {
                error!("Error opening sysfs file for giving file descriptor: {message}");
                Err(fdo::Error::IOError(message.to_string()))
            }
        }
    }

    async fn update_bios(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        // Update the bios as needed
        let config = platform_config().await.map_err(to_zbus_fdo_error)?;
        let Some(config) = config
            .as_ref()
            .and_then(|config| config.update_bios.as_ref())
        else {
            return Err(fdo::Error::NotSupported(String::from(
                "UpdateBios is not supported on this platform",
            )));
        };
        self.job_manager
            .run_process(&config.script, &config.script_args, "updating BIOS")
            .await
    }

    async fn update_dock(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        // Update the dock firmware as needed
        let config = platform_config().await.map_err(to_zbus_fdo_error)?;
        let Some(config) = config
            .as_ref()
            .and_then(|config| config.update_dock.as_ref())
        else {
            return Err(fdo::Error::NotSupported(String::from(
                "UpdateDock is not supported on this platform",
            )));
        };
        self.job_manager
            .run_process(&config.script, &config.script_args, "updating dock")
            .await
    }

    async fn trim_devices(&mut self) -> fdo::Result<zvariant::OwnedObjectPath> {
        // Run steamos-trim-devices script
        let config = platform_config().await.map_err(to_zbus_fdo_error)?;
        let Some(config) = config.as_ref().and_then(|config| config.storage.as_ref()) else {
            return Err(fdo::Error::NotSupported(String::from(
                "TrimDevices is not supported on this platform",
            )));
        };
        self.job_manager
            .run_process(
                &config.trim_devices.script,
                config.trim_devices.script_args.as_ref(),
                "trimming devices",
            )
            .await
    }

    async fn format_device(
        &mut self,
        device: &str,
        label: &str,
        validate: bool,
    ) -> fdo::Result<zvariant::OwnedObjectPath> {
        let config = platform_config().await.map_err(to_zbus_fdo_error)?;
        let Some(config) = config.as_ref().and_then(|config| config.storage.as_ref()) else {
            return Err(fdo::Error::NotSupported(String::from(
                "FormatDevice is not supported on this platform",
            )));
        };
        let config = &config.format_device;
        let mut args: Vec<&OsStr> = config.script_args.iter().map(AsRef::as_ref).collect();

        args.extend_from_slice(&[OsStr::new(config.label_flag.as_str()), OsStr::new(label)]);

        match (validate, &config.validate_flag, &config.no_validate_flag) {
            (true, Some(validate_flag), _) => args.push(OsStr::new(validate_flag)),
            (false, _, Some(no_validate_flag)) => args.push(OsStr::new(no_validate_flag)),
            _ => (),
        }

        if let Some(device_flag) = &config.device_flag {
            args.push(OsStr::new(device_flag));
        }
        args.push(OsStr::new(device));

        self.job_manager
            .run_process(
                &config.script,
                &args,
                format!("formatting {device}").as_str(),
            )
            .await
    }

    async fn set_gpu_power_profile(
        &mut self,
        value: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        if !self.order.check_header(header) {
            debug!("SetGpuPowerProfile: discarding out of order serial");
            return Ok(());
        }
        let Some(ref driver) = self.gpu_power_profile else {
            return Err(fdo::Error::Failed(String::from(
                "GPU power profile settings not configured",
            )));
        };
        let profile = driver
            .power_profile_from_str(value)
            .map_err(to_zbus_fdo_error)?;
        info!("Setting GPU power profile to {profile}");
        driver
            .set_power_profile(profile)
            .await
            .inspect_err(|message| error!("Error setting GPU power profile: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_cpu_scaling_governor(&self, governor: String) -> fdo::Result<()> {
        let g = CPUScalingGovernor::try_from(governor.as_str()).map_err(to_zbus_fdo_error)?;
        info!("Setting CPU scaling governor to {g}");
        set_cpu_scaling_governor(g)
            .await
            .inspect_err(|message| error!("Error setting CPU scaling governor: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn available_cpu_schedulers(&self) -> fdo::Result<Vec<String>> {
        self.cpu_scheduler_manager
            .get_available_cpu_schedulers()
            .await
            .inspect_err(|message| error!("Error getting available CPU schedulers: {message}"))
            .map(|v| v.into_iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn cpu_scheduler(&self) -> fdo::Result<String> {
        self.cpu_scheduler_manager
            .get_cpu_scheduler()
            .await
            .inspect_err(|message| error!("Error getting CPU scheduler: {message}"))
            .map(|s| s.to_string())
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_cpu_scheduler(
        &mut self,
        scheduler: String,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let s = CpuScheduler::try_from(scheduler.as_str()).map_err(to_zbus_fdo_error)?;
        self.cpu_scheduler_manager
            .set_cpu_scheduler(s)
            .await
            .inspect_err(|message| error!("Error setting CPU scheduler: {message}"))
            .map_err(to_zbus_fdo_error)?;
        Ok(self.cpu_scheduler_changed(&ctx).await?)
    }

    async fn set_cpu_boost_state(&self, state: u32) -> fdo::Result<()> {
        let state = match CPUBoostState::try_from(state) {
            Ok(state) => state,
            Err(err) => return Err(to_zbus_fdo_error(err)),
        };
        info!("Setting CPU boost state to {state}");
        set_cpu_boost_state(state)
            .await
            .inspect_err(|message| error!("Error setting CPU boost state: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_gpu_performance_level(
        &mut self,
        level: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        if !self.order.check_header(header) {
            debug!("SetGpuPerformanceLevel: discarding out of order serial");
            return Ok(());
        }
        let Some(ref driver) = self.gpu_performance_level else {
            return Err(fdo::Error::Failed(String::from(
                "GPU performance settings not configured",
            )));
        };
        info!("Setting GPU performance level to {level}");
        let level = match driver.performance_level_from_str(level) {
            Ok(level) => level,
            Err(e) => return Err(to_zbus_fdo_error(e)),
        };
        driver
            .set_performance_level(level)
            .await
            .inspect_err(|message| error!("Error setting GPU performance level: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_manual_gpu_clock(
        &mut self,
        clocks: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        if !self.order.check_header(header) {
            debug!("SetManualGpuClock: discarding out of order serial");
            return Ok(());
        }
        let Some(ref driver) = self.gpu_performance_level else {
            return Err(fdo::Error::Failed(String::from(
                "GPU performance settings not configured",
            )));
        };
        info!("Setting manual GPU clock to {clocks}");
        driver
            .set_clocks(clocks)
            .await
            .inspect_err(|message| error!("Error setting manual GPU clock: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn set_tdp_limit(
        &mut self,
        limit: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        if !self.order.check_header(header) {
            debug!("SetTdpLimit: discarding out of order serial");
            return Ok(());
        }
        let Some(ref manager) = self.tdp_limit_manager else {
            return Err(fdo::Error::Failed(String::from(
                "TDP limiting not configured",
            )));
        };
        info!("Setting TDP limit to {limit}");
        manager
            .set_tdp_limit(limit)
            .await
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn wifi_debug_mode_state(&self) -> u32 {
        // Get the wifi debug mode
        self.wifi_debug_mode as u32
    }

    async fn set_wifi_debug_mode(
        &mut self,
        mode: u32,
        options: HashMap<&str, zvariant::Value<'_>>,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        // Set the wifi debug mode to mode, using an int for flexibility going forward but only
        // doing things on 0 or 1 for now
        let wanted_mode = match WifiDebugMode::try_from(mode) {
            Ok(mode) => mode,
            Err(e) => return Err(fdo::Error::InvalidArgs(e.to_string())),
        };

        if self.wifi_debug_mode == wanted_mode {
            debug!("Not changing wifi debug mode since it's already set to {wanted_mode}");
            return Ok(());
        }

        let buffer_size = match options
            .get("buffer_size")
            .map(zbus::zvariant::Value::downcast_ref)
        {
            Some(Ok(v)) => v,
            None => 20000,
            Some(Err(e)) => return Err(fdo::Error::InvalidArgs(e.to_string())),
        };
        match set_wifi_debug_mode(
            wanted_mode,
            buffer_size,
            self.should_trace,
            &self.connection,
        )
        .await
        {
            Ok(()) => {
                self.wifi_debug_mode = wanted_mode;
                self.wifi_debug_mode_state_changed(&ctx).await?;
                Ok(())
            }
            Err(e) => {
                error!("Error setting wifi debug mode: {e}");
                Err(to_zbus_fdo_error(e))
            }
        }
    }

    async fn set_wifi_backend(&mut self, backend: u32) -> fdo::Result<()> {
        if self.wifi_debug_mode == WifiDebugMode::Tracing {
            return Err(fdo::Error::Failed(String::from(
                "operation not supported when wifi_debug_mode=tracing",
            )));
        }
        let backend = match WifiBackend::try_from(backend) {
            Ok(backend) => backend,
            Err(e) => return Err(fdo::Error::InvalidArgs(e.to_string())),
        };
        set_wifi_backend(backend, &self.connection)
            .await
            .inspect_err(|message| error!("Error setting wifi backend: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    async fn capture_debug_trace_output(&self) -> fdo::Result<String> {
        Ok(extract_wifi_trace()
            .await
            .inspect_err(|message| error!("Error capturing trace output: {message}"))
            .map_err(to_zbus_fdo_error)?
            .into_os_string()
            .to_string_lossy()
            .into())
    }

    async fn generate_debug_dump(&self) -> fdo::Result<String> {
        Ok(generate_wifi_dump()
            .await
            .inspect_err(|message| error!("Error capturing dump output: {message}"))
            .map_err(to_zbus_fdo_error)?
            .into_os_string()
            .to_string_lossy()
            .into())
    }

    #[zbus(property)]
    async fn inhibit_ds(&self) -> fdo::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.channel
            .send(DaemonCommand::ContextCommand(RootCommand::GetDsInhibit(tx)))
            .await
            .inspect_err(|message| error!("Error sending GetDsInhibit command: {message}"))
            .map_err(to_zbus_fdo_error)?;
        rx.await
            .inspect_err(|message| error!("Error receiving GetDsInhibit reply: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_inhibit_ds(&self, enable: bool) -> zbus::Result<()> {
        self.channel
            .send(DaemonCommand::ContextCommand(RootCommand::SetDsInhibit(
                enable,
            )))
            .await
            .inspect_err(|message| error!("Error sending SetDsInhibit command: {message}"))
            .map_err(to_zbus_error)
    }

    async fn reload_config(&self) -> fdo::Result<()> {
        self.channel
            .send(DaemonCommand::ReadConfig)
            .await
            .inspect_err(|message| error!("Error sending ReadConfig command: {message}"))
            .map_err(to_zbus_fdo_error)
    }

    #[zbus(signal)]
    async fn max_charge_level_changed(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    async fn set_max_charge_level(
        &mut self,
        level: i32,
        #[zbus(connection)] connection: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        if !self.order.check_header(header) {
            debug!("SetMaxChargeLevel: discarding out of order serial");
            return Ok(());
        }
        let written = set_max_charge_level(if level == -1 { 0 } else { level })
            .await
            .map_err(to_zbus_fdo_error)?;
        let connection = connection.clone();
        spawn(async move {
            if let Ok(SysfsWritten::Written(res)) = written.await {
                if let Ok(interface) = connection
                    .object_server()
                    .interface::<_, Self>("/com/steampowered/SteamOSManager1")
                    .await
                {
                    interface.max_charge_level_changed().await?;
                }
                res
            } else {
                Ok(())
            }
        });
        Ok(())
    }

    async fn set_performance_profile(&self, profile: &str) -> fdo::Result<()> {
        let config = device_config().await.map_err(to_zbus_fdo_error)?;
        let config = config
            .as_ref()
            .and_then(|config| config.performance_profile.as_ref())
            .ok_or(fdo::Error::Failed(String::from(
                "No performance platform-profile configured",
            )))?;
        set_platform_profile(&config.platform_profile_name, profile)
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn set_temporary_session(&self, session: &str) -> fdo::Result<()> {
        info!("Setting temporary session to {session}");
        set_temporary_session(session)
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn set_default_session(&self, session: &str) -> fdo::Result<()> {
        info!("Setting default session to {session}");
        set_default_session(session)
            .await
            .map_err(to_zbus_fdo_error)
    }

    async fn clean_temporary_sessions(&self) -> fdo::Result<()> {
        info!("Cleaning temporary sessions");
        clean_temporary_sessions().await.map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn hdmi_cec_can_awaken(&self) -> fdo::Result<bool> {
        let Some(cec_hw) = self.cec_hw.as_ref() else {
            return Ok(false);
        };
        cec_hw.can_awaken().await.map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn hdmi_cec_awaken(&self) -> fdo::Result<bool> {
        let cec_hw = self.cec_hw.as_ref().ok_or(fdo::Error::Failed(String::from(
            "No HDMI CEC hardware configured",
        )))?;
        cec_hw.get_awaken().await.map_err(to_zbus_fdo_error)
    }

    #[zbus(property)]
    async fn set_hdmi_cec_awaken(
        &self,
        awaken: bool,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let cec_hw = self.cec_hw.as_ref().ok_or(fdo::Error::Failed(String::from(
            "No HDMI CEC hardware configured",
        )))?;
        cec_hw.set_awaken(awaken).await.map_err(to_zbus_error)?;
        self.hdmi_cec_awaken_changed(&ctx).await
    }

    #[zbus(property)]
    async fn hdmi_cec_phys_addr(&self) -> fdo::Result<u16> {
        let cec_hw = self.cec_hw.as_ref().ok_or(fdo::Error::Failed(String::from(
            "No HDMI CEC hardware configured",
        )))?;
        Ok(cec_hw
            .get_phys_addr()
            .await
            .map_err(to_zbus_fdo_error)?
            .into())
    }

    #[zbus(property)]
    async fn set_hdmi_cec_phys_addr(
        &self,
        phys_addr: u16,
        #[zbus(signal_emitter)] ctx: SignalEmitter<'_>,
    ) -> zbus::Result<()> {
        let cec_hw = self.cec_hw.as_ref().ok_or(fdo::Error::Failed(String::from(
            "No HDMI CEC hardware configured",
        )))?;
        cec_hw
            .set_phys_addr(phys_addr.into())
            .await
            .map_err(to_zbus_error)?;
        self.hdmi_cec_phys_addr_changed(&ctx).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::daemon::channel;
    use crate::daemon::root::RootContext;
    use crate::gpu::test::{Nodes as GpuNodes, format_clocks, read_clocks};
    use crate::gpu::{
        self, AmdgpuPerformanceLevel, AmdgpuPerformanceLevelDriver, GpuPerformanceLevel,
    };
    use crate::hardware::test::fake_model;
    use crate::platform::{PlatformConfig, ResetConfig};
    use crate::process::test::{code, exit, ok};
    use crate::testing;
    use std::time::Duration;
    use tokio::fs::{create_dir_all, write};
    use tokio::time::sleep;
    use zbus::Connection;

    struct TestHandle {
        h: testing::TestHandle,
        connection: Connection,
    }

    async fn start() -> Result<TestHandle> {
        let mut handle = testing::start();
        fake_model(SteamDeckVariant::Jupiter).await?;
        create_dir_all(crate::path("/etc/NetworkManager/conf.d")).await?;
        write(
            crate::path("/etc/NetworkManager/conf.d/99-valve-wifi-backend.conf"),
            "wifi.backend=iwd\n",
        )
        .await?;
        gpu::test::create_nodes(&GpuNodes::all())
            .await
            .expect("setup");

        let (tx, _rx) = channel::<RootContext>();
        let connection = handle.new_dbus().await?;
        let manager = SteamOSManager::new(connection.clone(), tx).await?;
        connection
            .object_server()
            .at("/com/steampowered/SteamOSManager1", manager)
            .await?;

        sleep(Duration::from_millis(1)).await;

        Ok(TestHandle {
            h: handle,
            connection,
        })
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait PrepareFactoryReset {
        fn prepare_factory_reset(&self, kind: u32) -> zbus::Result<u32>;
    }

    #[tokio::test]
    async fn prepare_factory_reset() {
        let test = start().await.expect("start");

        let config = PlatformConfig {
            factory_reset: Some(ResetConfig::default()),
            ..PlatformConfig::default()
        };
        test.h.test.set_platform_config(config).await;

        let name = test.connection.unique_name().unwrap();
        let proxy = PrepareFactoryResetProxy::new(&test.connection, name.clone())
            .await
            .unwrap();

        test.h.test.set_process_cb(ok).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::All as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::RebootRequired as u32
        );

        test.h.test.set_process_cb(code).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::All as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.set_process_cb(exit).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::All as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.set_process_cb(ok).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::OS as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::RebootRequired as u32
        );

        test.h.test.set_process_cb(code).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::OS as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.set_process_cb(exit).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::OS as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.set_process_cb(ok).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::User as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::RebootRequired as u32
        );

        test.h.test.set_process_cb(code).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::User as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.h.test.set_process_cb(exit).await;
        assert_eq!(
            proxy
                .prepare_factory_reset(FactoryResetKind::User as u32)
                .await
                .unwrap(),
            PrepareFactoryResetResult::Unknown as u32
        );

        test.connection.close().await.unwrap();
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait AlsCalibrationGain {
        #[zbus(property(emits_changed_signal = "false"))]
        fn als_calibration_gain(&self) -> zbus::Result<Vec<f64>>;
    }

    #[tokio::test]
    async fn als_calibration_gain() {
        let test = start().await.expect("start");
        let name = test.connection.unique_name().unwrap();
        let proxy = AlsCalibrationGainProxy::new(&test.connection, name.clone())
            .await
            .unwrap();

        test.h
            .test
            .set_process_cb(|_, _| Ok((0, String::from("0.0\n"))))
            .await;

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[0.0]);

        fake_model(SteamDeckVariant::Galileo)
            .await
            .expect("fake_model");
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[0.0, 0.0]);

        fake_model(SteamDeckVariant::Unknown)
            .await
            .expect("fake_model");
        assert!(proxy.als_calibration_gain().await.unwrap().is_empty());

        fake_model(SteamDeckVariant::Jupiter)
            .await
            .expect("fake_model");

        test.h
            .test
            .set_process_cb(|_, _| Ok((0, String::from("1.0\n"))))
            .await;
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[1.0]);

        test.h
            .test
            .set_process_cb(|_, _| Ok((0, String::from("big\n"))))
            .await;
        assert_eq!(proxy.als_calibration_gain().await.unwrap(), &[-1.0]);

        test.connection.close().await.unwrap();
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait GpuPerformanceLevel {
        fn set_gpu_performance_level(&self, level: String) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn gpu_performance_level() {
        let test = start().await.expect("start");
        let driver = AmdgpuPerformanceLevelDriver {};

        let name = test.connection.unique_name().unwrap();
        let proxy = GpuPerformanceLevelProxy::new(&test.connection, name.clone())
            .await
            .unwrap();
        proxy
            .set_gpu_performance_level(
                GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Low).to_string(),
            )
            .await
            .expect("proxy_set");
        assert_eq!(
            driver.get_performance_level().await.unwrap(),
            GpuPerformanceLevel::Amdgpu(AmdgpuPerformanceLevel::Low)
        );

        test.connection.close().await.unwrap();
    }

    #[zbus::proxy(
        interface = "com.steampowered.SteamOSManager1.RootManager",
        default_path = "/com/steampowered/SteamOSManager1"
    )]
    trait ManualGpuClock {
        fn set_manual_gpu_clock(&self, clocks: u32) -> zbus::Result<()>;
    }

    #[tokio::test]
    async fn manual_gpu_clock() {
        let test = start().await.expect("start");

        let name = test.connection.unique_name().unwrap();
        let proxy = ManualGpuClockProxy::new(&test.connection, name.clone())
            .await
            .unwrap();

        proxy.set_manual_gpu_clock(200).await.expect("proxy_set");
        assert_eq!(read_clocks().await.unwrap(), format_clocks(200));

        test.connection.close().await.unwrap();
    }
}
