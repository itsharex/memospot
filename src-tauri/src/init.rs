use crate::runtime_config::RuntimeConfig;
use crate::sqlite;
/// Runtime checks and initialization code.
///
/// Functions in this module panics with native dialogs instead of returning errors.
/// Main purpose is to unclutter `main.rs`.
use crate::webview;
use crate::zip;
use config::Config;
use homedir::HomeDirExt;
use log::{debug, info, warn};
use memospot::*;
use migration::{Migrator, MigratorTrait};
use native_dialog::MessageType;
use std::env;
use std::env::consts::OS;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use tokio::time::Instant;
use writable::PathExt;

/// Ensure that data directory exists and is writable.
pub fn data_path(app_name: &str) -> PathBuf {
    let data_path = get_app_data_path(app_name);
    if !data_path.exists() {
        if let Err(e) = std::fs::create_dir_all(&data_path) {
            panic_dialog!(
                "Failed to create data directory `{}`:\n{}",
                data_path.to_string_lossy(),
                e.to_string()
            );
        }
    }

    if !&data_path.is_writable() {
        panic_dialog!(
            "Data directory is not writable:\n{}",
            data_path.to_string_lossy()
        );
    }
    data_path
}

/// Ensure that Memos data directory exists and is writable.
///
/// Use Memospot data directory if user-provided path is empty or ".".
/// Optionally, resolve a user-provided data directory.
pub fn memos_data(rtcfg: &RuntimeConfig) -> PathBuf {
    let data_str = rtcfg
        .yaml
        .memos
        .data
        .as_ref()
        .map(|s| s.as_str().trim())
        .unwrap_or("");

    // Use Memospot data directory if user-provided path is empty or ".".
    // Prevents resolving data path to a non-writable directory,
    // like /usr/local/bin or "Program Files".
    if data_str.is_empty() || data_str == "." {
        return rtcfg.paths.memospot_data.to_path_buf();
    }

    let expanded_path = PathBuf::from(data_str).expand_home().unwrap_or_default();
    let path = absolute_path(expanded_path)
        .unwrap_or_else(|_| rtcfg.paths.memospot_data.to_path_buf());
    if path.exists() && path.is_dir() {
        return path;
    }

    panic_dialog!(
        "Failed to resolve custom Memos data directory!\n{}\n\nEnsure it exists and is a directory, or remove the setting `memos.data` to use the default data path.",
        path.to_string_lossy()
    );
}

/// Ensure that backup directory exists and is writable.
///
/// Use Memospot data directory if user-provided path is empty or ".".
/// Optionally, resolve a user-provided directory.
pub fn backup_directory(rtcfg: &RuntimeConfig) -> PathBuf {
    let folder_name = "backups";
    let default_path = rtcfg.paths.memospot_data.join(folder_name);

    let cfg_path = rtcfg
        .yaml
        .memospot
        .backups
        .path
        .as_ref()
        .map(|s| s.as_str().trim())
        .unwrap_or("");

    // Use default directory if user-provided path is empty or ".".
    // Prevents resolving data path to a non-writable directory,
    // like /usr/local/bin or "Program Files".
    let path: PathBuf = if cfg_path.is_empty() || cfg_path == "." || cfg_path == folder_name {
        default_path
    } else {
        let expanded_path = PathBuf::from(cfg_path).expand_home().unwrap_or_default();
        absolute_path(expanded_path).unwrap_or(default_path)
    };

    if !path.exists() {
        if let Err(e) = std::fs::create_dir_all(&path) {
            panic_dialog!(
                "Failed to create backup directory `{}`:\n{}",
                path.to_string_lossy(),
                e.to_string()
            );
        }
    }

    if path.is_file() {
        panic_dialog!("Backup directory is a file:\n{}", path.to_string_lossy());
    }

    if !&path.is_writable() {
        panic_dialog!(
            "Backup directory is not writable:\n{}",
            path.to_string_lossy()
        );
    }

    path
}

