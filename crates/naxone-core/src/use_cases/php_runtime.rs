use std::collections::HashMap;

use crate::config::PhpRuntimeConfig;
use crate::domain::service::{PhpRuntimeOptions, ServiceInstance, ServiceKind};
use crate::domain::vhost::VirtualHost;

const MAX_TOTAL_WORKERS: u16 = 64;
const MAX_WORKERS_PER_VERSION: u16 = 16;
const MIN_MAX_REQUESTS: u32 = 100;
const MAX_MAX_REQUESTS: u32 = 10_000;
const PHPSTUDY_WORKERS_PER_VERSION: u16 = 16;

/// Apply the global worker budget to PHP versions referenced by enabled vhosts.
///
/// Every active version receives one worker first. Remaining slots use a
/// D'Hondt-style weighted distribution based on enabled-vhost counts, while
/// respecting the per-version cap. Installed but unused PHP versions keep
/// `php_runtime = None` and are not dependency-started with Nginx.
pub fn apply_php_runtime_policy(
    services: &mut [ServiceInstance],
    vhosts: &[VirtualHost],
    config: &PhpRuntimeConfig,
) {
    let mut site_counts: HashMap<u16, u32> = HashMap::new();
    for vhost in vhosts.iter().filter(|v| v.enabled) {
        if let Some(port) = vhost.php_fastcgi_port {
            *site_counts.entry(port).or_default() += 1;
        }
    }

    let active: Vec<(u16, u32)> = services
        .iter()
        .filter(|s| s.kind == ServiceKind::Php)
        .filter_map(|s| site_counts.get(&s.port).copied().map(|count| (s.port, count)))
        .collect();

    let allocation = allocate_worker_counts_for_config(&active, config);
    let max_requests = config.max_requests.clamp(MIN_MAX_REQUESTS, MAX_MAX_REQUESTS);

    for service in services.iter_mut().filter(|s| s.kind == ServiceKind::Php) {
        service.php_runtime = allocation.get(&service.port).map(|workers| PhpRuntimeOptions {
            workers: *workers,
            max_requests,
        });
    }
}

pub fn allocate_worker_counts_for_config(
    active: &[(u16, u32)],
    config: &PhpRuntimeConfig,
) -> HashMap<u16, u16> {
    if config.phpstudy_compatible_workers {
        return active
            .iter()
            .map(|(port, _)| (*port, PHPSTUDY_WORKERS_PER_VERSION))
            .collect();
    }
    allocate_worker_counts(
        active,
        config.total_worker_budget,
        config.max_workers_per_version,
    )
}

/// Return `port -> workers` for active `(port, enabled_vhost_count)` entries.
pub fn allocate_worker_counts(
    active: &[(u16, u32)],
    total_worker_budget: u16,
    max_workers_per_version: u16,
) -> HashMap<u16, u16> {
    if active.is_empty() {
        return HashMap::new();
    }

    let per_version_cap = max_workers_per_version.clamp(1, MAX_WORKERS_PER_VERSION);
    let minimum = active.len().min(MAX_TOTAL_WORKERS as usize) as u16;
    let capacity = minimum.saturating_mul(per_version_cap).min(MAX_TOTAL_WORKERS);
    // If active versions outnumber the configured budget, one worker per
    // version wins; otherwise the configured budget remains the hard ceiling.
    let target = total_worker_budget
        .clamp(1, MAX_TOTAL_WORKERS)
        .max(minimum)
        .min(capacity);

    let mut entries: Vec<(u16, u32, u16)> = active
        .iter()
        .take(MAX_TOTAL_WORKERS as usize)
        .map(|(port, count)| (*port, (*count).max(1), 1))
        .collect();
    entries.sort_by_key(|(port, _, _)| *port);

    let mut assigned = entries.len() as u16;
    while assigned < target {
        let mut best: Option<usize> = None;
        for (idx, (_, count, workers)) in entries.iter().enumerate() {
            if *workers >= per_version_cap {
                continue;
            }
            best = match best {
                None => Some(idx),
                Some(current) => {
                    let (_, best_count, best_workers) = entries[current];
                    // Compare count/(workers+1) without floating point.
                    let left = (*count as u64) * (best_workers as u64 + 1);
                    let right = (best_count as u64) * (*workers as u64 + 1);
                    if left > right {
                        Some(idx)
                    } else {
                        Some(current)
                    }
                }
            };
        }
        let Some(idx) = best else { break };
        entries[idx].2 += 1;
        assigned += 1;
    }

    entries
        .into_iter()
        .map(|(port, _, workers)| (port, workers))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_equal_versions_share_eight_workers() {
        let active = vec![(9000, 1), (9001, 1), (9002, 1), (9003, 1), (9004, 1)];
        let result = allocate_worker_counts(&active, 8, 4);
        assert_eq!(result.values().copied().sum::<u16>(), 8);
        assert_eq!(result[&9000], 2);
        assert_eq!(result[&9001], 2);
        assert_eq!(result[&9002], 2);
        assert_eq!(result[&9003], 1);
        assert_eq!(result[&9004], 1);
    }

    #[test]
    fn busy_version_gets_more_but_never_exceeds_cap() {
        let active = vec![(9000, 10), (9001, 1), (9002, 1)];
        let result = allocate_worker_counts(&active, 8, 4);
        assert_eq!(result.values().copied().sum::<u16>(), 8);
        assert_eq!(result[&9000], 4);
        assert!(result.values().all(|workers| *workers <= 4));
    }

    #[test]
    fn every_active_version_gets_one_when_budget_is_smaller() {
        let active = vec![(9000, 1), (9001, 1), (9002, 1)];
        let result = allocate_worker_counts(&active, 2, 4);
        assert_eq!(result.values().copied().sum::<u16>(), 3);
        assert!(result.values().all(|workers| *workers == 1));
    }

    #[test]
    fn phpstudy_mode_assigns_sixteen_to_every_active_version() {
        let active = vec![(9001, 1), (9002, 3), (9003, 1)];
        let config = PhpRuntimeConfig::default();
        let result = allocate_worker_counts_for_config(&active, &config);
        assert_eq!(result.values().copied().sum::<u16>(), 48);
        assert!(result.values().all(|workers| *workers == 16));
    }
}
