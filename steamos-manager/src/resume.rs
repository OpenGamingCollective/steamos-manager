/*
 * Copyright © 2026 Nilesh Chakraborty
 *
 * SPDX-License-Identifier: MIT
 */

use anyhow::Result;
use std::future::Future;
use tokio_stream::StreamExt;
use tracing::{debug, error, info};
use zbus::Connection;

use crate::Service;
use crate::gpu::{GpuPerformanceLevelDriverType, reset_amdgpu_dpm_on_resume};
use crate::hardware::device_config;
use crate::power::set_platform_profile;
use crate::systemd::LogindManagerProxy;

pub(crate) struct SleepResumeService {
    connection: Connection,
}

impl SleepResumeService {
    pub(crate) fn init(connection: Connection) -> Self {
        Self { connection }
    }

    async fn handle_resume(&self) -> Result<()> {
        info!("System resumed from sleep: restoring hardware power states");

        let config = device_config().await.unwrap_or(None);

        // 1. Ensure required platform profile is re-activated if configured
        if let Some(ref config) = config {
            if let Some(ref perf_config) = config.performance_profile {
                if let Err(e) = set_platform_profile(
                    &perf_config.platform_profile_name,
                    &perf_config.suggested_default,
                )
                .await
                {
                    debug!("Failed to restore platform profile on resume: {e}");
                }
            }
        }

        // 2. Clear uninitialized AMD GPU telemetry and 600MHz DPM clamp (skip on Intel / non-AMD)
        let is_amd_gpu = match config.as_ref().and_then(|c| c.gpu_performance.as_ref()) {
            Some(gpu_conf) => gpu_conf.driver == GpuPerformanceLevelDriverType::Amdgpu,
            None => true, // Fallback: probe sysfs if no explicit device configuration
        };

        if is_amd_gpu {
            if let Err(e) = reset_amdgpu_dpm_on_resume().await {
                debug!("AMD GPU DPM reset on resume: {e}");
            }
        }

        Ok(())
    }
}

impl Service for SleepResumeService {
    const NAME: &'static str = "SleepResumeService";

    fn run(&mut self) -> impl Future<Output = Result<()>> + Send {
        async move {
            let proxy = LogindManagerProxy::new(&self.connection).await?;
            let mut stream = proxy.receive_prepare_for_sleep().await?;
            info!("SleepResumeService listening for systemd-logind PrepareForSleep signals");

            while let Some(signal) = stream.next().await {
                match signal.args() {
                    Ok(args) if !args.start() => {
                        // start() == false indicates system resumed from sleep
                        if let Err(e) = self.handle_resume().await {
                            error!("Error handling post-resume hardware recovery: {e}");
                        }
                    }
                    Ok(_) => {
                        debug!("System preparing for sleep");
                    }
                    Err(e) => {
                        error!("Error parsing PrepareForSleep signal: {e}");
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "resume.test.rs"]
mod test;


