use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::model::{CgroupState, CpuStat, IoTotals, PsiLine, PsiSnapshot};

pub fn parse_single_u64(input: &str, label: &str) -> Result<u64> {
    input
        .trim()
        .parse::<u64>()
        .with_context(|| format!("parsing {label} as an unsigned integer"))
}

pub fn parse_keyed_u64(input: &str, label: &str) -> Result<BTreeMap<String, u64>> {
    let mut values = BTreeMap::new();
    for (line_no, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let key = parts
            .next()
            .ok_or_else(|| anyhow!("{label}: missing key on line {}", line_no + 1))?;
        let raw = parts
            .next()
            .ok_or_else(|| anyhow!("{label}: missing value for {key:?}"))?;
        if parts.next().is_some() {
            return Err(anyhow!(
                "{label}: unexpected extra fields on line {}",
                line_no + 1
            ));
        }
        let value = raw
            .parse::<u64>()
            .with_context(|| format!("{label}: parsing value for {key:?}"))?;
        values.insert(key.to_string(), value);
    }
    Ok(values)
}

pub fn parse_cpu_stat(input: &str) -> Result<CpuStat> {
    let mut values = parse_keyed_u64(input, "cpu.stat")?;
    let usage_usec = values
        .remove("usage_usec")
        .ok_or_else(|| anyhow!("cpu.stat is missing usage_usec"))?;
    Ok(CpuStat {
        usage_usec,
        user_usec: values.remove("user_usec"),
        system_usec: values.remove("system_usec"),
        extra: values,
    })
}

pub fn parse_io_stat(input: &str) -> Result<IoTotals> {
    let mut totals = IoTotals::default();
    for (line_no, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let device = parts
            .next()
            .ok_or_else(|| anyhow!("io.stat: missing device on line {}", line_no + 1))?;
        if !device.contains(':') {
            return Err(anyhow!("io.stat: invalid device {device:?}"));
        }
        for field in parts {
            let (key, raw) = field
                .split_once('=')
                .ok_or_else(|| anyhow!("io.stat: invalid field {field:?}"))?;
            let value = raw
                .parse::<u64>()
                .with_context(|| format!("io.stat: parsing {key:?}"))?;
            match key {
                "rbytes" => totals.read_bytes = totals.read_bytes.saturating_add(value),
                "wbytes" => totals.write_bytes = totals.write_bytes.saturating_add(value),
                "rios" => totals.read_ios = totals.read_ios.saturating_add(value),
                "wios" => totals.write_ios = totals.write_ios.saturating_add(value),
                "dbytes" => totals.discard_bytes = totals.discard_bytes.saturating_add(value),
                "dios" => totals.discard_ios = totals.discard_ios.saturating_add(value),
                _ => {}
            }
        }
    }
    Ok(totals)
}

pub fn parse_psi(input: &str) -> Result<PsiSnapshot> {
    let mut snapshot = PsiSnapshot::default();
    for (line_no, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let kind = parts
            .next()
            .ok_or_else(|| anyhow!("PSI: missing line type on line {}", line_no + 1))?;
        let mut values = BTreeMap::new();
        for field in parts {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| anyhow!("PSI: invalid field {field:?}"))?;
            values.insert(key, value);
        }
        let parsed = PsiLine {
            avg10: parse_psi_float(&values, "avg10")?,
            avg60: parse_psi_float(&values, "avg60")?,
            avg300: parse_psi_float(&values, "avg300")?,
            total_usec: values
                .get("total")
                .ok_or_else(|| anyhow!("PSI {kind}: missing total"))?
                .parse::<u64>()
                .with_context(|| format!("PSI {kind}: parsing total"))?,
        };
        match kind {
            "some" => snapshot.some = Some(parsed),
            "full" => snapshot.full = Some(parsed),
            _ => return Err(anyhow!("PSI: unknown line type {kind:?}")),
        }
    }
    if snapshot.some.is_none() && snapshot.full.is_none() {
        return Err(anyhow!("PSI file contained no some/full lines"));
    }
    Ok(snapshot)
}

fn parse_psi_float(values: &BTreeMap<&str, &str>, key: &str) -> Result<f64> {
    values
        .get(key)
        .ok_or_else(|| anyhow!("PSI: missing {key}"))?
        .parse::<f64>()
        .with_context(|| format!("PSI: parsing {key}"))
}

pub fn parse_cgroup_state(input: &str) -> Result<CgroupState> {
    let mut values = parse_keyed_u64(input, "cgroup.events")?;
    Ok(CgroupState {
        populated: values.remove("populated").map(|value| value != 0),
        frozen: values.remove("frozen").map(|value| value != 0),
        extra: values,
    })
}

