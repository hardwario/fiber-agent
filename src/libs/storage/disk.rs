//! Disk usage utilities for monitoring storage capacity

/// Disk partition usage statistics
#[derive(Debug, Clone, Copy)]
pub struct PartitionUsage {
    /// Total space in bytes
    pub total_bytes: u64,
    /// Available space in bytes
    pub available_bytes: u64,
    /// Used space percentage (0-100)
    pub used_percent: u8,
}

impl PartitionUsage {
    /// Create empty/unavailable partition usage
    pub fn unavailable() -> Self {
        Self {
            total_bytes: 0,
            available_bytes: 0,
            used_percent: 0,
        }
    }
}

/// Get disk usage for a given path using df command
pub fn get_partition_usage(path: &str) -> PartitionUsage {
    // Use df command to get filesystem statistics
    // df -B1 gives output in bytes: Filesystem 1B-blocks Used Available Use% Mounted
    if let Ok(output) = std::process::Command::new("df")
        .args(["-B1", path])
        .output()
    {
        if let Ok(output_str) = String::from_utf8(output.stdout) {
            // Skip header line, parse the data line
            for line in output_str.lines().skip(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                // Expected format: Filesystem 1B-blocks Used Available Use% Mounted
                if parts.len() >= 5 {
                    let total_bytes = parts[1].parse::<u64>().unwrap_or(0);
                    let available_bytes = parts[3].parse::<u64>().unwrap_or(0);
                    let used_percent = parts[4]
                        .trim_end_matches('%')
                        .parse::<u8>()
                        .unwrap_or(0);

                    return PartitionUsage {
                        total_bytes,
                        available_bytes,
                        used_percent,
                    };
                }
            }
        }
    }

    PartitionUsage::unavailable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_partition_usage_root() {
        // Test with root filesystem (should always exist)
        let usage = get_partition_usage("/");
        assert!(usage.total_bytes > 0);
        assert!(usage.used_percent <= 100);
    }

    #[test]
    fn test_get_partition_usage_invalid() {
        // Test with non-existent path - df will still work but may return different mount
        let usage = get_partition_usage("/nonexistent/path/xyz");
        // df falls back to root partition for non-existent paths
        // so this might still succeed
    }
}
