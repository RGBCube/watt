use crate::config::AppConfig;
use crate::core::{BatteryInfo, CpuCoreInfo, CpuGlobalInfo, SystemInfo, SystemLoad, SystemReport};
use crate::cpu::get_logical_core_count;
use crate::util::error::SysMonitorError;
use log::debug;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    thread,
    time::Duration,
    time::SystemTime,
};

pub type Result<T, E = SysMonitorError> = std::result::Result<T, E>;

// Read a sysfs file to a string, trimming whitespace
fn read_sysfs_file_trimmed(path: impl AsRef<Path>) -> Result<String> {
    fs::read_to_string(path.as_ref())
        .map(|s| s.trim().to_string())
        .map_err(|e| {
            SysMonitorError::ReadError(format!("Path: {:?}, Error: {}", path.as_ref().display(), e))
        })
}

// Read a sysfs file and parse it to a specific type
fn read_sysfs_value<T: FromStr>(path: impl AsRef<Path>) -> Result<T> {
    let content = read_sysfs_file_trimmed(path.as_ref())?;
    content.parse::<T>().map_err(|_| {
        SysMonitorError::ParseError(format!(
            "Could not parse '{}' from {:?}",
            content,
            path.as_ref().display()
        ))
    })
}

pub fn get_system_info() -> SystemInfo {
    let cpu_model = get_cpu_model().unwrap_or_else(|_| "Unknown".to_string());
    let linux_distribution = get_linux_distribution().unwrap_or_else(|_| "Unknown".to_string());
    let architecture = std::env::consts::ARCH.to_string();

    SystemInfo {
        cpu_model,
        architecture,
        linux_distribution,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CpuTimes {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl CpuTimes {
    const fn total_time(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }

    const fn idle_time(&self) -> u64 {
        self.idle + self.iowait
    }
}

fn read_all_cpu_times() -> Result<HashMap<u32, CpuTimes>> {
    let content = fs::read_to_string("/proc/stat").map_err(SysMonitorError::Io)?;
    let mut cpu_times_map = HashMap::new();

    for line in content.lines() {
        if line.starts_with("cpu") && line.chars().nth(3).is_some_and(|c| c.is_ascii_digit()) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 11 {
                return Err(SysMonitorError::ProcStatParseError(format!(
                    "Line too short: {line}"
                )));
            }

            let core_id_str = &parts[0][3..];
            let core_id = core_id_str.parse::<u32>().map_err(|_| {
                SysMonitorError::ProcStatParseError(format!(
                    "Failed to parse core_id: {core_id_str}"
                ))
            })?;

            let times = CpuTimes {
                user: parts[1].parse().map_err(|_| {
                    SysMonitorError::ProcStatParseError(format!(
                        "Failed to parse user time: {}",
                        parts[1]
                    ))
                })?,
                nice: parts[2].parse().map_err(|_| {
                    SysMonitorError::ProcStatParseError(format!(
                        "Failed to parse nice time: {}",
                        parts[2]
                    ))
                })?,
                system: parts[3].parse().map_err(|_| {
                    SysMonitorError::ProcStatParseError(format!(
                        "Failed to parse system time: {}",
                        parts[3]
                    ))
                })?,
                idle: parts[4].parse().map_err(|_| {
                    SysMonitorError::ProcStatParseError(format!(
                        "Failed to parse idle time: {}",
                        parts[4]
                    ))
                })?,
                iowait: parts[5].parse().map_err(|_| {
                    SysMonitorError::ProcStatParseError(format!(
                        "Failed to parse iowait time: {}",
                        parts[5]
                    ))
                })?,
                irq: parts[6].parse().map_err(|_| {
                    SysMonitorError::ProcStatParseError(format!(
                        "Failed to parse irq time: {}",
                        parts[6]
                    ))
                })?,
                softirq: parts[7].parse().map_err(|_| {
                    SysMonitorError::ProcStatParseError(format!(
                        "Failed to parse softirq time: {}",
                        parts[7]
                    ))
                })?,
                steal: parts[8].parse().map_err(|_| {
                    SysMonitorError::ProcStatParseError(format!(
                        "Failed to parse steal time: {}",
                        parts[8]
                    ))
                })?,
            };
            cpu_times_map.insert(core_id, times);
        }
    }
    Ok(cpu_times_map)
}

pub fn get_cpu_core_info(
    core_id: u32,
    prev_times: &CpuTimes,
    current_times: &CpuTimes,
) -> Result<CpuCoreInfo> {
    let cpufreq_path = PathBuf::from(format!("/sys/devices/system/cpu/cpu{core_id}/cpufreq/"));

    let current_frequency_mhz = read_sysfs_value::<u32>(cpufreq_path.join("scaling_cur_freq"))
        .map(|khz| khz / 1000)
        .ok();
    let min_frequency_mhz = read_sysfs_value::<u32>(cpufreq_path.join("scaling_min_freq"))
        .map(|khz| khz / 1000)
        .ok();
    let max_frequency_mhz = read_sysfs_value::<u32>(cpufreq_path.join("scaling_max_freq"))
        .map(|khz| khz / 1000)
        .ok();

    // Temperature detection.
    // Should be generic enough to be able to support for multiple hardware sensors
    // with the possibility of extending later down the road.
    let mut temperature_celsius: Option<f32> = None;

    // Search for temperature in hwmon devices
    if let Ok(hwmon_dir) = fs::read_dir("/sys/class/hwmon") {
        for hw_entry in hwmon_dir.flatten() {
            let hw_path = hw_entry.path();

            // Check hwmon driver name
            if let Ok(name) = read_sysfs_file_trimmed(hw_path.join("name")) {
                // Intel CPU temperature driver
                if name == "coretemp" {
                    if let Some(temp) = get_temperature_for_core(&hw_path, core_id, "Core") {
                        temperature_celsius = Some(temp);
                        break;
                    }
                }
                // AMD CPU temperature driver
                // TODO: 'zenergy' can also report those stats, I think?
                else if name == "k10temp" || name == "zenpower" || name == "amdgpu" {
                    // AMD's k10temp doesn't always label cores individually
                    // First try to find core-specific temps
                    if let Some(temp) = get_temperature_for_core(&hw_path, core_id, "Tdie") {
                        temperature_celsius = Some(temp);
                        break;
                    }

                    // Try Tctl temperature (CPU control temp)
                    if let Some(temp) = get_generic_sensor_temperature(&hw_path, "Tctl") {
                        temperature_celsius = Some(temp);
                        break;
                    }

                    // Try CPU temperature
                    if let Some(temp) = get_generic_sensor_temperature(&hw_path, "CPU") {
                        temperature_celsius = Some(temp);
                        break;
                    }

                    // Fall back to any available temperature input without a specific label
                    temperature_celsius = get_fallback_temperature(&hw_path);
                    if temperature_celsius.is_some() {
                        break;
                    }
                }
                // Other CPU temperature drivers
                else if name.contains("cpu") || name.contains("temp") {
                    // Try to find a label that matches this core
                    if let Some(temp) = get_temperature_for_core(&hw_path, core_id, "Core") {
                        temperature_celsius = Some(temp);
                        break;
                    }

                    // Fall back to any temperature reading if specific core not found
                    temperature_celsius = get_fallback_temperature(&hw_path);
                    if temperature_celsius.is_some() {
                        break;
                    }
                }
            }
        }
    }

    // Try /sys/devices/platform paths for thermal zones as a last resort
    if temperature_celsius.is_none() {
        if let Ok(thermal_zones) = fs::read_dir("/sys/devices/virtual/thermal") {
            for entry in thermal_zones.flatten() {
                let zone_path = entry.path();
                let name = entry.file_name().into_string().unwrap_or_default();

                if name.starts_with("thermal_zone") {
                    // Try to match by type
                    if let Ok(zone_type) = read_sysfs_file_trimmed(zone_path.join("type")) {
                        if zone_type.contains("cpu")
                            || zone_type.contains("x86")
                            || zone_type.contains("core")
                        {
                            if let Ok(temp_mc) = read_sysfs_value::<i32>(zone_path.join("temp")) {
                                temperature_celsius = Some(temp_mc as f32 / 1000.0);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    let usage_percent: Option<f32> = {
        let prev_idle = prev_times.idle_time();
        let current_idle = current_times.idle_time();

        let prev_total = prev_times.total_time();
        let current_total = current_times.total_time();

        let total_diff = current_total.saturating_sub(prev_total);
        let idle_diff = current_idle.saturating_sub(prev_idle);

        // Avoid division by zero if no time has passed or counters haven't changed
        if total_diff == 0 {
            None
        } else {
            let usage = 100.0 * (1.0 - (idle_diff as f32 / total_diff as f32));
            Some(usage.clamp(0.0, 100.0)) // clamp between 0 and 100
        }
    };

    Ok(CpuCoreInfo {
        core_id,
        current_frequency_mhz,
        min_frequency_mhz,
        max_frequency_mhz,
        usage_percent,
        temperature_celsius,
    })
}

/// Finds core-specific temperature
fn get_temperature_for_core(hw_path: &Path, core_id: u32, label_prefix: &str) -> Option<f32> {
    for i in 1..=32 {
        // Increased range to handle systems with many sensors
        let label_path = hw_path.join(format!("temp{i}_label"));
        let input_path = hw_path.join(format!("temp{i}_input"));

        if label_path.exists() && input_path.exists() {
            if let Ok(label) = read_sysfs_file_trimmed(&label_path) {
                // Match various common label formats:
                // "Core X", "core X", "Core-X", "CPU Core X", etc.
                let core_pattern = format!("{label_prefix} {core_id}");
                let alt_pattern = format!("{label_prefix}-{core_id}");

                if label.eq_ignore_ascii_case(&core_pattern)
                    || label.eq_ignore_ascii_case(&alt_pattern)
                    || label
                        .to_lowercase()
                        .contains(&format!("core {core_id}").to_lowercase())
                {
                    if let Ok(temp_mc) = read_sysfs_value::<i32>(&input_path) {
                        return Some(temp_mc as f32 / 1000.0);
                    }
                }
            }
        }
    }
    None
}

// Finds generic sensor temperatures by label
fn get_generic_sensor_temperature(hw_path: &Path, label_name: &str) -> Option<f32> {
    for i in 1..=32 {
        let label_path = hw_path.join(format!("temp{i}_label"));
        let input_path = hw_path.join(format!("temp{i}_input"));

        if label_path.exists() && input_path.exists() {
            if let Ok(label) = read_sysfs_file_trimmed(&label_path) {
                if label.eq_ignore_ascii_case(label_name)
                    || label.to_lowercase().contains(&label_name.to_lowercase())
                {
                    if let Ok(temp_mc) = read_sysfs_value::<i32>(&input_path) {
                        return Some(temp_mc as f32 / 1000.0);
                    }
                }
            }
        } else if !label_path.exists() && input_path.exists() {
            // Some sensors might not have labels but still have valid temp inputs
            if let Ok(temp_mc) = read_sysfs_value::<i32>(&input_path) {
                return Some(temp_mc as f32 / 1000.0);
            }
        }
    }
    None
}

// Fallback to any temperature reading from a sensor
fn get_fallback_temperature(hw_path: &Path) -> Option<f32> {
    for i in 1..=32 {
        let input_path = hw_path.join(format!("temp{i}_input"));

        if input_path.exists() {
            if let Ok(temp_mc) = read_sysfs_value::<i32>(&input_path) {
                return Some(temp_mc as f32 / 1000.0);
            }
        }
    }
    None
}

pub fn get_all_cpu_core_info() -> Result<Vec<CpuCoreInfo>> {
    let initial_cpu_times = read_all_cpu_times()?;
    thread::sleep(Duration::from_millis(250)); // interval for CPU usage calculation
    let final_cpu_times = read_all_cpu_times()?;

    let num_cores = get_logical_core_count()
        .map_err(|_| SysMonitorError::ReadError("Could not get the number of cores".to_string()))?;

    let mut core_infos = Vec::with_capacity(num_cores as usize);

    for core_id in 0..num_cores {
        if let (Some(prev), Some(curr)) = (
            initial_cpu_times.get(&core_id),
            final_cpu_times.get(&core_id),
        ) {
            match get_cpu_core_info(core_id, prev, curr) {
                Ok(info) => core_infos.push(info),
                Err(e) => {
                    // Log or handle error for a single core, maybe push a partial info or skip
                    eprintln!("Error getting info for core {core_id}: {e}");
                }
            }
        } else {
            // Log or handle missing times for a core
            eprintln!("Missing CPU time data for core {core_id}");
        }
    }
    Ok(core_infos)
}

pub fn get_cpu_global_info(cpu_cores: &[CpuCoreInfo]) -> CpuGlobalInfo {
    // Find a valid CPU to read global settings from
    // Try cpu0 first, then fall back to any available CPU with cpufreq
    let mut cpufreq_base_path_buf = PathBuf::from("/sys/devices/system/cpu/cpu0/cpufreq/");

    if !cpufreq_base_path_buf.exists() {
        let core_count = get_logical_core_count().unwrap_or_else(|e| {
            eprintln!("Warning: {e}");
            0
        });

        for i in 0..core_count {
            let test_path = PathBuf::from(format!("/sys/devices/system/cpu/cpu{i}/cpufreq/"));
            if test_path.exists() {
                cpufreq_base_path_buf = test_path;
                break; // Exit the loop as soon as we find a valid path
            }
        }
    }

    let turbo_status_path = Path::new("/sys/devices/system/cpu/intel_pstate/no_turbo");
    let boost_path = Path::new("/sys/devices/system/cpu/cpufreq/boost");

    let current_governor = if cpufreq_base_path_buf.join("scaling_governor").exists() {
        read_sysfs_file_trimmed(cpufreq_base_path_buf.join("scaling_governor")).ok()
    } else {
        None
    };

    let available_governors = if cpufreq_base_path_buf
        .join("scaling_available_governors")
        .exists()
    {
        read_sysfs_file_trimmed(cpufreq_base_path_buf.join("scaling_available_governors"))
            .map_or_else(
                |_| vec![],
                |s| s.split_whitespace().map(String::from).collect(),
            )
    } else {
        vec![]
    };

    let turbo_status = if turbo_status_path.exists() {
        // 0 means turbo enabled, 1 means disabled for intel_pstate
        read_sysfs_value::<u8>(turbo_status_path)
            .map(|val| val == 0)
            .ok()
    } else if boost_path.exists() {
        // 1 means turbo enabled, 0 means disabled for generic cpufreq boost
        read_sysfs_value::<u8>(boost_path).map(|val| val == 1).ok()
    } else {
        None
    };

    // EPP (Energy Performance Preference)
    let energy_perf_pref =
        read_sysfs_file_trimmed(cpufreq_base_path_buf.join("energy_performance_preference")).ok();

    // EPB (Energy Performance Bias)
    let energy_perf_bias =
        read_sysfs_file_trimmed(cpufreq_base_path_buf.join("energy_performance_bias")).ok();

    let platform_profile = read_sysfs_file_trimmed("/sys/firmware/acpi/platform_profile").ok();

    // Calculate average CPU temperature from the core temperatures
    let average_temperature_celsius = if cpu_cores.is_empty() {
        None
    } else {
        // Filter cores with temperature readings, then calculate average
        let cores_with_temp: Vec<&CpuCoreInfo> = cpu_cores
            .iter()
            .filter(|core| core.temperature_celsius.is_some())
            .collect();

        if cores_with_temp.is_empty() {
            None
        } else {
            // Sum up all temperatures and divide by count
            let sum: f32 = cores_with_temp
                .iter()
                .map(|core| core.temperature_celsius.unwrap())
                .sum();
            Some(sum / cores_with_temp.len() as f32)
        }
    };

    // Return the constructed CpuGlobalInfo
    CpuGlobalInfo {
        current_governor,
        available_governors,
        turbo_status,
        epp: energy_perf_pref,
        epb: energy_perf_bias,
        platform_profile,
        average_temperature_celsius,
    }
}

pub fn get_battery_info(config: &AppConfig) -> Result<Vec<BatteryInfo>> {
    let mut batteries = Vec::new();
    let power_supply_path = Path::new("/sys/class/power_supply");

    if !power_supply_path.exists() {
        return Ok(batteries); // no power supply directory
    }

    let ignored_supplies = config.ignored_power_supplies.clone().unwrap_or_default();

    // Determine overall AC connection status
    let mut overall_ac_connected = false;
    for entry in fs::read_dir(power_supply_path)? {
        let entry = entry?;
        let ps_path = entry.path();
        let name = entry.file_name().into_string().unwrap_or_default();

        // Check for AC adapter type (common names: AC, ACAD, ADP)
        if let Ok(ps_type) = read_sysfs_file_trimmed(ps_path.join("type")) {
            if ps_type == "Mains"
                || ps_type == "USB_PD_DRP"
                || ps_type == "USB_PD"
                || ps_type == "USB_DCP"
                || ps_type == "USB_CDP"
                || ps_type == "USB_ACA"
            {
                // USB types can also provide power
                if let Ok(online) = read_sysfs_value::<u8>(ps_path.join("online")) {
                    if online == 1 {
                        overall_ac_connected = true;
                        break;
                    }
                }
            }
        } else if name.starts_with("AC") || name.contains("ACAD") || name.contains("ADP") {
            // Fallback for type file missing
            if let Ok(online) = read_sysfs_value::<u8>(ps_path.join("online")) {
                if online == 1 {
                    overall_ac_connected = true;
                    break;
                }
            }
        }
    }

    // No AC adapter detected but we're on a desktop system
    // Default to AC power for desktops
    if !overall_ac_connected {
        overall_ac_connected = is_likely_desktop_system();
    }

    for entry in fs::read_dir(power_supply_path)? {
        let entry = entry?;
        let ps_path = entry.path();
        let name = entry.file_name().into_string().unwrap_or_default();

        if ignored_supplies.contains(&name) {
            continue;
        }

        if let Ok(ps_type) = read_sysfs_file_trimmed(ps_path.join("type")) {
            if ps_type == "Battery" {
                // Skip peripheral batteries that aren't real laptop batteries
                if is_peripheral_battery(&ps_path, &name) {
                    debug!("Skipping peripheral battery: {name}");
                    continue;
                }

                let status_str = read_sysfs_file_trimmed(ps_path.join("status")).ok();
                let capacity_percent = read_sysfs_value::<u8>(ps_path.join("capacity")).ok();

                let power_rate_watts = if ps_path.join("power_now").exists() {
                    read_sysfs_value::<i32>(ps_path.join("power_now")) // uW
                        .map(|uw| uw as f32 / 1_000_000.0)
                        .ok()
                } else if ps_path.join("current_now").exists()
                    && ps_path.join("voltage_now").exists()
                {
                    let current_ua = read_sysfs_value::<i32>(ps_path.join("current_now")).ok(); // uA
                    let voltage_uv = read_sysfs_value::<i32>(ps_path.join("voltage_now")).ok(); // uV
                    if let (Some(c), Some(v)) = (current_ua, voltage_uv) {
                        // Power (W) = (Voltage (V) * Current (A))
                        // (v / 1e6 V) * (c / 1e6 A) = (v * c / 1e12) W
                        Some((f64::from(c) * f64::from(v) / 1_000_000_000_000.0) as f32)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let charge_start_threshold =
                    read_sysfs_value::<u8>(ps_path.join("charge_control_start_threshold")).ok();
                let charge_stop_threshold =
                    read_sysfs_value::<u8>(ps_path.join("charge_control_end_threshold")).ok();

                batteries.push(BatteryInfo {
                    name: name.clone(),
                    ac_connected: overall_ac_connected,
                    charging_state: status_str,
                    capacity_percent,
                    power_rate_watts,
                    charge_start_threshold,
                    charge_stop_threshold,
                });
            }
        }
    }

    // If we found no batteries but have power supplies, we're likely on a desktop
    if batteries.is_empty() && overall_ac_connected {
        debug!("No laptop batteries found, likely a desktop system");
    }

    Ok(batteries)
}

/// Check if a battery is likely a peripheral (mouse, keyboard, etc) not a laptop battery
fn is_peripheral_battery(ps_path: &Path, name: &str) -> bool {
    // Convert name to lowercase once for case-insensitive matching
    let name_lower = name.to_lowercase();

    // Common peripheral battery names
    if name_lower.contains("mouse")
        || name_lower.contains("keyboard")
        || name_lower.contains("trackpad")
        || name_lower.contains("gamepad")
        || name_lower.contains("controller")
        || name_lower.contains("headset")
        || name_lower.contains("headphone")
    {
        return true;
    }

    // Small capacity batteries are likely not laptop batteries
    if let Ok(energy_full) = read_sysfs_value::<i32>(ps_path.join("energy_full")) {
        // Most laptop batteries are at least 20,000,000 µWh (20 Wh)
        // Peripheral batteries are typically much smaller
        if energy_full < 10_000_000 {
            // 10 Wh in µWh
            return true;
        }
    }

    // Check for model name that indicates a peripheral
    if let Ok(model) = read_sysfs_file_trimmed(ps_path.join("model_name")) {
        if model.contains("bluetooth") || model.contains("wireless") {
            return true;
        }
    }

    false
}

/// Determine if this is likely a desktop system rather than a laptop
fn is_likely_desktop_system() -> bool {
    // Check for DMI system type information
    if let Ok(chassis_type) = fs::read_to_string("/sys/class/dmi/id/chassis_type") {
        let chassis_type = chassis_type.trim();

        // Chassis types:
        // 3=Desktop, 4=Low Profile Desktop, 5=Pizza Box, 6=Mini Tower
        // 7=Tower, 8=Portable, 9=Laptop, 10=Notebook, 11=Hand Held, 13=All In One
        // 14=Sub Notebook, 15=Space-saving, 16=Lunch Box, 17=Main Server Chassis
        match chassis_type {
            "3" | "4" | "5" | "6" | "7" | "15" | "16" | "17" => return true, // desktop form factors
            "9" | "10" | "14" => return false,                               // laptop form factors
            _ => {} // Unknown, continue with other checks
        }
    }

    // Check CPU power policies, desktops often don't have these
    let power_saving_exists = Path::new("/sys/module/intel_pstate/parameters/no_hwp").exists()
        || Path::new("/sys/devices/system/cpu/cpufreq/conservative").exists();

    if !power_saving_exists {
        return true; // likely a desktop
    }

    // Check battery-specific ACPI paths that laptops typically have
    let laptop_acpi_paths = [
        "/sys/class/power_supply/BAT0",
        "/sys/class/power_supply/BAT1",
        "/proc/acpi/battery",
    ];

    for path in &laptop_acpi_paths {
        if Path::new(path).exists() {
            return false; // Likely a laptop
        }
    }

    // Default to assuming desktop if we can't determine
    true
}

pub fn get_system_load() -> Result<SystemLoad> {
    let loadavg_str = read_sysfs_file_trimmed("/proc/loadavg")?;
    let parts: Vec<&str> = loadavg_str.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(SysMonitorError::ParseError(
            "Could not parse /proc/loadavg: expected at least 3 parts".to_string(),
        ));
    }
    let load_avg_1min = parts[0].parse().map_err(|_| {
        SysMonitorError::ParseError(format!("Failed to parse 1min load: {}", parts[0]))
    })?;
    let load_avg_5min = parts[1].parse().map_err(|_| {
        SysMonitorError::ParseError(format!("Failed to parse 5min load: {}", parts[1]))
    })?;
    let load_avg_15min = parts[2].parse().map_err(|_| {
        SysMonitorError::ParseError(format!("Failed to parse 15min load: {}", parts[2]))
    })?;

    Ok(SystemLoad {
        load_avg_1min,
        load_avg_5min,
        load_avg_15min,
    })
}

pub fn collect_system_report(config: &AppConfig) -> Result<SystemReport> {
    let system_info = get_system_info();
    let cpu_cores = get_all_cpu_core_info()?;
    let cpu_global = get_cpu_global_info(&cpu_cores);
    let batteries = get_battery_info(config)?;
    let system_load = get_system_load()?;

    Ok(SystemReport {
        system_info,
        cpu_cores,
        cpu_global,
        batteries,
        system_load,
        timestamp: SystemTime::now(),
    })
}

pub fn get_cpu_model() -> Result<String> {
    let path = Path::new("/proc/cpuinfo");
    let content = fs::read_to_string(path).map_err(|_| {
        SysMonitorError::ReadError(format!("Cannot read contents of {}.", path.display()))
    })?;

    for line in content.lines() {
        if line.starts_with("model name") {
            if let Some(val) = line.split(':').nth(1) {
                let cpu_model = val.trim().to_string();
                return Ok(cpu_model);
            }
        }
    }
    Err(SysMonitorError::ParseError(
        "Could not find CPU model name in /proc/cpuinfo.".to_string(),
    ))
}

pub fn get_linux_distribution() -> Result<String> {
    let os_release_path = Path::new("/etc/os-release");
    let content = fs::read_to_string(os_release_path).map_err(|_| {
        SysMonitorError::ReadError(format!(
            "Cannot read contents of {}.",
            os_release_path.display()
        ))
    })?;

    for line in content.lines() {
        if line.starts_with("PRETTY_NAME=") {
            if let Some(val) = line.split('=').nth(1) {
                let linux_distribution = val.trim_matches('"').to_string();
                return Ok(linux_distribution);
            }
        }
    }

    let lsb_release_path = Path::new("/etc/lsb-release");
    let content = fs::read_to_string(lsb_release_path).map_err(|_| {
        SysMonitorError::ReadError(format!(
            "Cannot read contents of {}.",
            lsb_release_path.display()
        ))
    })?;

    for line in content.lines() {
        if line.starts_with("DISTRIB_DESCRIPTION=") {
            if let Some(val) = line.split('=').nth(1) {
                let linux_distribution = val.trim_matches('"').to_string();
                return Ok(linux_distribution);
            }
        }
    }

    Err(SysMonitorError::ParseError(format!(
        "Could not find distribution name in {} or {}.",
        os_release_path.display(),
        lsb_release_path.display()
    )))
}
