/*
 * Copyright © 2023 Collabora Ltd.
 * Copyright © 2024 Valve Software
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::{Result, bail, ensure};
use config::builder::AsyncState;
use config::{ConfigBuilder, FileFormat};
use nix::sys::stat::{self, Mode};
use num_enum::TryFromPrimitive;
use std::ffi::OsStr;
use std::fs::Permissions;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use strum::{Display, EnumString};
use tempfile::Builder as TempFileBuilder;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::sleep;
use tracing::{debug, error};
use udev::{Event, EventType};
use zbus::Connection;

use crate::process::{run_script, script_output};
use crate::systemd::{JobMode, SystemdUnit, daemon_reload};
use crate::udev::single_poll;
use crate::{path, read_config_directory};

const OVERRIDE_CONTENTS: &str = "[Service]
ExecStart=
ExecStart=/usr/lib/iwd/iwd -d
";
const OVERRIDE_FOLDER: &str = "/etc/systemd/system/iwd.service.d";
const OVERRIDE_PATH: &str = "/etc/systemd/system/iwd.service.d/99-valve-override.conf";

// Only use one path for output for now. If needed we can add a timestamp later
// to have multiple files, etc.
const TRACE_CMD_PATH: &str = "/usr/bin/trace-cmd";

const MIN_BUFFER_SIZE: u32 = 100;

const WIFI_BACKEND_PATHS: &[&str] = &[
    "/usr/lib/NetworkManager/conf.d",
    "/etc/NetworkManager/conf.d",
];
const WIFI_BACKEND_FILE: &str = "/etc/NetworkManager/conf.d/99-valve-wifi-backend.conf";

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[repr(u32)]
pub enum WifiDebugMode {
    #[strum(
        to_string = "off",
        serialize = "disable",
        serialize = "disabled",
        serialize = "0"
    )]
    Off = 0,
    Tracing = 1,
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(ascii_case_insensitive)]
#[repr(u32)]
pub enum WifiPowerManagement {
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

impl From<WifiPowerManagement> for NmSettingWirelessPowersave {
    fn from(val: WifiPowerManagement) -> NmSettingWirelessPowersave {
        match val {
            WifiPowerManagement::Disabled => NmSettingWirelessPowersave::Disable,
            WifiPowerManagement::Enabled => NmSettingWirelessPowersave::Enable,
        }
    }
}

#[derive(Display, EnumString, PartialEq, Debug, Copy, Clone, TryFromPrimitive)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[repr(u32)]
pub enum WifiBackend {
    Iwd = 0,
    WPASupplicant = 1,
}

#[derive(PartialEq, Debug, Copy, Clone, Default, TryFromPrimitive)]
#[repr(i64)]
enum NmSettingWirelessPowersave {
    Default = 0,
    Ignore = 1,
    Disable = 2,
    #[default]
    Enable = 3,
}

#[derive(PartialEq, Debug, Clone)]
struct NmWifiSettings {
    backend: WifiBackend,
    powersave: NmSettingWirelessPowersave,
}

pub(crate) async fn setup_iwd_config(want_override: bool) -> std::io::Result<()> {
    // Copy override.conf file into place or out of place depending
    // on install value

    if want_override {
        // Copy it in
        // Make sure the folder exists
        fs::create_dir_all(path(OVERRIDE_FOLDER)).await?;
        // Then write the contents into the file
        fs::write(path(OVERRIDE_PATH), OVERRIDE_CONTENTS).await
    } else {
        // Delete it
        match fs::remove_file(path(OVERRIDE_PATH)).await {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            res => res,
        }
    }
}

async fn restart_iwd(connection: &Connection) -> Result<()> {
    // First reload systemd since we modified the config most likely
    // otherwise we wouldn't be restarting iwd.
    daemon_reload(connection)
        .await
        .inspect_err(|message| error!("restart_iwd: reload systemd got an error: {message}"))?;

    // worked, now restart iwd
    let unit = SystemdUnit::new(connection, "iwd.service").await?;
    unit.restart(JobMode::Fail)
        .await
        .inspect_err(|message| error!("restart_iwd: restart unit got an error: {message}"))?;
    Ok(())
}

async fn stop_tracing() -> Result<()> {
    run_script(TRACE_CMD_PATH, &["stop"]).await?;
    Ok(())
}

async fn start_tracing(buffer_size: u32) -> Result<()> {
    // Start tracing
    let size_str = buffer_size.to_string();
    run_script(
        TRACE_CMD_PATH,
        &["start", "-e", "ath11k_wmi_diag", "-b", &size_str],
    )
    .await
}

fn make_tempfile(prefix: &str) -> Result<(fs::File, PathBuf)> {
    let umask = stat::umask(Mode::from_bits_truncate(0));
    let output = TempFileBuilder::new()
        .prefix(prefix)
        .permissions(Permissions::from_mode(0o666))
        .tempfile()?;
    let (output, path) = output.keep()?;
    let output = fs::File::from_std(output);
    stat::umask(umask);

    Ok((output, path))
}

pub async fn extract_wifi_trace() -> Result<PathBuf> {
    let (_, path) = make_tempfile("wifi-trace-")?;
    run_script(
        "trace-cmd",
        &[OsStr::new("extract"), OsStr::new("-o"), path.as_os_str()],
    )
    .await?;
    Ok(path)
}

pub(crate) async fn set_wifi_debug_mode(
    mode: WifiDebugMode,
    buffer_size: u32,
    should_trace: bool,
    connection: &Connection,
) -> Result<()> {
    match get_wifi_backend().await {
        Ok(WifiBackend::Iwd) => (),
        Ok(backend) => bail!("Setting Wi-Fi debug mode not supported with backend {backend}"),
        Err(e) => return Err(e),
    }

    match mode {
        WifiDebugMode::Off => {
            // If mode is 0 disable wifi debug mode
            // Stop any existing trace and flush to disk.
            if should_trace && let Err(message) = stop_tracing().await {
                bail!("stop_tracing command got an error: {message}");
            }
            // Stop_tracing was successful
            if let Err(message) = setup_iwd_config(false).await {
                bail!("setup_iwd_config false got an error: {message}");
            }
            // setup_iwd_config false worked
            if let Err(message) = restart_iwd(connection).await {
                bail!("restart_iwd got an error: {message}");
            }
        }
        WifiDebugMode::Tracing => {
            ensure!(buffer_size > MIN_BUFFER_SIZE, "Buffer size too small");

            if let Err(message) = setup_iwd_config(true).await {
                bail!("setup_iwd_config true got an error: {message}");
            }
            // setup_iwd_config worked
            if let Err(message) = restart_iwd(connection).await {
                bail!("restart_iwd got an error: {message}");
            }
            // restart_iwd worked
            if should_trace && let Err(message) = start_tracing(buffer_size).await {
                bail!("start_tracing got an error: {message}");
            }
        }
    }
    Ok(())
}

// This function re-creates the network interface if missing.
//
// As described in https://iwd.wiki.kernel.org/interface_lifecycle,#
// iwd destroys and re-creates the network interface, unless configured
// otherwise by "[DriverQuirks] DefaultInterface" or disabled by
// default in the source (it happens for some drivers):
// https://git.kernel.org/pub/scm/network/wireless/iwd.git/tree/src/wiphy.c?h=2.14#n88
//
// The original LCD model has one of the drivers in which this is
// disabled at the source-code level, but in the OLED/Galileo model
// with the ath11k* driver, it is not.
//
// NetworkManager brings up the service providing wifi.backend as
// necessary, but this does not happen when the interface is not
// present.  So in the OLED model, when switching from "iwd" to
// "wpa_supplicant", "iwd" destroys the interface when stopped, NM
// never brings up the newly configured back-end "wpa_supplicant", and
// the system remains disconnected forever.  When switching from
// "wpa_supplicant" to "iwd" it's all right because "wpa_supplicant"
// doesn't destroy the network interface.  And similarly, in the LCD
// model the interface is never destroyed, so NM always brings up the
// service for the corresponding wifi.backend just fine.
//
// Unfortunately iwd does not support "config fragments" and
// NetworkManager does not have config for this option like with
// 'wifi.iwd.autoconnect', so the only possible way is to edit
// '/etc/iwd/main.conf', which is not the cleanest solution.
//
// So, in order to work around this problem, this function re-creates
// the network interface if missing.
//
// Originally discussed in:
// https://gitlab.steamos.cloud/holo/holo/-/merge_requests/747
#[cfg(feature = "wifi-backend-switching")]
async fn ensure_default_interface() -> Result<()> {
    if !list_wifi_interfaces().await?.is_empty() {
        return Ok(());
    }
    let Some(phy) = list_wifi_phys().await?.into_iter().next() else {
        // No Wi-Fi hardware present, so there's no interface to re-create
        debug!("No wifi phys present, skipping interface creation");
        return Ok(());
    };
    debug!("Missing wlan interace, creating it explicitly");
    run_script(
        "/usr/bin/iw",
        &[
            "phy",
            phy.as_str(),
            "interface",
            "add",
            "wlan0",
            "type",
            "station",
        ],
    )
    .await?;
    for _ in 0..10 {
        if !list_wifi_interfaces().await?.is_empty() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    bail!("wlan0 could not be created");
}

async fn get_nm_settings() -> Result<NmWifiSettings> {
    let mut builder = ConfigBuilder::<AsyncState>::default();
    for dir in WIFI_BACKEND_PATHS {
        builder = read_config_directory(builder, path(dir), &["conf"], FileFormat::Ini).await?;
    }
    let config = builder.build().await?;

    let backend;
    if let Some(be) = config
        .get_table("device")
        .ok()
        .and_then(|mut t| t.remove("wifi.backend"))
    {
        let be = be.into_string()?;
        backend = WifiBackend::from_str(be.as_str())?;
    } else {
        bail!("Wi-Fi backend not found in config");
    }

    let powersave;
    if let Some(ps) = config
        .get_table("connection")
        .ok()
        .and_then(|mut t| t.remove("wifi.powersave"))
    {
        let ps = ps.into_int()?;
        powersave = NmSettingWirelessPowersave::try_from(ps)?;
    } else {
        powersave = NmSettingWirelessPowersave::default();
    }

    Ok(NmWifiSettings { powersave, backend })
}

async fn set_nm_settings(
    settings: NmWifiSettings,
    restart: bool,
    connection: &Connection,
) -> Result<()> {
    let NmWifiSettings { powersave, backend } = settings;
    let powersave = powersave as i64;
    let contents =
        format!("[connection]\nwifi.powersave={powersave}\n[device]\nwifi.backend={backend}\n");
    fs::write(path(WIFI_BACKEND_FILE), contents).await?;

    // Stop the other backend and restart NM
    let unit = SystemdUnit::new(connection, "NetworkManager.service").await?;

    if restart {
        #[cfg(feature = "wifi-backend-switching")]
        {
            let backend_unit = SystemdUnit::new(
                connection,
                match backend {
                    WifiBackend::Iwd => "wpa_supplicant.service",
                    WifiBackend::WPASupplicant => "iwd.service",
                },
            )
            .await?;

            unit.stop(JobMode::Fail).await?.wait().await?;
            // These systemd units are not set to enabled permanently, NetworkManager will pull them as necessary
            backend_unit.disable().await?;
            backend_unit.stop(JobMode::Fail).await?.wait().await?;

            // Restart NetworkManager even if this fails, otherwise the system is
            // left with no networking at all
            let res = ensure_default_interface().await;
            unit.start(JobMode::Fail).await?;
            res?;
        }
    } else {
        unit.reload(JobMode::Fail).await?.wait().await?;
    }
    Ok(())
}

pub(crate) async fn get_wifi_backend() -> Result<WifiBackend> {
    Ok(get_nm_settings().await?.backend)
}

#[cfg(feature = "wifi-backend-switching")]
pub(crate) async fn set_wifi_backend(backend: WifiBackend, connection: &Connection) -> Result<()> {
    let mut settings = get_nm_settings().await?;
    settings.backend = backend;
    set_nm_settings(settings, true, connection).await
}

pub(crate) async fn list_wifi_interfaces() -> Result<Vec<String>> {
    let output = script_output("/usr/bin/iw", &["dev"]).await?;
    Ok(output
        .lines()
        .filter_map(|line| match line.trim().split_once(' ') {
            Some(("Interface", name)) => Some(name.to_string()),
            _ => None,
        })
        .collect())
}

async fn list_wifi_phys() -> Result<Vec<String>> {
    let output = script_output("/usr/bin/iw", &["phy"]).await?;
    Ok(output
        .lines()
        .filter_map(|line| match line.trim().split_once(' ') {
            Some(("Wiphy", name)) => Some(name.to_string()),
            _ => None,
        })
        .collect())
}

pub(crate) async fn get_wifi_power_management_state() -> Result<WifiPowerManagement> {
    let mut found_any = false;
    for iface in list_wifi_interfaces().await? {
        let output =
            script_output("/usr/bin/iw", &["dev", iface.as_str(), "get", "power_save"]).await?;
        for line in output.lines() {
            match line.trim() {
                "Power save: on" => return Ok(WifiPowerManagement::Enabled),
                "Power save: off" => found_any = true,
                _ => continue,
            }
        }
    }
    ensure!(found_any, "No interfaces found");
    Ok(WifiPowerManagement::Disabled)
}

pub(crate) async fn set_wifi_power_management_state(
    state: WifiPowerManagement,
    connection: &Connection,
) -> Result<()> {
    // Manually set power management for runtime
    let str_state = match state {
        WifiPowerManagement::Disabled => "off",
        WifiPowerManagement::Enabled => "on",
    };

    for iface in list_wifi_interfaces().await? {
        run_script(
            "/usr/bin/iw",
            &["dev", iface.as_str(), "set", "power_save", str_state],
        )
        .await
        .inspect_err(|message| error!("Error setting Wi-Fi power management state: {message}"))?;
    }

    // ...and inform and reload NetworkManager to take care of it after suspend/wake or reboot
    let mut settings = get_nm_settings().await?;
    settings.powersave = state.into();
    set_nm_settings(settings, false, connection).await
}

async fn generate_wifi_dump_inner() -> Result<PathBuf> {
    fn cb(ev: &Event) -> bool {
        if ev.event_type() != EventType::Add {
            return false;
        }
        let path = ev.syspath();
        let Ok(link) = std::fs::read_link(path.join("failing_device/driver")) else {
            return false;
        };
        link.file_name() == Some(OsStr::new("ath11k_pci"))
    }

    let poller = single_poll("devcoredump", cb, Duration::from_secs(5));
    fs::write(
        path("/sys/kernel/debug/ath11k/pci-0000:03:00.0/simulate_fw_crash"),
        "mhi-rddm\n",
    )
    .await?;
    let devcd = poller?.await??;
    let data = devcd.join("data");
    let (mut output, path) = make_tempfile("wifi-dump-")?;

    {
        let mut dump = fs::File::open(&data).await?;
        let mut buf = [0; 4096];
        loop {
            let read = dump.read(&mut buf).await?;
            if read == 0 {
                break;
            }
            output.write_all(&buf[..read]).await?;
        }
    }

    fs::write(data, "1\n").await?;
    Ok(path)
}

pub(crate) async fn generate_wifi_dump() -> Result<PathBuf> {
    const DEVCD_BLOCK: &str = "/var/lib/steamos-log-submitter/data/devcd-block/ath11k_pci";
    let placeholder = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path(DEVCD_BLOCK))
        .await;
    if let Err(ref err) = placeholder {
        ensure!(
            err.kind() == ErrorKind::NotFound,
            "Cound not create SLS helper block"
        );
    }

    let res = generate_wifi_dump_inner().await;

    if placeholder.is_ok() {
        let _ = fs::remove_file(DEVCD_BLOCK).await;
    }

    res
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::systemd::test::{MockManager, MockUnit};
    use crate::{enum_on_off, enum_roundtrip, testing};
    use std::ffi::OsStr;
    use tokio::fs::{create_dir_all, read_to_string, remove_dir, try_exists, write};

    #[test]
    fn test_wifi_backend_to_string() {
        assert_eq!(WifiBackend::Iwd.to_string(), "iwd");
        assert_eq!(WifiBackend::WPASupplicant.to_string(), "wpa_supplicant");
    }

    #[tokio::test]
    async fn test_setup_iwd_config() {
        let _h = testing::start();

        // Remove with no dir
        assert!(setup_iwd_config(false).await.is_ok());

        create_dir_all(path(OVERRIDE_FOLDER))
            .await
            .expect("create_dir_all");

        // Remove with dir but no file
        assert!(setup_iwd_config(false).await.is_ok());

        // Remove with dir and file
        write(path(OVERRIDE_PATH), "").await.expect("write");
        assert!(try_exists(path(OVERRIDE_PATH)).await.unwrap());

        assert!(setup_iwd_config(false).await.is_ok());
        assert!(!try_exists(path(OVERRIDE_PATH)).await.unwrap());

        // Double remove
        assert!(setup_iwd_config(false).await.is_ok());

        // Create with no dir
        remove_dir(path(OVERRIDE_FOLDER)).await.expect("remove_dir");

        assert!(setup_iwd_config(true).await.is_ok());
        assert_eq!(
            read_to_string(path(OVERRIDE_PATH)).await.unwrap(),
            OVERRIDE_CONTENTS
        );

        // Create with dir
        assert!(setup_iwd_config(false).await.is_ok());
        assert!(setup_iwd_config(true).await.is_ok());
        assert_eq!(
            read_to_string(path(OVERRIDE_PATH)).await.unwrap(),
            OVERRIDE_CONTENTS
        );
    }

    #[tokio::test]
    async fn test_get_nm_settings() {
        let _h = testing::start();

        for dir in WIFI_BACKEND_PATHS {
            create_dir_all(path(dir)).await.expect("create_dir_all");
        }

        assert!(get_nm_settings().await.is_err());

        write(path(WIFI_BACKEND_PATHS[0]).join("test.conf"), "[device]")
            .await
            .expect("write");
        assert!(get_nm_settings().await.is_err());

        write(
            path(WIFI_BACKEND_PATHS[0]).join("test.conf"),
            "[device]\nwifi.backend=iwd\n",
        )
        .await
        .expect("write");
        assert_eq!(
            get_nm_settings().await.unwrap(),
            NmWifiSettings {
                powersave: NmSettingWirelessPowersave::default(),
                backend: WifiBackend::Iwd,
            }
        );

        write(
            path(WIFI_BACKEND_PATHS[0]).join("test.conf"),
            "[connection]\nwifi.powersave=2\n",
        )
        .await
        .expect("write");
        assert!(get_nm_settings().await.is_err());

        write(
            path(WIFI_BACKEND_PATHS[0]).join("test.conf"),
            "[device]\nwifi.backend=iwd\n[connection]\nwifi.powersave=2\n",
        )
        .await
        .expect("write");
        assert_eq!(
            get_nm_settings().await.unwrap(),
            NmWifiSettings {
                powersave: NmSettingWirelessPowersave::try_from(2).unwrap(),
                backend: WifiBackend::Iwd,
            }
        );
    }

    #[tokio::test]
    async fn test_set_nm_settings_no_restart() {
        let mut h = testing::start();
        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");
        object_server
            .at(
                "/org/freedesktop/systemd1/unit/NetworkManager_2eservice",
                MockUnit::default(),
            )
            .await
            .expect("at");

        create_dir_all(path(WIFI_BACKEND_PATHS.last().unwrap()))
            .await
            .expect("create_dir_all");

        assert!(get_nm_settings().await.is_err());

        set_nm_settings(
            NmWifiSettings {
                backend: WifiBackend::WPASupplicant,
                powersave: NmSettingWirelessPowersave::Disable,
            },
            false,
            &connection,
        )
        .await
        .unwrap();

        let contents = read_to_string(path(WIFI_BACKEND_FILE)).await.unwrap();
        assert_eq!(
            contents,
            "[connection]\nwifi.powersave=2\n[device]\nwifi.backend=wpa_supplicant\n"
        );

        assert_eq!(
            get_nm_settings().await.unwrap(),
            NmWifiSettings {
                backend: WifiBackend::WPASupplicant,
                powersave: NmSettingWirelessPowersave::Disable,
            }
        );
    }

    #[cfg(feature = "wifi-backend-switching")]
    #[tokio::test]
    async fn test_set_nm_settings_restart() {
        let mut h = testing::start();

        let mut iwd = MockUnit::default();
        iwd.active = String::from("active");
        iwd.unit_file = String::from("enabled");

        let mut wpa_supplicant = MockUnit::default();
        wpa_supplicant.active = String::from("inactive");
        wpa_supplicant.unit_file = String::from("disabled");

        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");
        object_server
            .at(
                "/org/freedesktop/systemd1/unit/NetworkManager_2eservice",
                MockUnit::default(),
            )
            .await
            .expect("at");
        object_server
            .at("/org/freedesktop/systemd1/unit/iwd_2eservice", iwd)
            .await
            .expect("at");
        let iwd = object_server
            .interface::<_, MockUnit>("/org/freedesktop/systemd1/unit/iwd_2eservice")
            .await
            .unwrap();
        object_server
            .at(
                "/org/freedesktop/systemd1/unit/wpa_supplicant_2eservice",
                wpa_supplicant,
            )
            .await
            .expect("at");
        let wpa_supplicant = object_server
            .interface::<_, MockUnit>("/org/freedesktop/systemd1/unit/wpa_supplicant_2eservice")
            .await
            .unwrap();

        fn process_output(executable: &OsStr, args: &[&OsStr]) -> Result<(i32, String)> {
            ensure!(executable.to_string_lossy() == "/usr/bin/iw", "Not iw");
            ensure!(args[0] == "dev", "Not dev");
            if args.len() > 2 {
                bail!("Unknown query");
            }
            Ok((0, String::from("Interface wlan0")))
        }
        h.test.set_process_cb(process_output).await;

        create_dir_all(path(WIFI_BACKEND_PATHS.last().unwrap()))
            .await
            .expect("create_dir_all");

        assert!(get_nm_settings().await.is_err());
        assert_eq!(iwd.get().await.active, "active");
        assert_eq!(iwd.get().await.unit_file, "enabled");
        assert_eq!(wpa_supplicant.get().await.active, "inactive");
        assert_eq!(wpa_supplicant.get().await.unit_file, "disabled");

        set_nm_settings(
            NmWifiSettings {
                backend: WifiBackend::WPASupplicant,
                powersave: NmSettingWirelessPowersave::Disable,
            },
            true,
            &connection,
        )
        .await
        .unwrap();

        let contents = read_to_string(path(WIFI_BACKEND_FILE)).await.unwrap();
        assert_eq!(
            contents,
            "[connection]\nwifi.powersave=2\n[device]\nwifi.backend=wpa_supplicant\n"
        );

        assert_eq!(
            get_nm_settings().await.unwrap(),
            NmWifiSettings {
                backend: WifiBackend::WPASupplicant,
                powersave: NmSettingWirelessPowersave::Disable,
            }
        );
        assert_eq!(iwd.get().await.active, "inactive");
        assert_eq!(iwd.get().await.unit_file, "disabled");
        // wpa_supplicant is started by NetworkManager so we can't test this
    }

    #[tokio::test]
    async fn test_set_nm_settings_restart_no_wifi() {
        let mut h = testing::start();

        let mut iwd = MockUnit::default();
        iwd.active = String::from("active");
        iwd.unit_file = String::from("enabled");

        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");
        object_server
            .at(
                "/org/freedesktop/systemd1/unit/NetworkManager_2eservice",
                MockUnit::default(),
            )
            .await
            .expect("at");
        let network_manager = object_server
            .interface::<_, MockUnit>("/org/freedesktop/systemd1/unit/NetworkManager_2eservice")
            .await
            .unwrap();
        object_server
            .at("/org/freedesktop/systemd1/unit/iwd_2eservice", iwd)
            .await
            .expect("at");

        fn process_output(executable: &OsStr, args: &[&OsStr]) -> Result<(i32, String)> {
            ensure!(executable.to_string_lossy() == "/usr/bin/iw", "Not iw");
            match args[0].to_str() {
                Some("dev") if args.len() == 1 => Ok((0, String::new())),
                Some("phy") if args.len() == 1 => Ok((0, String::new())),
                _ => bail!("Unknown query"),
            }
        }
        h.test.set_process_cb(process_output).await;

        create_dir_all(path(WIFI_BACKEND_PATHS.last().unwrap()))
            .await
            .expect("create_dir_all");

        set_nm_settings(
            NmWifiSettings {
                backend: WifiBackend::WPASupplicant,
                powersave: NmSettingWirelessPowersave::Disable,
            },
            true,
            &connection,
        )
        .await
        .unwrap();

        assert_eq!(network_manager.get().await.active, "active");
    }

    #[tokio::test]
    async fn test_set_nm_settings_restart_create_interface_fails() {
        let mut h = testing::start();

        let mut iwd = MockUnit::default();
        iwd.active = String::from("active");
        iwd.unit_file = String::from("enabled");

        let connection = h.new_dbus().await.expect("dbus");
        connection
            .request_name("org.freedesktop.systemd1")
            .await
            .expect("request_name");
        let object_server = connection.object_server();
        object_server
            .at("/org/freedesktop/systemd1", MockManager::default())
            .await
            .expect("at");
        object_server
            .at(
                "/org/freedesktop/systemd1/unit/NetworkManager_2eservice",
                MockUnit::default(),
            )
            .await
            .expect("at");
        let network_manager = object_server
            .interface::<_, MockUnit>("/org/freedesktop/systemd1/unit/NetworkManager_2eservice")
            .await
            .unwrap();
        object_server
            .at("/org/freedesktop/systemd1/unit/iwd_2eservice", iwd)
            .await
            .expect("at");

        fn process_output(executable: &OsStr, args: &[&OsStr]) -> Result<(i32, String)> {
            ensure!(executable.to_string_lossy() == "/usr/bin/iw", "Not iw");
            match args[0].to_str() {
                Some("dev") if args.len() == 1 => Ok((0, String::new())),
                Some("phy") if args.len() == 1 => Ok((0, String::from("Wiphy phy0"))),
                Some("phy") => Ok((1, String::new())),
                _ => bail!("Unknown query"),
            }
        }
        h.test.set_process_cb(process_output).await;

        create_dir_all(path(WIFI_BACKEND_PATHS.last().unwrap()))
            .await
            .expect("create_dir_all");

        assert!(
            set_nm_settings(
                NmWifiSettings {
                    backend: WifiBackend::WPASupplicant,
                    powersave: NmSettingWirelessPowersave::Disable,
                },
                true,
                &connection,
            )
            .await
            .is_err()
        );

        // NetworkManager should still be brought back up
        assert_eq!(network_manager.get().await.active, "active");
    }

    #[tokio::test]
    async fn test_get_wifi_backend() {
        let _h = testing::start();

        for dir in WIFI_BACKEND_PATHS {
            create_dir_all(path(dir)).await.expect("create_dir_all");
        }

        assert!(get_wifi_backend().await.is_err());

        write(path(WIFI_BACKEND_PATHS[0]).join("test.conf"), "[device]")
            .await
            .expect("write");
        assert!(get_wifi_backend().await.is_err());

        write(
            path(WIFI_BACKEND_PATHS[0]).join("test.conf"),
            "[device]\nwifi.backend=fake\n",
        )
        .await
        .expect("write");
        assert!(get_wifi_backend().await.is_err());

        write(
            path(WIFI_BACKEND_PATHS[0]).join("test.conf"),
            "[device]\nwifi.backend=iwd\n",
        )
        .await
        .expect("write");
        assert_eq!(get_wifi_backend().await.unwrap(), WifiBackend::Iwd);

        write(
            path(WIFI_BACKEND_PATHS[0]).join("test.conf"),
            "[device]\nwifi.backend=wpa_supplicant\n",
        )
        .await
        .expect("write");
        assert_eq!(
            get_wifi_backend().await.unwrap(),
            WifiBackend::WPASupplicant
        );
    }

    #[tokio::test]
    async fn test_power_management_enabled() {
        let h = testing::start();

        fn process_output(executable: &OsStr, args: &[&OsStr]) -> Result<(i32, String)> {
            ensure!(executable.to_string_lossy() == "/usr/bin/iw", "Not iw");
            ensure!(args[0] == "dev", "Not dev");
            if args.len() < 2 {
                return Ok((0, String::from("Interface eth0")));
            }
            ensure!(args[1] == "eth0", "Not eth0");
            ensure!(args[3] == "power_save", "Not power_save");
            match args[2].to_str() {
                Some("get") => Ok((0, String::from("Power save: on"))),
                _ => bail!("Unknown query"),
            }
        }
        h.test.set_process_cb(process_output).await;

        assert_eq!(
            get_wifi_power_management_state().await.expect("get"),
            WifiPowerManagement::Enabled
        );
    }

    #[tokio::test]
    async fn test_power_management_disabled() {
        let h = testing::start();

        fn process_output(executable: &OsStr, args: &[&OsStr]) -> Result<(i32, String)> {
            ensure!(executable.to_string_lossy() == "/usr/bin/iw", "Not iw");
            ensure!(args[0] == "dev", "Not dev");
            if args.len() < 2 {
                return Ok((0, String::from("Interface eth0")));
            }
            ensure!(args[1] == "eth0", "Not eth0");
            ensure!(args[3] == "power_save", "Not power_save");
            match args[2].to_str() {
                Some("get") => Ok((0, String::from("Power save: off"))),
                _ => bail!("Unknown query"),
            }
        }
        h.test.set_process_cb(process_output).await;

        assert_eq!(
            get_wifi_power_management_state().await.expect("get"),
            WifiPowerManagement::Disabled
        );
    }

    #[tokio::test]
    async fn test_power_management_multi_iface() {
        let h = testing::start();

        fn process_output(executable: &OsStr, args: &[&OsStr]) -> Result<(i32, String)> {
            ensure!(executable.to_string_lossy() == "/usr/bin/iw", "Not iw");
            ensure!(args[0] == "dev", "Not dev");
            if args.len() < 2 {
                return Ok((0, String::from("Interface eth0\nInterface eth1")));
            }
            ensure!(args[3] == "power_save", "Not power_save");
            match args[1].to_str() {
                Some("eth0") => Ok((0, String::from("Power save: off"))),
                Some("eth1") => Ok((0, String::from("Power save: on"))),
                _ => bail!("Unknown query"),
            }
        }
        h.test.set_process_cb(process_output).await;

        assert_eq!(
            get_wifi_power_management_state().await.expect("get"),
            WifiPowerManagement::Enabled
        );
    }

    #[test]
    fn wifi_debug_mode_roundtrip() {
        enum_roundtrip!(WifiDebugMode {
            0: u32 = Off,
            1: u32 = Tracing,
            "off": str = Off,
            "tracing": str = Tracing,
        });
        assert!(WifiDebugMode::try_from(2).is_err());
        assert!(WifiDebugMode::from_str("onf").is_err());
    }

    #[test]
    fn wifi_power_management_roundtrip() {
        enum_roundtrip!(WifiPowerManagement {
            0: u32 = Disabled,
            1: u32 = Enabled,
            "disabled": str = Disabled,
            "enabled": str = Enabled,
        });
        enum_on_off!(WifiPowerManagement => (Enabled, Disabled));
        assert!(WifiPowerManagement::try_from(2).is_err());
        assert!(WifiPowerManagement::from_str("onf").is_err());
    }

    #[test]
    fn wifi_backend_roundtrip() {
        enum_roundtrip!(WifiBackend {
            0: u32 = Iwd,
            1: u32 = WPASupplicant,
            "iwd": str = Iwd,
            "wpa_supplicant": str = WPASupplicant,
        });
        assert!(WifiBackend::try_from(2).is_err());
        assert!(WifiBackend::from_str("iwl").is_err());
    }

    #[tokio::test]
    async fn trace_extract() {
        let h = testing::start();

        fn process_output(_: &OsStr, args: &[&OsStr]) -> Result<(i32, String)> {
            assert_eq!(args[0], OsStr::new("extract"));
            assert_eq!(args[1], OsStr::new("-o"));
            std::fs::write(args[2], b"output").unwrap();
            Ok((0, String::new()))
        }
        h.test.set_process_cb(process_output).await;

        let pathbuf = extract_wifi_trace().await.unwrap();

        assert_eq!(fs::read_to_string(&pathbuf).await.unwrap(), "output");
        fs::remove_file(pathbuf).await.unwrap();
    }
}