/// Ensure that database files are writable, if they exist.
pub fn database(rtcfg: &RuntimeConfig) -> PathBuf {
    let db_file = &format!(
        "memos_{}.db",
        rtcfg.yaml.memos.mode.as_deref().unwrap_or_default()
    );
    let db_path = rtcfg.paths.memos_data.join(db_file);
    let files = vec![
        db_path.with_extension("db"),
        db_path.with_extension("db-wal"),
        db_path.with_extension("db-shm"),
    ];
    for file in files {
        // Remove demo database in dev/debug mode. Demo database is not handled by
        // migrations and can prevent Memos from starting if the model is outdated.
        if cfg!(debug_assertions) && rtcfg.yaml.memos.mode.as_deref() == Some("demo") {
            if file.exists() {
                if let Err(e) = std::fs::remove_file(&file) {
                    warn_dialog!("Failed to remove demo database:\n{}", e);
                }
            }
            continue;
        }
        if file.exists() && !&file.is_writable() {
            panic_dialog!("Database file is not writable:\n{}", file.to_string_lossy());
        }
    }
    db_path
}

/// Run database migrations.
pub async fn migrate_database(rtcfg: &RuntimeConfig) {
    if !rtcfg.yaml.memospot.migrations.enabled.unwrap_or_default() {
        warn!("Database migrations were disabled via configuration.");
        return;
    }
    if !rtcfg.paths.memos_db_file.exists() {
        return;
    }

    let db = sqlite::get_database_connection(rtcfg)
        .await
        .unwrap_or_else(|e| {
            panic_dialog!("Failed to connect to the database:\n{}", e.to_string());
        });
    let migration_amount = Migrator::get_pending_migrations(&db)
        .await
        .unwrap_or_default()
        .len();
    let _ = db.close().await;
    if migration_amount == 0 {
        debug!("No pending migrations found.");
        return;
    }

    if rtcfg.yaml.memospot.backups.enabled.unwrap_or_default() {
        let datetime = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let backup_name = format!("db-{}-pre-migration.zst.zip", datetime);
        let backup_path = rtcfg.paths._memospot_backups.join(&backup_name);
        let start_time = Instant::now();
        let backup = zip::related_files(
            &rtcfg.paths.memos_db_file,
            &["db-wal", "db-shm"],
            &backup_path,
        );
        match backup.await {
            Ok(_) => {
                info!(
                    "Database backup completed successfully! Operation took {:?}. Backup file: {}",
                    start_time.elapsed(),
                    backup_path.to_string_lossy()
                );
            }
            Err(e) => {
                warn_dialog!("Failed to backup Memos database:\n{}", e);
            }
        }
    }

    let start_time = Instant::now();
    let db = sqlite::get_database_connection(rtcfg)
        .await
        .unwrap_or_else(|e| {
            panic_dialog!("Failed to connect to the database:\n{}", e.to_string());
        });
    if let Err(e) = Migrator::up(&db, None).await {
        warn_dialog!("Failed to run database migrations:\n{}", e.to_string());
    }
    db.close().await.unwrap_or_else(|e| {
        panic_dialog!("Failed to close database connection:\n{}", e.to_string());
    });

    info!(
        "Database migrations took {:?}. Ran {} migrations.",
        start_time.elapsed(),
        migration_amount,
    );
}

/// Ensure that WebView is available.
pub fn ensure_webview() {
    if webview::is_available() {
        return;
    }

    let user_confirmed = confirm_dialog(
            "WebView Error",
            "WebView is *required* for this application to work and it's not available on this system!\
            \n\nDo you want to install it?",
            MessageType::Error,
        );
    if !user_confirmed {
        warn!("User declined to setup WebView.");
        exit(1);
    }

    tauri::async_runtime::block_on(async move {
        if let Err(e) = webview::install().await {
            error_dialog!(
                "Failed to install WebView:\n{}\n\nPlease install it manually.",
                e
            );

            if let Err(e) = webview::open_install_website() {
                warn!("Failed to launch WebView download website:\n{}", e);
            }
            exit(1)
        }
    });

    if !webview::is_available() {
        panic_dialog!(
            "Unable to setup WebView!\n\n\
                Please install it manually and relaunch the application."
        );
    }
}

