//! Host CPU-temperature reader, behind a trait so the metrics sampler stays
//! platform-agnostic. Linux reads the CPU hwmon (k10temp/coretemp/…) from sysfs; every
//! other machine has no readable CPU sensor, so the fallback reports `None` and the
//! temperature graph is simply omitted ("if available"). Mirrors the sysfs-with-fallback
//! style of `summarize::rss_of`.

use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::path::Path;

/// Reads the host CPU temperature.
pub trait Thermometer: Send + Sync {
    /// CPU package temperature in millidegrees Celsius, or `None` if unavailable.
    fn read_mc(&self) -> Option<i64>;
}

/// Pick the reader for this host: the Linux hwmon reader if a CPU sensor is found, else
/// the no-op fallback (non-Linux, or Linux with no recognized sensor). Resolved once —
/// the hwmon index isn't stable across boots, but it is within one server run.
pub fn for_host() -> Box<dyn Thermometer> {
    #[cfg(target_os = "linux")]
    if let Some(t) = LinuxThermometer::detect() {
        return Box::new(t);
    }
    #[cfg(target_os = "macos")]
    return Box::new(MacThermometer);
    #[allow(unreachable_code)]
    Box::new(MachineThermometer)
}

/// Fallback for machines with no readable CPU sensor (macOS/dev, or Linux with no
/// recognized hwmon chip). Always `None`.
struct MachineThermometer;

impl Thermometer for MachineThermometer {
    fn read_mc(&self) -> Option<i64> {
        None
    }
}

/// macOS (Apple Silicon) CPU temperature via the `macmon` crate, which reads the SMC /
/// IOReport sensors without root. `macmon`'s `Sampler` holds IOKit/CoreFoundation handles
/// and is not `Send`, so we can't store it in this `Send + Sync` trait object; instead we
/// build a fresh `Sampler` inside each `read_mc` (created, used, and dropped on one stack
/// frame — it never crosses a thread boundary). Fieldless, so the struct is trivially
/// `Send + Sync`. This path only runs when the server is started on a Mac (dev); the
/// deployed Linux box uses `LinuxThermometer`.
#[cfg(target_os = "macos")]
struct MacThermometer;

/// How long `get_metrics` samples before returning. Temperature (SMC) is read within the
/// call; the window mainly governs the power/usage deltas we ignore, so keep it short.
#[cfg(target_os = "macos")]
const MAC_SAMPLE_MS: u32 = 100;

#[cfg(target_os = "macos")]
impl Thermometer for MacThermometer {
    fn read_mc(&self) -> Option<i64> {
        let mut sampler = macmon::Sampler::new().ok()?;
        let c = sampler.get_metrics(MAC_SAMPLE_MS).ok()?.temp.cpu_temp_avg;
        // 0.0 means no sensor value this sample; treat as unavailable.
        (c > 0.0).then(|| (c as f64 * 1000.0).round() as i64)
    }
}

/// Reads a resolved Linux hwmon `tempN_input` (millidegrees Celsius) each sample.
#[cfg(target_os = "linux")]
struct LinuxThermometer {
    input_path: PathBuf,
}

/// CPU-temperature hwmon chip `name`s, most-preferred first. `coretemp` = Intel,
/// `k10temp`/`zenpower` = AMD, then generic SoC/ACPI zones.
#[cfg(target_os = "linux")]
const CPU_CHIP_PRIORITY: &[&str] = &["coretemp", "k10temp", "zenpower", "cpu_thermal", "acpitz"];

/// `tempN_label`s that denote the package/whole-die temperature, most-preferred first.
#[cfg(target_os = "linux")]
const PACKAGE_LABELS: &[&str] = &["Tctl", "Tdie", "Package id 0", "Tccd1"];

#[cfg(target_os = "linux")]
impl LinuxThermometer {
    /// Scan `/sys/class/hwmon` for a CPU-temperature chip and resolve the specific
    /// `tempN_input` file to read. `None` if nothing recognizable is present.
    fn detect() -> Option<Self> {
        let chips = read_hwmon_chips(Path::new("/sys/class/hwmon"));
        let dir = pick_cpu_chip(&chips)?;
        let input_path = pick_temp_input(&dir)?;
        Some(Self { input_path })
    }
}

#[cfg(target_os = "linux")]
impl Thermometer for LinuxThermometer {
    fn read_mc(&self) -> Option<i64> {
        std::fs::read_to_string(&self.input_path)
            .ok()?
            .trim()
            .parse::<i64>()
            .ok()
    }
}

/// `(chip_name, hwmon_dir)` for every `/sys/class/hwmon/hwmon*` with a readable `name`.
#[cfg(target_os = "linux")]
fn read_hwmon_chips(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let dir = e.path();
        if let Ok(name) = std::fs::read_to_string(dir.join("name")) {
            out.push((name.trim().to_string(), dir));
        }
    }
    out
}

/// Choose the CPU-temperature chip's hwmon dir from `(name, dir)` pairs, by
/// `CPU_CHIP_PRIORITY`. Pure (list injected) so it's testable without sysfs.
#[cfg(target_os = "linux")]
fn pick_cpu_chip(chips: &[(String, PathBuf)]) -> Option<PathBuf> {
    CPU_CHIP_PRIORITY.iter().find_map(|want| {
        chips
            .iter()
            .find(|(name, _)| name == want)
            .map(|(_, dir)| dir.clone())
    })
}

/// Within a chip dir, pick the `tempN_input` for the package/die temperature: prefer one
/// whose `tempN_label` is in `PACKAGE_LABELS`, else fall back to `temp1_input`.
#[cfg(target_os = "linux")]
fn pick_temp_input(dir: &Path) -> Option<PathBuf> {
    for label in PACKAGE_LABELS {
        // temp1..temp9 covers real hardware; labels beyond that are vanishingly rare.
        for n in 1..=9 {
            let label_file = dir.join(format!("temp{n}_label"));
            if let Ok(l) = std::fs::read_to_string(&label_file) {
                if l.trim() == *label {
                    let input = dir.join(format!("temp{n}_input"));
                    if input.is_file() {
                        return Some(input);
                    }
                }
            }
        }
    }
    let first = dir.join("temp1_input");
    first.is_file().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_fallback_is_always_none() {
        assert_eq!(MachineThermometer.read_mc(), None);
    }

    #[test]
    fn for_host_never_panics() {
        // On CI/macOS this is the fallback; on the Linux box it's the real reader.
        let _ = for_host().read_mc();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pick_cpu_chip_prefers_cpu_over_gpu_and_by_priority() {
        // amdgpu + wifi + k10temp present (the home box's real set): pick k10temp.
        let chips = vec![
            ("amdgpu".into(), PathBuf::from("/hwmon1")),
            ("k10temp".into(), PathBuf::from("/hwmon2")),
            ("mt7921_phy0".into(), PathBuf::from("/hwmon4")),
        ];
        assert_eq!(pick_cpu_chip(&chips), Some(PathBuf::from("/hwmon2")));

        // coretemp outranks k10temp when both somehow appear.
        let chips = vec![
            ("k10temp".into(), PathBuf::from("/a")),
            ("coretemp".into(), PathBuf::from("/b")),
        ];
        assert_eq!(pick_cpu_chip(&chips), Some(PathBuf::from("/b")));

        // No CPU chip -> None (only a GPU sensor).
        let chips = vec![("amdgpu".into(), PathBuf::from("/hwmon1"))];
        assert_eq!(pick_cpu_chip(&chips), None);
    }
}
