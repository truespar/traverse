//! Windows service support: install, uninstall and service-mode execution, so `traverse-server`
//! runs as a native Windows service.

use std::ffi::OsString;
use std::time::{Duration, Instant};

use clap::Parser;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const SERVICE_NAME: &str = "TraverseServer";
const SERVICE_DISPLAY_NAME: &str = "Traverse Graph Database";
const SERVICE_DESCRIPTION: &str = "Traverse graph database server - Bolt, HTTP API, and Studio";

/// Installs the service with the Windows Service Control Manager.
pub fn install() -> windows_service::Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;

    let exe_path = std::env::current_exe().expect("failed to get executable path");

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let service = manager.create_service(&service_info, ServiceAccess::CHANGE_CONFIG)?;
    service.set_description(SERVICE_DESCRIPTION)?;

    println!("Service '{SERVICE_NAME}' installed successfully.");
    println!();
    println!("  Configure via:  traverse.toml in the executable directory,");
    println!("                  or environment variables (TRAVERSE_DATA, etc.)");
    println!();
    println!("  Start:          sc start {SERVICE_NAME}");
    println!("                  net start {SERVICE_NAME}");
    println!();
    println!("  Stop:           sc stop {SERVICE_NAME}");
    println!("  Remove:         traverse-server --service-uninstall");

    Ok(())
}

/// Uninstalls the service.
pub fn uninstall() -> windows_service::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;

    let service_access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = manager.open_service(SERVICE_NAME, service_access)?;

    // stop it if running
    if let Ok(status) = service.query_status()
        && status.current_state != ServiceState::Stopped
    {
        println!("Stopping service...");
        service.stop()?;
    }

    service.delete()?;

    // wait for the deletion
    let start = Instant::now();
    let timeout = Duration::from_secs(10);
    while start.elapsed() < timeout {
        if let Err(windows_service::Error::Winapi(e)) =
            manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        {
            let raw = e.raw_os_error().unwrap_or(0);
            // ERROR_SERVICE_DOES_NOT_EXIST = 1060
            if raw == 1060 {
                println!("Service '{SERVICE_NAME}' removed successfully.");
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    println!(
        "Service '{SERVICE_NAME}' marked for deletion (will complete after reboot or when all handles are closed)."
    );
    Ok(())
}

// -- service dispatch ----------------------------------------------

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Entry point called by the SCM.
pub fn run_as_service() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

/// Service main, called by the SCM dispatcher.
fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service exited with error: {e}");
    }
}

fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    // tracing to a rolling log file (no console in service mode)
    let log_dir = std::env::var("TRAVERSE_LOG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("."))
        });
    std::fs::create_dir_all(&log_dir).ok();
    let file_appender = tracing_appender::rolling::daily(&log_dir, "traverse.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    tracing::info!("Traverse server starting as Windows service");

    // shutdown channel
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    // service control handler
    let status_handle =
        service_control_handler::register(SERVICE_NAME, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                shutdown_tx.send(()).ok();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;

    // report: starting
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(30),
        process_id: None,
    })?;

    // server in a tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let status_handle_clone = status_handle;
    let result = rt.block_on(async move {
        // same startup logic as interactive mode
        match run_server_inner(status_handle_clone, shutdown_rx).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::error!("server error: {e}");
                Err(e)
            }
        }
    });

    // report: stopped
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(if result.is_ok() { 0 } else { 1 }),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

/// The interactive server's `serve`, stopped by the SCM instead of Ctrl+C; reports Running once
/// it listens.
async fn run_server_inner(
    status_handle: windows_service::service_control_handler::ServiceStatusHandle,
    shutdown_rx: std::sync::mpsc::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // CLI sees --service and config flags from launch_arguments or the service's "ImagePath"
    // registry value
    let cli = crate::startup::Cli::parse();
    let shutdown = async move {
        tokio::task::spawn_blocking(move || {
            shutdown_rx.recv().ok();
        })
        .await
        .ok();
        tracing::info!("Service stop requested, shutting down gracefully");
    };
    crate::startup::serve(cli, shutdown, move || {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;
        tracing::info!("Traverse server running as Windows service");
        Ok(())
    })
    .await
}
