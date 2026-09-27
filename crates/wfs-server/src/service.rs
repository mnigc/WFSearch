//! Windows service lifecycle: the SCM dispatch entry point.
//!
//! Registering or removing the service is deliberately not this binary's job.
//! The deployer (or the host application embedding the engine) owns the
//! `sc create` / `sc delete` calls — see docs/deploy.md.

use std::ffi::OsString;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::{app, config, snapshot};

pub const SERVICE_NAME: &str = "WFSearch";

define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_as_service() {
        tracing::error!("service error: {e}");
    }
}

/// Called from main for the `run` subcommand — hands control to the SCM.
pub fn dispatch() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn run_as_service() -> windows_service::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handler = service_control_handler::register(SERVICE_NAME, move |event| match event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;

    let mut status = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    };
    handler.set_service_status(status.clone())?;

    let application = match boot_waiting_for_the_port(&rx) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("boot failed: {e}");
            status.current_state = ServiceState::Stopped;
            status.exit_code = ServiceExitCode::Win32(1);
            let _ = handler.set_service_status(status);
            return Ok(());
        }
    };
    let _ = rx.recv(); // block until Stop/Shutdown

    let _ = snapshot::save(&application.state);
    application.shutdown();

    status.current_state = ServiceState::Stopped;
    let _ = handler.set_service_status(status);
    Ok(())
}

/// How long the service keeps trying to come up before it gives up and reports
/// itself stopped: 20 * 3s outlasts an SVCode session that is about to close.
const BOOT_ATTEMPTS: u32 = 20;
const BOOT_RETRY_GAP: Duration = Duration::from_secs(3);

/// The gateway port belongs to whoever binds it first, and straight after a
/// (re)start that is usually the plain-user sidecar an older SVCode left
/// running — one that can bind 15100 but cannot read a single MFT. Exiting on
/// `AddrInUse` handed the impostor the port for good and left the service
/// stopped with nothing to restart it, so the boot is retried until the port
/// frees.
///
/// `Running` is reported before the first attempt: the SCM times a
/// `StartPending` service out after 30 s, and an engine that comes up a few
/// seconds late beats a start the services console shows as failed.
fn boot_waiting_for_the_port(rx: &Receiver<()>) -> anyhow::Result<app::App> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=BOOT_ATTEMPTS {
        match app::boot(config::Config::load(None)) {
            Ok(application) => return Ok(application),
            Err(e) => {
                tracing::warn!("boot failed (attempt {attempt}/{BOOT_ATTEMPTS}): {e}");
                last = Some(e);
            }
        }
        if stop_requested(rx) {
            break;
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("boot failed")))
}

/// Waits out one retry gap, returning early when the SCM asked us to stop so
/// `net stop` is not held up for the rest of the window.
fn stop_requested(rx: &Receiver<()>) -> bool {
    const TICK: Duration = Duration::from_millis(200);
    let mut waited = Duration::ZERO;
    while waited < BOOT_RETRY_GAP {
        if rx.try_recv().is_ok() {
            return true;
        }
        std::thread::sleep(TICK);
        waited += TICK;
    }
    false
}
