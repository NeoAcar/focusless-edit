mod controller;
mod storage_worker;

use std::{env, fs};

use anyhow::{Context, Result};
use controller::Controller;
use directories::ProjectDirs;
use slint::ComponentHandle;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

slint::include_modules!();

fn main() -> Result<()> {
    // The XDG portal backend used by the native Linux file dialogs performs
    // D-Bus work asynchronously. Keep its executor alive for the full UI run.
    let portal_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the Linux portal runtime")?;
    let _portal_runtime_guard = portal_runtime.enter();

    let (project_dirs, _log_guard) = initialize_logging()?;
    info!("starting Focusless Edit");

    let ui = AppWindow::new().context("could not create the application window")?;
    let controller = Controller::install(&ui, &project_dirs)?;

    let startup_path = env::args_os().nth(1).map(Into::into).or_else(|| {
        let recovery = controller.borrow().recovery_path().to_path_buf();
        recovery.is_file().then_some(recovery)
    });
    if let Some(path) = startup_path {
        controller.borrow_mut().begin_open(&ui, path);
    }

    ui.run().context("application event loop failed")
}

fn initialize_logging() -> Result<(ProjectDirs, WorkerGuard)> {
    let project_dirs = ProjectDirs::from("dev", "Focusless", "Focusless Edit")
        .context("could not determine application data directories")?;
    let log_dir = project_dirs
        .state_dir()
        .unwrap_or(project_dirs.data_local_dir());
    fs::create_dir_all(log_dir)
        .with_context(|| format!("could not create log directory {}", log_dir.display()))?;

    let appender = tracing_appender::rolling::daily(log_dir, "focusless.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok((project_dirs, guard))
}
