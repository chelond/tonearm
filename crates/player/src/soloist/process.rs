//! Runs Soloist as a child of Tonearm that cannot outlive it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Soloist's exit code for an expired build.
pub const EXIT_EXPIRED: i32 = 10;

pub struct Spawned {
    pub child: Child,
    pub ws_port: u16,
}

pub struct Args<'a> {
    pub exe: &'a Path,
    pub device_name: &'a str,
    pub api_key: &'a str,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub volume: u8,
    pub pipewire_device: Option<&'a str>,
}

/// No console (and no conhost.exe) for Soloist on Windows.
pub fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        // DETACHED_PROCESS: no console at all, so no conhost.exe next to Soloist (CREATE_NO_WINDOW
        // only hides the window and still costs a conhost). Its output is piped to soloist.log.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Starts Soloist with its WebSocket API on a free loopback port and waits for that port.
pub async fn spawn(args: &Args<'_>, log: PathBuf) -> std::io::Result<Spawned> {
    tokio::fs::create_dir_all(&args.data_dir).await?;
    tokio::fs::create_dir_all(&args.cache_dir).await?;
    stop_leftover(&args.data_dir).await;
    let port_file = args.data_dir.join("ws.port");
    let _ = tokio::fs::remove_file(&port_file).await;

    let mut cmd = Command::new(args.exe);
    cmd.arg("--device-name")
        .arg(args.device_name)
        .arg("--api-key")
        .arg(args.api_key)
        .arg("--data-dir")
        .arg(&args.data_dir)
        .arg("--cache-dir")
        .arg(&args.cache_dir)
        // --verbose: soloist.log then says why a sign-in did not go through.
        .args([
            "--verbose",
            "--cache-size",
            "200",
            "--ws",
            "127.0.0.1:0",
            "--initial-volume",
        ])
        .arg(args.volume.min(100).to_string());
    if let Some(node) = args.pipewire_device {
        cmd.arg("--pipewire-device").arg(node);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_window(&mut cmd);
    let mut child = cmd.spawn()?;
    tie_to_this_process(&child);
    keep_output(&mut child, log, args.api_key.to_owned());

    for _ in 0..150 {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(std::io::Error::other(exit_message(status.code())));
        }
        if let Ok(text) = tokio::fs::read_to_string(&port_file).await
            && let Ok(port) = text.trim().parse::<u16>()
            && port != 0
        {
            return Ok(Spawned {
                child,
                ws_port: port,
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill().await;
    Err(std::io::Error::other(
        "Soloist не открыл управление за 15 секунд",
    ))
}

#[must_use]
pub fn exit_message(code: Option<i32>) -> String {
    match code {
        Some(EXIT_EXPIRED) => "сборка Soloist устарела".into(),
        Some(c) => format!("Soloist завершился с кодом {c} (подробности в soloist.log)"),
        None => "Soloist остановлен".into(),
    }
}

/// Soloist's own output goes to `soloist.log` next to its data, with the key blanked out.
fn keep_output(child: &mut Child, log: PathBuf, key: String) {
    let streams: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for stream in streams {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        // Owner-only: Soloist's own output may carry account details.
        #[cfg(unix)]
        options.mode(0o600);
        let Ok(mut file) = options.open(&log).await else {
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A log left by an older version was created world-readable.
            let _ = file
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .await;
        }
        while let Some(line) = rx.recv().await {
            let line = if key.is_empty() {
                line
            } else {
                line.replace(&key, "***")
            };
            let _ = file.write_all(line.as_bytes()).await;
            let _ = file.write_all(b"\n").await;
        }
    });
}

/// A Soloist left by a Tonearm that was killed holds the data directory's lock: stop it.
/// Windows never leaves one (job object); elsewhere check `soloist.pid` points at a soloist.
async fn stop_leftover(data_dir: &Path) {
    #[cfg(target_os = "linux")]
    {
        let Ok(pid) = tokio::fs::read_to_string(data_dir.join("soloist.pid")).await else {
            return;
        };
        let pid = pid.trim();
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            return;
        }
        let exe = tokio::fs::read_link(format!("/proc/{pid}/exe")).await;
        if exe.is_ok_and(|p| p.file_name().is_some_and(|n| n == "soloist")) {
            tracing::info!(pid, "stopping a leftover soloist");
            let _ = Command::new("kill").arg(pid).status().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = data_dir;
}

/// Windows: put the child in a job object that kills it when Tonearm's handle closes, which
/// the system does however Tonearm ends (crash, Task Manager). Elsewhere `kill_on_drop` and a
/// normal quit cover it, and `stop_leftover` handles a killed Tonearm.
#[cfg(windows)]
#[allow(unsafe_code)]
fn tie_to_this_process(child: &Child) {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    // The job handle stays open for the life of the process (it must, to keep the limit).
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| {
        // SAFETY: plain Win32 calls; the struct is zero-initialised POD and outlives the call.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .unwrap_or(0),
            );
            if ok == 0 {
                return 0;
            }
            job as usize
        }
    });
    let Some(handle) = child.raw_handle() else {
        return;
    };
    if job != 0 {
        // SAFETY: both handles are valid: the job is never closed, the child is alive.
        let ok = unsafe { AssignProcessToJobObject(job as _, handle as _) };
        if ok == 0 {
            tracing::warn!("cannot tie soloist to tonearm's job object");
        }
    }
}

#[cfg(not(windows))]
fn tie_to_this_process(_: &Child) {}
