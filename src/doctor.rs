//! Read-only diagnostics. Never execute configured commands or expose configuration contents.

use crate::cli::{BackendArgs, BackendKind, resolve_backend};
use crate::config::{Config, LlmConfig, SignoffConfig, WrConfig};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Default)]
pub struct Report {
    pub checks: Vec<(Status, String, String)>,
}

impl Report {
    fn add(&mut self, status: Status, name: &str, detail: impl Into<String>) {
        self.checks.push((status, name.into(), detail.into()));
    }
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.0 == Status::Fail)
    }
    fn executable(&mut self, name: &str) {
        match find_executable(name, std::env::var_os("PATH").as_deref()) {
            Some(path) => self.add(Status::Pass, "Executable", path.display().to_string()),
            None => self.add(
                Status::Fail,
                "Executable",
                format!("{name}: install it or correct PATH / the configured command"),
            ),
        }
    }
    fn directory(&mut self, name: &str, path: &Path, writable: bool) {
        match std::fs::read_dir(path) {
            Err(_) => self.add(
                Status::Fail,
                name,
                format!(
                    "{}: directory is missing or unreadable; check path and permissions",
                    path.display()
                ),
            ),
            Ok(_) if writable && !can_write(path) => self.add(
                Status::Fail,
                name,
                format!("{}: no write access; check permissions", path.display()),
            ),
            Ok(_) => self.add(
                Status::Pass,
                name,
                format!("{}: accessible (no write probe performed)", path.display()),
            ),
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (status, name, detail) in &self.checks {
            let label = match status {
                Status::Pass => "PASS",
                Status::Warn => "WARN",
                Status::Fail => "FAIL",
                Status::Skip => "SKIP",
            };
            // Keep output on one line even when a path contains control characters.
            let safe: String = detail
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            writeln!(f, "{label}  {name}: {safe}")?;
        }
        Ok(())
    }
}

fn can_write(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // access only checks permissions and does not change the filesystem.
        unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path).is_ok_and(|m| !m.permissions().readonly())
    }
}

