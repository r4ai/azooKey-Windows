use anyhow::{bail, Context as _};
use shared::AppConfig;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;
use std::{env, thread};

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, PartialEq, Eq)]
struct RuntimePaths {
    app_dir: PathBuf,
    backend_dir: PathBuf,
    server: PathBuf,
    ui: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let config = AppConfig::new();
    let current_exe = env::current_exe().context("failed to locate launcher.exe")?;
    let paths = runtime_paths(&current_exe, &config.zenzai.backend)?;

    prepend_to_path(&paths.backend_dir)?;

    let mut server = start_process(&paths.server, &paths.app_dir, "[server]")?;
    let mut ui = match start_process(&paths.ui, &paths.app_dir, "[ui]") {
        Ok(ui) => ui,
        Err(error) => {
            let _ = terminate_process_tree(&mut server);
            return Err(error);
        }
    };

    supervise_processes(&mut server, &mut ui)
}

fn runtime_paths(current_exe: &Path, configured_backend: &str) -> anyhow::Result<RuntimePaths> {
    let app_dir = current_exe
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .context("launcher.exe has no parent directory")?
        .to_path_buf();
    let backend_name = match configured_backend {
        "cpu" => "llama_cpu",
        "cuda" => "llama_cuda",
        "vulkan" => "llama_vulkan",
        _ => "llama_cpu",
    };

    Ok(RuntimePaths {
        backend_dir: app_dir.join(backend_name),
        server: app_dir.join("azookey-server.exe"),
        ui: app_dir.join("ui.exe"),
        app_dir,
    })
}

fn prepend_to_path(directory: &Path) -> anyhow::Result<()> {
    let mut entries = vec![directory.to_path_buf()];
    if let Some(existing_path) = env::var_os("PATH") {
        entries.extend(env::split_paths(&existing_path));
    }
    let joined = env::join_paths(entries).context("failed to construct the runtime PATH")?;
    env::set_var("PATH", joined);
    Ok(())
}

fn start_process(exe: &Path, app_dir: &Path, prefix: &str) -> anyhow::Result<Child> {
    let mut child = Command::new(exe)
        .current_dir(app_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {}", exe.display()))?;

    let stdout = child
        .stdout
        .take()
        .context("failed to capture child stdout")?;
    let stdout_reader = BufReader::new(stdout);
    let prefix_stdout = prefix.to_string();
    thread::spawn(move || {
        for line in stdout_reader.lines().map_while(Result::ok) {
            println!("{}: {}", prefix_stdout, line);
        }
    });

    let stderr = child
        .stderr
        .take()
        .context("failed to capture child stderr")?;
    let stderr_reader = BufReader::new(stderr);
    let prefix_stderr = prefix.to_string();
    thread::spawn(move || {
        for line in stderr_reader.lines().map_while(Result::ok) {
            eprintln!("{}: {}", prefix_stderr, line);
        }
    });

    Ok(child)
}

fn supervise_processes(server: &mut Child, ui: &mut Child) -> anyhow::Result<()> {
    loop {
        if let Some(status) = server.try_wait().context("failed to query server status")? {
            let cleanup_error = terminate_process_tree(ui).err();
            return unexpected_exit("azookey-server.exe", status, "ui.exe", cleanup_error);
        }
        if let Some(status) = ui.try_wait().context("failed to query UI status")? {
            let cleanup_error = terminate_process_tree(server).err();
            return unexpected_exit("ui.exe", status, "azookey-server.exe", cleanup_error);
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

fn unexpected_exit(
    exited_name: &str,
    status: ExitStatus,
    terminated_name: &str,
    cleanup_error: Option<anyhow::Error>,
) -> anyhow::Result<()> {
    if let Some(error) = cleanup_error {
        bail!(
            "{exited_name} exited unexpectedly with {status}; failed to stop {terminated_name}: {error:#}"
        );
    }
    bail!("{exited_name} exited unexpectedly with {status}; stopped {terminated_name}")
}

fn terminate_process_tree(child: &mut Child) -> anyhow::Result<()> {
    if child
        .try_wait()
        .context("failed to query child status before termination")?
        .is_some()
    {
        return Ok(());
    }

    let taskkill =
        PathBuf::from(env::var_os("WINDIR").unwrap_or_else(|| OsString::from(r"C:\Windows")))
            .join("System32")
            .join("taskkill.exe");
    let status = Command::new(&taskkill)
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to start {}", taskkill.display()))?;

    if !status.success() && child.try_wait()?.is_none() {
        child.kill().context("failed to terminate child process")?;
    }
    child
        .wait()
        .context("failed to wait for terminated child")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD_ROLE: &str = "AZOOKEY_LAUNCHER_TEST_CHILD_ROLE";

    #[test]
    fn runtime_paths_are_absolute_siblings_of_launcher() {
        let paths = runtime_paths(
            Path::new(r"C:\Program Files\Azookey\launcher.exe"),
            "vulkan",
        )
        .unwrap();

        assert_eq!(paths.app_dir, PathBuf::from(r"C:\Program Files\Azookey"));
        assert_eq!(
            paths.backend_dir,
            PathBuf::from(r"C:\Program Files\Azookey\llama_vulkan")
        );
        assert_eq!(
            paths.server,
            PathBuf::from(r"C:\Program Files\Azookey\azookey-server.exe")
        );
        assert_eq!(paths.ui, PathBuf::from(r"C:\Program Files\Azookey\ui.exe"));
    }

    #[test]
    fn unknown_backend_falls_back_to_cpu() {
        let paths = runtime_paths(Path::new(r"D:\Azookey\launcher.exe"), "unknown").unwrap();

        assert_eq!(paths.backend_dir, PathBuf::from(r"D:\Azookey\llama_cpu"));
    }

    #[test]
    fn supervision_stops_ui_when_server_exits() {
        if env::var_os(CHILD_ROLE).is_some() {
            return;
        }

        let mut server = spawn_test_child("exit");
        let mut ui = spawn_test_child("wait");
        let error = supervise_processes(&mut server, &mut ui).unwrap_err();

        assert!(error
            .to_string()
            .contains("azookey-server.exe exited unexpectedly"));
        assert!(ui.try_wait().unwrap().is_some());
    }

    #[test]
    fn supervision_stops_server_when_ui_exits() {
        if env::var_os(CHILD_ROLE).is_some() {
            return;
        }

        let mut server = spawn_test_child("wait");
        let mut ui = spawn_test_child("exit");
        let error = supervise_processes(&mut server, &mut ui).unwrap_err();

        assert!(error.to_string().contains("ui.exe exited unexpectedly"));
        assert!(server.try_wait().unwrap().is_some());
    }

    #[test]
    fn process_helper() {
        match env::var(CHILD_ROLE).as_deref() {
            Ok("wait") => thread::sleep(Duration::from_secs(30)),
            Ok("exit") | Err(_) => {}
            Ok(role) => panic!("unexpected child role: {role}"),
        }
    }

    fn spawn_test_child(role: &str) -> Child {
        Command::new(env::current_exe().unwrap())
            .args(["--exact", "tests::process_helper"])
            .env(CHILD_ROLE, role)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
}
