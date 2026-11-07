// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Context;
use base::kill_process_group;
use base::reap_child;
use base::syslog;
use base::syslog::LogArgs;
use base::syslog::LogConfig;
use base::warn;
use device_virtio_console::vhost_user::run_console_device;
use device_virtio_fs::vhost_user::run_fs_device;
use devices::virtio::vhost_user_backend::run_vsock_device;
#[cfg(feature = "virtio_wl")]
use devices::virtio::vhost_user_backend::run_wl_device;

use crate::crosvm::sys::cmdline::Commands;
use crate::crosvm::sys::cmdline::DeviceSubcommand;
use crate::crosvm::sys::linux::start_devices;
use crate::CommandStatus;
use crate::Config;

pub(crate) fn start_device(command: DeviceSubcommand) -> anyhow::Result<()> {
    match command {
        DeviceSubcommand::Console(cfg) => run_console_device(cfg),
        DeviceSubcommand::Fs(cfg) => run_fs_device(cfg),
        DeviceSubcommand::Vsock(cfg) => run_vsock_device(cfg),
        #[cfg(feature = "virtio_wl")]
        DeviceSubcommand::Wl(cfg) => run_wl_device(cfg),
    }
}

// Take one best-effort snapshot without requiring CONFIG_PROC_CHILDREN.
fn log_remaining_children() {
    warn!("device children still running after 1s; allowing up to 10s for shutdown");
    let processes = match std::fs::read_dir("/proc") {
        Ok(processes) => processes,
        Err(e) => {
            warn!("cannot enumerate remaining children: {}", e);
            return;
        }
    };
    let parent_pid = std::process::id();
    for process in processes.flatten() {
        let Ok(pid) = process.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(process.path().join("stat")) else {
            continue; // A process may have exited during enumeration.
        };
        // comm may contain spaces or ')'; the fields after its last ')' start with state, ppid.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(1)?.parse::<u32>().ok());
        if ppid != Some(parent_pid) {
            continue;
        }
        let read_field = |name| {
            std::fs::read_to_string(process.path().join(name))
                .unwrap_or_else(|e| format!("<unavailable: {e}>"))
        };
        let comm = read_field("comm");
        let wchan = read_field("wchan");
        warn!(
            "child still running: pid={} comm={:?} wchan={:?}",
            pid,
            comm.trim_end(),
            wchan.trim_end()
        );
    }
}

// Wait for all children to exit. Return true if they have all exited, false
// otherwise.
fn wait_all_children() -> bool {
    // Allow slow process teardown without letting a stuck child hold up exit indefinitely.
    const CHILD_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
    const CHILD_WAIT_DIAGNOSTIC: Duration = Duration::from_secs(1);
    const CHILD_WAIT_INTERVAL: Duration = Duration::from_millis(10);
    let start = Instant::now();
    let mut logged_children = false;
    loop {
        loop {
            match reap_child() {
                Ok(0) => break,
                // We expect ECHILD which indicates that there were no children left.
                Err(e) if e.errno() == libc::ECHILD => return true,
                Err(e) => {
                    warn!("error while waiting for children: {}", e);
                    return false;
                }
                // We reaped one child, so continue reaping.
                _ => {}
            }
        }
        if !logged_children && start.elapsed() >= CHILD_WAIT_DIAGNOSTIC {
            log_remaining_children();
            logged_children = true;
        }
        let remaining = CHILD_WAIT_TIMEOUT.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return false;
        }
        // There's no timeout option for waitpid which reap_child calls internally, so our only
        // recourse is to sleep while waiting for the children to exit.
        sleep(CHILD_WAIT_INTERVAL.min(remaining));
    }
}

pub(crate) fn cleanup() {
    // Reap exit status from any child device processes. At this point, all devices should have been
    // dropped in the main process and told to shutdown. Allow up to 10 seconds for process
    // teardown, with a diagnostic snapshot after 1 second. Fast exits do not incur a delay.
    if !wait_all_children() {
        // We gave them a chance, and it's too late.
        warn!("not all child processes have exited; sending SIGKILL");
        if let Err(e) = kill_process_group() {
            // We're now at the mercy of the OS to clean up after us.
            warn!("unable to kill all child processes: {}", e);
        }
    }
}

pub(crate) fn run_command(command: Commands, _log_args: LogArgs) -> anyhow::Result<()> {
    match command {
        Commands::Devices(cmd) => start_devices(cmd).context("start_devices subcommand failed"),
    }
}

pub(crate) fn init_log(log_config: LogConfig, _cfg: &Config) -> anyhow::Result<()> {
    if let Err(e) = syslog::init_with(log_config) {
        eprintln!("failed to initialize syslog: {e}");
        return Err(anyhow!("failed to initialize syslog: {}", e));
    }
    Ok(())
}

pub(crate) fn error_to_exit_code(_res: &std::result::Result<CommandStatus, anyhow::Error>) -> i32 {
    1
}