/// Initialize application configuration.
///
/// - Ensure that configuration file exists and is writable.
/// - If configuration file is missing or malformed, optionally reset it to defaults.
pub fn config(config_path: &PathBuf) -> Config {
    if !config_path.exists() {
        if let Err(e) = Config::reset_file(config_path) {
            panic_dialog!(
                "Failed to create configuration file:\n{}\n\n{}",
                config_path.to_string_lossy(),
                e.to_string()
            );
        }
    }

    if config_path.is_dir() {
        panic_dialog!(
            "Provided configuration path is a directory! It must be a file.\n{}",
            config_path.to_string_lossy()
        );
    }

    if !config_path.is_writable() {
        panic_dialog!(
            "Configuration file is not writable:\n{}",
            config_path.to_string_lossy()
        );
    }

    let mut cfg_reader = Config::init(config_path);
    if let Err(e) = cfg_reader {
        let user_confirmed = confirm_dialog(
            "Configuration Error",
            &format!(
                "Failed to parse configuration file:\n\n{}\n\n\
                Do you want to reset the configuration file and start the application with default settings?",
                e
            ),
            MessageType::Warning
        );

        if !user_confirmed {
            panic_dialog!("You must fix the config file manually and restart the application.");
        }

        if let Err(e) = Config::reset_file(config_path) {
            panic_dialog!(
                "Failed to reset configuration file `{}`:\n{}",
                config_path.to_string_lossy(),
                e.to_string()
            );
        }
        cfg_reader = Ok(Config::default());
    }

    let mut config = cfg_reader.unwrap_or_else(|e| {
        panic_dialog!("Failed to parse configuration file:\n{}", e.to_string());
    });

    if cfg!(debug_assertions) {
        // Use Memos in demo mode during development,
        // as it's already seeded with some data.
        config.memos.mode = Some("demo".to_string());

        let current_port = config.memos.port.unwrap_or_default();
        // Use an upper port to use a dedicated WebView cache for development.
        if current_port != 0 {
            config.memos.port = Some(current_port + 1);
        }
    }
    config
}

/// Ensure that Memos port is available.
///
/// Tries to find a free port if the configured one is already
/// in use and updates the referenced configuration in place.
pub fn memos_port(rtcfg: &RuntimeConfig) -> u16 {
    if let Some(free_port) =
        portpicker::find_free_port(rtcfg.yaml.memos.port.unwrap_or_default())
    {
        return free_port;
    }

    panic_dialog!("Failed to find an open port!");
}

/// Memos URL.
///
/// If remote server is enabled, return the configured URL.
/// Otherwise, return the default Memos address for the spawned server.
pub fn memos_url(rtcfg: &RuntimeConfig) -> String {
    if !rtcfg.yaml.memospot.remote.enabled.unwrap_or_default()
        || rtcfg.yaml.memospot.remote.url.is_none()
    {
        return format!(
            "http://localhost:{}/",
            rtcfg.yaml.memos.port.unwrap_or_default()
        );
    }

    let url = rtcfg.yaml.memospot.remote.url.as_deref().unwrap();
    if url.is_empty() || !url.starts_with("http") {
        panic_dialog!(
            "Invalid remote server URL: `{}`\n\nURL must start with http:// or https://.\nCheck memospot.yaml.",
            url
        );
    }

    url.trim_end_matches('/').to_string() + "/"
}

