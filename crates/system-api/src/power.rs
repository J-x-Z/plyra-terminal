use crate::PowerSample;
use anyhow::{ensure, Result};

/// One fixed native sampler through the authenticated administrator Broker.
pub fn sample_power(interval_ms: u16) -> Result<PowerSample> {
    ensure!(
        (500..=2000).contains(&interval_ms),
        "power interval must be 500–2000 ms"
    );
    #[cfg(target_os = "macos")]
    {
        ensure!(
            crate::current_uid() == 0,
            "powermetrics requires administrator authorization"
        );
        let sample = crate::bounded_command(
            "/usr/bin/powermetrics",
            &[
                "--sample-count".into(),
                "1".into(),
                "--sample-rate".into(),
                interval_ms.to_string(),
                "--samplers".into(),
                "cpu_power,gpu_power,thermal".into(),
                "--format".into(),
                "text".into(),
            ],
            std::time::Duration::from_millis(u64::from(interval_ms) + 2000),
            65536,
        )?;
        ensure!(
            sample.success,
            "native power sampling failed: {}",
            sample.stderr
        );
        Ok(parse_sample(&sample.stdout, interval_ms, crate::now_ms()))
    }
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("native SoC power sampling is unavailable on this platform")
}

#[cfg(any(target_os = "macos", test))]
fn power_value(value: &str) -> Option<f64> {
    let value = value.trim();
    let (number, factor) = if let Some(number) = value.strip_suffix("mW") {
        (number, 1.0)
    } else if let Some(number) = value.strip_suffix('W') {
        (number, 1000.0)
    } else {
        return None;
    };
    let number = number.trim().parse::<f64>().ok()?;
    (number.is_finite() && number >= 0.0).then_some(number * factor)
}

#[cfg(any(target_os = "macos", test))]
fn parse_sample(output: &str, interval_ms: u16, sampled_at_ms: u64) -> PowerSample {
    let mut result = PowerSample {
        sampled_at_ms,
        interval_ms,
        cpu_milliwatts: None,
        gpu_milliwatts: None,
        ane_milliwatts: None,
        thermal_pressure: None,
        source: "macOS powermetrics cpu_power,gpu_power,thermal".into(),
        scope: "Estimated whole-SoC subsystem power during the sampling interval".into(),
        summary: String::new(),
    };
    for line in output.lines() {
        let line = line.trim();
        let mut retained = false;
        if let Some(value) = line.strip_prefix("CPU Power:") {
            result.cpu_milliwatts = power_value(value);
            retained = true;
        } else if let Some(value) = line.strip_prefix("GPU Power:") {
            result.gpu_milliwatts = power_value(value);
            retained = true;
        } else if let Some(value) = line.strip_prefix("ANE Power:") {
            result.ane_milliwatts = power_value(value);
            retained = true;
        } else if let Some(value) = line.strip_prefix("Current pressure level:") {
            result.thermal_pressure = Some(value.trim().to_owned());
            retained = true;
        }
        if retained && result.summary.len() + line.len() < 8192 {
            result
                .summary
                .extend(line.chars().filter(|c| !c.is_control()));
            result.summary.push('\n');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_parser_preserves_estimates_units_and_unavailable_values() {
        let sample = parse_sample("CPU Power: 1234 mW\nGPU Power: 0.25 W\nANE Power: 0 mW\nCurrent pressure level: Nominal\n", 1000, 42);
        assert_eq!(sample.cpu_milliwatts, Some(1234.0));
        assert_eq!(sample.gpu_milliwatts, Some(250.0));
        assert_eq!(sample.thermal_pressure.as_deref(), Some("Nominal"));
        assert_eq!(sample.sampled_at_ms, 42);
        assert!(sample.scope.contains("whole-SoC"));
        let unavailable = parse_sample("CPU Power: unavailable\nGPU Power: NaN mW\n", 500, 1);
        assert!(unavailable.cpu_milliwatts.is_none() && unavailable.gpu_milliwatts.is_none());
        assert!(sample_power(0).is_err());
        assert!(sample_power(2001).is_err());
    }
}

// FVPROV1 4151 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMzFmYmNmYmI4NDNhYjYyM2I0ZjFmMmZkNzQ1ZjkxZTNlODAwNDQ1MGI0ZGQxN2FjN2M5MDliMzBlNDY0NjU5NiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOWVjMjMxZjVhMzkyNzdlZWE1MjlmMDk0YjY5ODQ0ZWNlMGM2NTIxZGMwOWQ4MGI3NzkxMWRlZDM1ZmM4MTY2MjgxN2ZhYjU2ZTUxYmVkNzYxNzU4M2Y1ZjA3ZDk2YWI2YzM1YzlkMjdmOTQ1ZjIwZGU3MTNmNTA1OWZjN2U3MGYifV19XQ== FVPROV1-END