pub fn counter_rate(previous: u64, current: u64, elapsed: Duration) -> Option<f64> {
    if current < previous || elapsed.is_zero() {
        return None;
    }
    Some((current - previous) as f64 / elapsed.as_secs_f64())
}

pub fn cpu_percent(previous_usec: u64, current_usec: u64, elapsed: Duration) -> Option<f64> {
    counter_rate(previous_usec, current_usec, elapsed).map(|usec_per_sec| usec_per_sec / 10_000.0)
}

/// Turn a systemd ControlGroup property into a path below the cgroup-v2 mount.
/// This is a lexical validation step; callers must canonicalize the live path
/// and re-check it remains below the canonical cgroup root before reading or
/// acting on it.
pub fn cgroup_fs_path(
    cgroup_root: &Path,
    user_manager_control_group: &str,
    unit_control_group: &str,
) -> Result<PathBuf> {
    validate_absolute_cgroup(user_manager_control_group)?;
    validate_absolute_cgroup(unit_control_group)?;

    let manager = user_manager_control_group.trim_end_matches('/');
    let expected_prefix = format!("{manager}/");
    if !unit_control_group.starts_with(&expected_prefix) {
        return Err(anyhow!(
            "unit control group {unit_control_group:?} is not below user manager {user_manager_control_group:?}"
        ));
    }

    Ok(cgroup_root.join(unit_control_group.trim_start_matches('/')))
}

fn validate_absolute_cgroup(value: &str) -> Result<()> {
    if !value.starts_with('/') {
        return Err(anyhow!("control group path must be absolute: {value:?}"));
    }
    if Path::new(value)
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(anyhow!("invalid control group path {value:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_stat_and_preserves_unknown_fields() {
        let parsed =
            parse_cpu_stat("usage_usec 123\nuser_usec 80\nsystem_usec 43\nnr_throttled 2\n")
                .unwrap();
        assert_eq!(parsed.usage_usec, 123);
        assert_eq!(parsed.user_usec, Some(80));
        assert_eq!(parsed.extra["nr_throttled"], 2);
    }

    #[test]
    fn aggregates_io_across_devices_and_ignores_unknown_keys() {
        let parsed = parse_io_stat(
            "8:0 rbytes=10 wbytes=20 rios=1 wios=2 cost.usage=9\n8:16 rbytes=30 wbytes=40 rios=3 wios=4\n",
        )
        .unwrap();
        assert_eq!(parsed.read_bytes, 40);
        assert_eq!(parsed.write_bytes, 60);
        assert_eq!(parsed.read_ios, 4);
        assert_eq!(parsed.write_ios, 6);
    }

    #[test]
    fn parses_psi_some_and_full() {
        let parsed = parse_psi(
            "some avg10=1.25 avg60=2.50 avg300=3.75 total=100\nfull avg10=0.10 avg60=0.20 avg300=0.30 total=10\n",
        )
        .unwrap();
        assert_eq!(parsed.some.unwrap().avg60, 2.5);
        assert_eq!(parsed.full.unwrap().total_usec, 10);
    }

    #[test]
    fn parses_cgroup_state_without_rejecting_new_keys() {
        let parsed = parse_cgroup_state("populated 1\nfrozen 0\nfuture 9\n").unwrap();
        assert_eq!(parsed.populated, Some(true));
        assert_eq!(parsed.frozen, Some(false));
        assert_eq!(parsed.extra["future"], 9);
    }

    #[test]
    fn rates_handle_counter_reset_and_zero_elapsed_time() {
        assert_eq!(counter_rate(100, 200, Duration::from_secs(2)), Some(50.0));
        assert_eq!(counter_rate(200, 100, Duration::from_secs(2)), None);
        assert_eq!(counter_rate(100, 200, Duration::ZERO), None);
        assert_eq!(
            cpu_percent(0, 2_000_000, Duration::from_secs(1)),
            Some(200.0)
        );
    }

    #[test]
    fn cgroup_path_must_remain_below_user_manager() {
        let root = Path::new("/sys/fs/cgroup");
        let got = cgroup_fs_path(
            root,
            "/user.slice/user-1000.slice/user@1000.service",
            "/user.slice/user-1000.slice/user@1000.service/app.slice/example.service",
        )
        .unwrap();
        assert_eq!(
            got,
            PathBuf::from(
                "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/example.service"
            )
        );
        assert!(cgroup_fs_path(root, "/user.slice/a", "/system.slice/x").is_err());
        assert!(cgroup_fs_path(root, "/user.slice/a", "/user.slice/a/../x").is_err());
    }
}
