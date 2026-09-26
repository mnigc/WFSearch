//! Windows service lifecycle: SCM dispatch, install/uninstall.

use std::ffi::OsString;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::{app, config, snapshot};

pub const SERVICE_NAME: &str = "WFSearch";
pub const SERVICE_DISPLAY: &str = "WFSearch File Search Engine";
pub const SERVICE_DESC: &str =
    "Fast NTFS filename search engine (MFT index + USN journal). Query via named pipe \\\\.\\pipe\\wfs-engine-v1 or http://127.0.0.1:15100.";

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

    let cfg = config::Config::load(None);
    let application = match app::boot(cfg) {
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

pub fn install() -> anyhow::Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let exe = std::env::current_exe()?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: SERVICE_DISPLAY.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec!["run".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem — required for MFT access
        account_password: None,
    };
    let svc = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG)?;
    svc.set_description(SERVICE_DESC)?;
    println!(
        "service '{SERVICE_NAME}' installed (autostart). Start it with: sc start {SERVICE_NAME}"
    );
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let svc = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    )?;
    if svc.query_status()?.current_state != ServiceState::Stopped {
        anyhow::bail!("service is running; stop it first: sc stop {SERVICE_NAME}");
    }
    svc.delete()?;
    println!("service '{SERVICE_NAME}' removed");
    Ok(())
}