/// Locate Memos server binary.
///
/// Look for Memos server binary in the following order:
/// 1. Provided Memos binary path from the configuration file.
/// 2. Memospot current working directory.
/// 3. Memospot data directory.
/// 4. ProgramData/memos (Windows only).
/// 5. /usr/local/bin, /var/opt/memos, /usr/local/memos (POSIX only).
pub fn find_memos(rtcfg: &RuntimeConfig) -> PathBuf {
    // Prefer path from the configuration file if it's valid.
    if let Some(binary_path) = &rtcfg.yaml.memos.binary_path {
        let yaml_bin = binary_path.as_str().trim();
        if !yaml_bin.is_empty() {
            let expanded_path = Path::new(yaml_bin).expand_home().unwrap_or_default();
            let path = absolute_path(expanded_path).unwrap_or_default();
            if path.exists() && path.is_file() {
                return path;
            }
        }
    }

    let binary_name = match OS {
        "windows" => "memos.exe",
        _ => "memos",
    };

    let mut search_paths: Vec<PathBuf> = Vec::from([
        rtcfg.paths.memospot_cwd.clone(),
        rtcfg.paths.memospot_data.clone(),
    ]);

    // Windows fall back.
    if OS == "windows" {
        if let Ok(program_data) = env::var("PROGRAMDATA") {
            search_paths.push(PathBuf::from(program_data).join("memos"));
        }
    } else {
        // POSIX fall back.
        search_paths.push(PathBuf::from("/usr/local/bin"));
        search_paths.push(PathBuf::from("/var/opt/memos"));
        search_paths.push(PathBuf::from("/usr/local/memos"));
    }

    debug!("Looking for Memos server at: {:?}", search_paths);
    for path in search_paths {
        let memos_path = path.join(binary_name);
        if memos_path.exists() && memos_path.is_file() {
            info!("Memos server found at: {}", memos_path.to_string_lossy());
            return memos_path;
        }
    }

    panic_dialog!("Unable to find Memos server!");
}

static LOGGING_CONFIG_YAML: &str = r#"
# Log4rs configuration file.
# https://github.com/estk/log4rs#quick-start
#
# Use absolute paths for file appender. Otherwise, it'll try to write next to the application binary.
# Data directory is available as: $ENV{MEMOSPOT_DATA}
appenders:
    file:
        encoder:
            pattern: "{d(%Y-%m-%d %H:%M:%S)} - {h({l})}: {m}{n}"
        path: $ENV{MEMOSPOT_DATA}/memospot.log
        kind: rolling_file
        policy:
            trigger:
                kind: size
                limit: 10 mb
            roller:
                kind: fixed_window
                pattern: $ENV{MEMOSPOT_DATA}/memospot.log.{}.gz
                count: 5
                base: 1
root:
    # debug | info | warn | error | off
    level: info
    appenders:
        - file
"#;

/// Setup logging if it's enabled.
///
/// - Validates `logging_config.yaml`.
///
/// Return true if logging is enabled.
pub fn setup_logger(rtcfg: &RuntimeConfig) -> bool {
    if !rtcfg.yaml.memospot.log.enabled.unwrap_or_default() {
        return false;
    }
    let log_config: PathBuf = rtcfg.paths.memospot_data.join("logging_config.yaml");

    // SAFETY: We're setting an environment variable, which is generally safe.
    // The unsafe block is required due to the potential for race conditions in
    // a multithreaded context.
    unsafe {
        // Allows using $ENV{MEMOSPOT_DATA} in log4rs config.
        std::env::set_var(
            "MEMOSPOT_DATA",
            rtcfg.paths.memospot_data.to_string_lossy().to_string(),
        );
    }
    if log4rs::init_file(&log_config, Default::default()).is_ok() {
        // logging is enabled and config is ok
        return true;
    }

    // Logging is enabled, but config is bad.
    if let Ok(mut file) = File::create(&log_config) {
        let config_template = LOGGING_CONFIG_YAML.replace("    ", "  ");
        if let Err(e) = file.write_all(config_template.as_bytes()) {
            panic_dialog!(
                "Failed to write to `{}`:\n{}",
                log_config.to_string_lossy(),
                e.to_string()
            );
        }
        if let Err(e) = file.flush() {
            panic_dialog!(
                "Failed to flush `{}` to disk:\n{}",
                log_config.to_string_lossy(),
                &e
            );
        }
    } else {
        panic_dialog!(
            "Failed to truncate `{}`. Please delete it and restart the application.",
            log_config.to_string_lossy()
        );
    }

    if log4rs::init_file(&log_config, Default::default()).is_ok() {
        // Logging is enabled and config was reset.
        return true;
    }

    panic_dialog!(
        "Failed to setup logging!\nPlease delete `{}` and restart the application.",
        log_config.to_string_lossy()
    );
}