fn find_executable(name: &str, search: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let candidates = if Path::new(name).components().count() > 1 {
        vec![PathBuf::from(name)]
    } else {
        search
            .map(|p| std::env::split_paths(p).map(|d| d.join(name)).collect())
            .unwrap_or_default()
    };
    candidates.into_iter().find(|p| {
        if !p.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

pub fn check(home: &Path) -> Report {
    let mut report = Report::default();
    let path = Config::default_path(home);
    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(_) => {
            // TOML parser errors can echo lines containing credentials.
            report.add(
                Status::Fail,
                "Config",
                format!(
                    "{}: cannot read or parse configuration; check permissions and TOML syntax",
                    path.display()
                ),
            );
            return report;
        }
    };
    report.add(
        if path.exists() {
            Status::Pass
        } else {
            Status::Skip
        },
        "Config",
        path.display().to_string(),
    );
    inspect(&mut report, home, &config);
    report
}

fn inspect(report: &mut Report, home: &Path, config: &Config) {
    let wr = config.wr != WrConfig::default();
    let signoff = config.signoff != SignoffConfig::default();
    let llm = config.llm != LlmConfig::default() || wr || signoff;
    if config.wr.days.is_some_and(|days| days == 0 || days > 366) {
        report.add(
            Status::Fail,
            "Weekly range",
            "Set wr.days between 1 and 366",
        );
    }
    report.add(
        if wr { Status::Pass } else { Status::Skip },
        "Weekly report",
        if wr {
            "Configured"
        } else {
            "No weekly-report options configured"
        },
    );
    if llm {
        match resolve_backend(&BackendArgs::default(), config, None) {
            Err(_) => report.add(
                Status::Fail,
                "AI backend",
                "Invalid backend; choose cli or api",
            ),
            Ok(backend) => {
                report.add(
                    if backend.timeout_secs == 0 {
                        Status::Fail
                    } else {
                        Status::Pass
                    },
                    "AI timeout",
                    format!(
                        "{} seconds; configure llm.timeout_secs to a positive value",
                        backend.timeout_secs
                    ),
                );
                match backend.kind {
                    BackendKind::Cli => {
                        use crate::report::cli_backend::{parse_custom_cmd, preset};
                        let spec = match &backend.cli_cmd {
                            Some(cmd) => parse_custom_cmd(cmd).ok(),
                            None => preset(&backend.cli_name),
                        };
                        match spec {
                            Some(spec) => report.executable(&spec.program),
                            None => report.add(Status::Fail, "AI CLI", "Unknown preset or empty command; correct llm.cli_name / llm.cli_cmd"),
                        }
                        report.add(Status::Warn, "AI authentication", "Not verified; authenticate with your selected CLI if needed (no model invocation performed)");
                    }
                    BackendKind::Api => {
                        let present =
                            std::env::var(&backend.api_key_env).is_ok_and(|v| !v.trim().is_empty());
                        report.add(
                            if present { Status::Pass } else { Status::Fail },
                            "API key",
                            if present {
                                "Configured environment variable is nonempty (value hidden)"
                            } else {
                                "Set the environment variable named by llm.api_key_env"
                            },
                        );
                        let valid = reqwest::Url::parse(&backend.api_base_url).is_ok_and(|url| {
                            matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                        });
                        report.add(
                            if valid { Status::Pass } else { Status::Fail },
                            "API endpoint",
                            if valid {
                                "HTTP(S) URL is valid (value hidden)"
                            } else {
                                "Correct llm.api_base_url to an HTTP(S) URL"
                            },
                        );
                        report.add(Status::Warn, "API connectivity", "Authentication, model availability and quota are not verified; no request sent");
                    }
                }
                if let Some(path) = backend.template {
                    report.add(
                        if std::fs::read_to_string(path).is_ok() {
                            Status::Pass
                        } else {
                            Status::Fail
                        },
                        "Prompt template",
                        "Configured template must be readable UTF-8; contents are not displayed",
                    );
                }
            }
        }
    } else {
        report.add(
            Status::Skip,
            "AI backend",
            "No AI or report options configured",
        );
    }

    for source in crate::sources::default_sources(home) {
        if source.root().exists() {
            report.directory(source.kind().display_name(), source.root(), false);
            let mut files = false;
            let mut unreadable = false;
            for entry in walkdir::WalkDir::new(source.root()).follow_links(false) {
                match entry {
                    Ok(e) => files |= e.file_type().is_file(),
                    Err(_) => unreadable = true,
                }
            }
            if !files || unreadable {
                report.add(Status::Warn, "History", "No files found or some directories unreadable; inspect source permissions (message parsing not verified)");
            }
        } else {
            report.add(
                Status::Skip,
                source.kind().display_name(),
                "Local history directory not present",
            );
        }
    }
    if config.sync.path.is_some() || config.sync.device.is_some() {
        if crate::cli::configured_sync(config, home, None).is_err() {
            report.add(
                Status::Fail,
                "Sync configuration",
                "Invalid sync settings; check sync.path and sync.device",
            );
        }
    }
    if let Some(path) = &config.sync.path {
        report.directory("Sync archive", path, true);
        let homes = crate::sync::archive_homes(path);
        if homes.is_empty() {
            report.add(Status::Warn, "Archived devices", "No device homes discovered; run buddy sync or inspect archive directory permissions");
        }
        for archived in homes {
            report.directory("Archived device home", &archived, false);
        }
        report.add(
            Status::Warn,
            "Cloud replication",
            "Not verified; confirm uploads/downloads in your sync product before collecting",
        );
    } else {
        report.add(Status::Skip, "Sync", "No sync.path configured");
    }

    let email = config
        .wr
        .mail_to
        .iter()
        .chain(config.signoff.mail_to.iter())
        .any(|to| !to.is_empty());
    if email {
        report.executable("mutt");
        let candidates = [
            home.join(".muttrc"),
            home.join(".mutt/muttrc"),
            home.join(".config/mutt/muttrc"),
            PathBuf::from("/etc/Muttrc"),
        ];
        report.add(if candidates.iter().any(|p| std::fs::File::open(p).is_ok()) { Status::Pass } else { Status::Warn }, "Mutt configuration", "Conventional configuration paths checked for readability only; configure mutt if absent");
        report.add(Status::Warn, "Email delivery", "SMTP credentials, included configuration and delivery are not verified; no configuration executed or email sent");
    } else {
        report.add(Status::Skip, "Email", "No recipients configured");
    }
    if signoff {
        report.executable("git");
        for workspace in config.signoff.workspaces.iter().flatten() {
            report.directory("Signoff workspace", Path::new(&workspace.path), false);
        }
        report.add(
            Status::Warn,
            "Signoff trust",
            "Unlisted workspaces default to yolo; review workspace trust before enabling actions",
        );
    } else {
        report.add(Status::Skip, "Signoff", "No signoff options configured");
    }
    if llm || config.out_dir.is_some() {
        let out = Path::new(config.out_dir.as_deref().unwrap_or("out"));
        let absolute = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(out);
        let parent = absolute
            .ancestors()
            .find(|p| p.exists())
            .unwrap_or(&absolute);
        report.directory("Output directory or existing ancestor", parent, true);
        recent_run(report, out);
    } else {
        report.add(
            Status::Skip,
            "Output and workers",
            "No output or report options configured",
        );
    }
}

fn recent_run(report: &mut Report, out: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    let latest = walkdir::WalkDir::new(out)
        .max_depth(3)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && (e.file_name() == "report.log" || e.file_name() == "signoff.log")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.into_path())))
        .max();
    let Some((_, path)) = latest else {
        report.add(
            Status::Skip,
            "Recent run",
            "No report.log or signoff.log found",
        );
        return;
    };
    let mut tail = Vec::new();
    let read = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::open(&path)?;
        let size = file.metadata()?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(65536)))?;
        file.take(65536).read_to_end(&mut tail)?;
        Ok(())
    })();
    if read.is_err() {
        report.add(
            Status::Warn,
            "Recent log",
            "Cannot read latest log; inspect file permissions",
        );
        return;
    }
    let text = String::from_utf8_lossy(&tail).to_lowercase();
    let failed = text.contains("error:") || text.contains("timed out");
    report.add(
        if failed { Status::Warn } else { Status::Pass },
        "Recent log",
        format!(
            "{}: {} (last 64 KiB inspected, not proof of current health)",
            path.display(),
            if failed {
                "historical error/timeout found; inspect locally"
            } else {
                "no error/timeout marker found"
            }
        ),
    );
    if path.with_extension("pid").exists() {
        report.add(Status::Warn, "Worker PID", "PID file exists beside latest log; it may be retained after completion. Process identity and duplicate workers are not verified; inspect locally before stopping anything");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_skips_optional_dependencies_and_does_not_write() {
        let home = tempfile::tempdir().unwrap();
        let report = check(home.path());
        assert!(!report.failed());
        assert!(!report.to_string().contains("Executable:"));
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[test]
    fn malformed_config_does_not_echo_secret() {
        let home = tempfile::tempdir().unwrap();
        let path = Config::default_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "secret = \"private-value\" invalid").unwrap();
        let report = check(home.path());
        assert!(report.failed());
        assert!(!report.to_string().contains("private-value"));
    }

    #[test]
    fn missing_configured_archive_fails_without_creating_it() {
        let home = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.sync.path = Some(home.path().join("missing"));
        let mut report = Report::default();
        inspect(&mut report, home.path(), &config);
        assert!(report.failed());
        assert!(!config.sync.path.unwrap().exists());
    }

    #[test]
    fn executable_lookup_uses_injected_search_path() {
        let home = tempfile::tempdir().unwrap();
        assert!(find_executable("missing-buddy-tool", Some(home.path().as_os_str())).is_none());
        let path = home.path().join("fake-tool");
        std::fs::write(&path, "not executed").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(find_executable("fake-tool", Some(home.path().as_os_str())).is_none());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(
            find_executable("fake-tool", Some(home.path().as_os_str())),
            Some(path)
        );
    }

    #[test]
    fn historical_failure_warns_without_exposing_log_body() {
        let out = tempfile::tempdir().unwrap();
        std::fs::write(
            out.path().join("report.log"),
            "Error: timed out private-conversation",
        )
        .unwrap();
        let mut report = Report::default();
        recent_run(&mut report, out.path());
        assert!(!report.failed());
        assert!(report.to_string().contains("WARN"));
        assert!(!report.to_string().contains("private-conversation"));
    }
}
