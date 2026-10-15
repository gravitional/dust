use crate::Result;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub fn install(scope: &std::path::Path) -> Result<()> {
    use std::ffi::OsString;
    use windows_service::{
        service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let exe = std::env::current_exe()?;
    let database = crate::default_database(scope)?;
    let info = ServiceInfo {
        name: OsString::from("dust-indexer"),
        display_name: OsString::from("Dust NTFS Indexer"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![
            OsString::from("--indexer-service"),
            OsString::from("--index-database"),
            OsString::from(database),
        ],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG)?
        .set_description("Maintains dust's NTFS index and USN Journal cursor")?;
    Ok(())
}

#[cfg(windows)]
pub fn run(database: Option<&str>) -> Result<()> {
    if let Some(database) = database {
        unsafe {
            std::env::set_var("DUST_INDEX_DATABASE", database);
        }
    }
    windows_service::service_dispatcher::start("dust-indexer", ffi_service_main)?;
    Ok(())
}

#[cfg(windows)]
windows_service::define_windows_service!(ffi_service_main, service_main);

#[cfg(windows)]
fn service_main(arguments: Vec<std::ffi::OsString>) {
    if let Err(error) = run_service(arguments) {
        eprintln!("dust-indexer service: {error}");
    }
}

#[cfg(windows)]
fn run_service(arguments: Vec<std::ffi::OsString>) -> Result<()> {
    use std::sync::mpsc;
    use windows_service::{
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
    };
    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |event| match event {
        ServiceControl::Stop => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register("dust-indexer", handler)?;
    status.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    })?;
    let stop = Arc::new(AtomicBool::new(false));
    let health = Arc::new(Mutex::new(None));
    let db = arguments
        .windows(2)
        .find(|pair| pair[0] == "--index-database")
        .map(|pair| PathBuf::from(&pair[1]))
        .or_else(|| std::env::var_os("DUST_INDEX_DATABASE").map(PathBuf::from))
        .map(PathBuf::from)
        .ok_or("index database path is required")?;
    if let Err(error) = crate::refresh(&db) {
        if let Ok(mut state) = health.lock() {
            *state = Some(error.to_string());
        }
    }
    let worker_stop = stop.clone();
    let worker_health = health.clone();
    let worker = std::thread::spawn(move || crate::ipc::serve(&db, worker_stop, worker_health));
    let refresh_stop = stop.clone();
    let refresh_health = health.clone();
    let refresh_db = arguments
        .windows(2)
        .find(|pair| pair[0] == "--index-database")
        .map(|pair| PathBuf::from(&pair[1]))
        .or_else(|| std::env::var_os("DUST_INDEX_DATABASE").map(PathBuf::from));
    let refresher = std::thread::spawn(move || {
        while !refresh_stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if refresh_stop.load(Ordering::Relaxed) {
                break;
            }
            let result = match refresh_db.as_deref() {
                Some(path) => crate::refresh(path),
                None => Err("index database path missing".into()),
            };
            if let Err(error) = result {
                if let Ok(mut state) = refresh_health.lock() {
                    *state = Some(error.to_string());
                }
            } else if let Ok(mut state) = refresh_health.lock() {
                *state = None;
            }
        }
    });
    let _ = stop_rx.recv();
    stop.store(true, Ordering::Relaxed);
    let _ = refresher.join();
    let _ = worker.join();
    status.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: std::time::Duration::default(),
        process_id: None,
    })?;
    Ok(())
}
