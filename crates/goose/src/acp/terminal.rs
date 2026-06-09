//! Client-side terminal support for goose acting as an ACP client.
//!
//! When goose bridges to an external ACP agent (e.g. Claude Code), the agent
//! runs shell commands by asking goose — the client — to execute them through
//! the `terminal/*` methods. goose owns the process, captures its output, and
//! reports exit status back. Embedding a terminal in a tool call lets the agent
//! release it while the captured output stays available, so this manager
//! retains output after release to resolve those embeds into text.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    ContentBlock, CreateTerminalRequest, TerminalExitStatus, TerminalId, TerminalOutputResponse,
    TextContent, ToolCallContent,
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{watch, Notify};

use crate::subprocess::configure_subprocess;

/// Output cap applied when the agent does not request one, bounding memory for
/// commands that emit unbounded output.
const DEFAULT_OUTPUT_BYTE_LIMIT: usize = 1 << 20;

/// Number of released terminals whose output we keep for tool-call embeds.
const RETAINED_CAP: usize = 256;

const READ_CHUNK: usize = 8192;

#[derive(Clone)]
pub(crate) struct TerminalManager {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: Mutex<HashMap<String, Arc<TerminalSession>>>,
    retained: Mutex<RetainedOutputs>,
    next_id: AtomicU64,
    work_dir: PathBuf,
}

struct TerminalSession {
    session_id: String,
    buffer: Arc<Mutex<TerminalBuffer>>,
    exit_rx: watch::Receiver<Option<TerminalExitStatus>>,
    kill: Arc<Notify>,
}

struct TerminalBuffer {
    data: Vec<u8>,
    truncated: bool,
    byte_limit: usize,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(sessions) = self.sessions.lock() {
            for session in sessions.values() {
                session.kill.notify_one();
            }
        }
    }
}

impl TerminalBuffer {
    fn append(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > self.byte_limit {
            let overflow = self.data.len() - self.byte_limit;
            self.data.drain(0..overflow);
            while std::str::from_utf8(&self.data).is_err_and(|e| e.valid_up_to() == 0) {
                self.data.drain(..1);
            }
            self.truncated = true;
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

#[derive(Default)]
struct RetainedOutputs {
    map: HashMap<String, String>,
    order: VecDeque<String>,
}

impl RetainedOutputs {
    fn insert(&mut self, id: String, output: String) {
        if self.map.insert(id.clone(), output).is_none() {
            self.order.push_back(id);
            while self.order.len() > RETAINED_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

impl TerminalManager {
    pub(crate) fn new(work_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                retained: Mutex::new(RetainedOutputs::default()),
                next_id: AtomicU64::new(0),
                work_dir,
            }),
        }
    }

    pub(crate) async fn create(
        &self,
        request: CreateTerminalRequest,
    ) -> std::io::Result<TerminalId> {
        let cwd = request
            .cwd
            .clone()
            .unwrap_or_else(|| self.inner.work_dir.clone());
        let byte_limit = request
            .output_byte_limit
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
            .unwrap_or(DEFAULT_OUTPUT_BYTE_LIMIT);
        let session_id = request.session_id.0.to_string();

        let mut cmd = terminal_command(&request.command, &request.args);
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for var in &request.env {
            cmd.env(&var.name, &var.value);
        }
        configure_subprocess(&mut cmd);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let buffer = Arc::new(Mutex::new(TerminalBuffer {
            data: Vec::new(),
            truncated: false,
            byte_limit,
        }));
        let (exit_tx, exit_rx) = watch::channel(None);
        let kill = Arc::new(Notify::new());

        {
            let buffer = buffer.clone();
            let kill = kill.clone();
            tokio::spawn(async move {
                let pumps = [
                    stdout.map(|s| tokio::spawn(pump(s, buffer.clone()))),
                    stderr.map(|s| tokio::spawn(pump(s, buffer.clone()))),
                ];
                let status = tokio::select! {
                    s = child.wait() => s,
                    _ = kill.notified() => {
                        kill_terminal_process(&mut child);
                        child.wait().await
                    }
                };
                for pump in pumps.into_iter().flatten() {
                    let _ = pump.await;
                }
                let _ = exit_tx.send(Some(to_exit_status(status)));
            });
        }

        let id = format!(
            "term-{}",
            self.inner.next_id.fetch_add(1, Ordering::Relaxed)
        );
        if let Ok(mut sessions) = self.inner.sessions.lock() {
            sessions.insert(
                id.clone(),
                Arc::new(TerminalSession {
                    session_id,
                    buffer,
                    exit_rx,
                    kill,
                }),
            );
        }
        Ok(TerminalId::new(id))
    }

    pub(crate) fn output(&self, session_id: &str, id: &str) -> Option<TerminalOutputResponse> {
        let session = self.session(session_id, id)?;
        let (output, truncated) = {
            let buffer = session.buffer.lock().expect("terminal buffer poisoned");
            (buffer.output(), buffer.truncated)
        };
        let exit_status = session.exit_rx.borrow().clone();
        Some(TerminalOutputResponse::new(output, truncated).exit_status(exit_status))
    }

    pub(crate) async fn wait_for_exit(
        &self,
        session_id: &str,
        id: &str,
    ) -> Option<TerminalExitStatus> {
        let exit_rx = self.session(session_id, id)?.exit_rx.clone();
        wait_for_exit(exit_rx).await
    }

    pub(crate) fn kill(&self, session_id: &str, id: &str) {
        if let Some(session) = self.session(session_id, id) {
            session.kill.notify_one();
        }
    }

    pub(crate) async fn release(&self, session_id: &str, id: &str) {
        let Some(session) = self.session(session_id, id) else {
            return;
        };
        session.kill.notify_one();
        let _ = wait_for_exit(session.exit_rx.clone()).await;
        let removed = self
            .inner
            .sessions
            .lock()
            .ok()
            .and_then(|mut sessions| sessions.remove(id));
        if let Some(session) = removed {
            let output = session
                .buffer
                .lock()
                .map(|buffer| buffer.output())
                .unwrap_or_default();
            if let Ok(mut retained) = self.inner.retained.lock() {
                retained.insert(id.to_string(), output);
            }
        }
    }

    pub(crate) fn close_session(&self, session_id: &str) {
        let removed = self
            .inner
            .sessions
            .lock()
            .map(|mut sessions| {
                let ids = sessions
                    .iter()
                    .filter(|(_, terminal)| terminal.session_id == session_id)
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                ids.into_iter()
                    .filter_map(|id| sessions.remove(&id))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for session in removed {
            session.kill.notify_one();
        }
    }

    fn session(&self, session_id: &str, id: &str) -> Option<Arc<TerminalSession>> {
        let session = self.inner.sessions.lock().ok()?.get(id).cloned()?;
        (session.session_id == session_id).then_some(session)
    }

    /// Replace any embedded `terminal/*` references with the terminal's captured
    /// output so downstream consumers see real text instead of an opaque id.
    pub(crate) fn resolve_content(&self, content: Vec<ToolCallContent>) -> Vec<ToolCallContent> {
        content
            .into_iter()
            .map(|block| match block {
                ToolCallContent::Terminal(terminal) => {
                    let output = self
                        .resolve_output(terminal.terminal_id.0.as_ref())
                        .unwrap_or_default();
                    ToolCallContent::from(ContentBlock::Text(TextContent::new(output)))
                }
                other => other,
            })
            .collect()
    }

    fn resolve_output(&self, id: &str) -> Option<String> {
        if let Ok(sessions) = self.inner.sessions.lock() {
            if let Some(session) = sessions.get(id) {
                return Some(
                    session
                        .buffer
                        .lock()
                        .map(|buffer| buffer.output())
                        .unwrap_or_default(),
                );
            }
        }
        self.inner
            .retained
            .lock()
            .ok()
            .and_then(|retained| retained.map.get(id).cloned())
    }
}

fn terminal_command(command: &str, args: &[String]) -> Command {
    if !args.is_empty() {
        let mut process = Command::new(command);
        process.args(args);
        return process;
    }

    #[cfg(windows)]
    let process = {
        let mut process = Command::new("cmd.exe");
        process.args(["/D", "/S", "/C", command]);
        process
    };
    #[cfg(not(windows))]
    let process = {
        let mut process = Command::new("/bin/sh");
        process.args(["-c", command]);
        process
    };
    process
}

fn kill_terminal_process(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: configure_subprocess created a process group whose id is this child pid.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

async fn wait_for_exit(
    mut exit_rx: watch::Receiver<Option<TerminalExitStatus>>,
) -> Option<TerminalExitStatus> {
    loop {
        if let Some(status) = exit_rx.borrow().clone() {
            return Some(status);
        }
        if exit_rx.changed().await.is_err() {
            return exit_rx.borrow().clone();
        }
    }
}

async fn pump<R: AsyncRead + Unpin>(mut reader: R, buffer: Arc<Mutex<TerminalBuffer>>) {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut buffer) = buffer.lock() {
                    buffer.append(&chunk[..n]);
                }
            }
        }
    }
}

fn to_exit_status(status: std::io::Result<std::process::ExitStatus>) -> TerminalExitStatus {
    let Ok(status) = status else {
        return TerminalExitStatus::new();
    };
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status).map(|s| s.to_string());
    #[cfg(not(unix))]
    let signal: Option<String> = None;
    TerminalExitStatus::new()
        .exit_code(status.code().map(|c| c as u32))
        .signal(signal)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{SessionId, Terminal};
    use tokio::time::{timeout, Duration};

    fn request(command: &str, args: &[&str]) -> CreateTerminalRequest {
        CreateTerminalRequest::new(SessionId::new("s"), command)
            .args(args.iter().map(|a| a.to_string()).collect())
    }

    #[tokio::test]
    async fn captures_output_and_exit_status() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(request("sh", &["-c", "printf hello"]))
            .await
            .unwrap();
        let id = id.0.to_string();

        let status = manager.wait_for_exit("s", &id).await.unwrap();
        assert_eq!(status.exit_code, Some(0));

        let output = manager.output("s", &id).unwrap();
        assert_eq!(output.output, "hello");
        assert!(!output.truncated);
        assert_eq!(output.exit_status.unwrap().exit_code, Some(0));
    }

    #[tokio::test]
    async fn executes_a_shell_command_when_args_are_omitted() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(CreateTerminalRequest::new(
                SessionId::new("s"),
                "printf combined",
            ))
            .await
            .unwrap();

        let status = manager.wait_for_exit("s", id.0.as_ref()).await.unwrap();

        assert_eq!(status.exit_code, Some(0));
        assert_eq!(
            manager.output("s", id.0.as_ref()).unwrap().output,
            "combined"
        );
    }

    #[tokio::test]
    async fn resolves_embedded_terminal_to_text() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(request("sh", &["-c", "printf done"]))
            .await
            .unwrap();
        manager.wait_for_exit("s", id.0.as_ref()).await;

        let resolved = manager.resolve_content(vec![ToolCallContent::Terminal(Terminal::new(id))]);

        match &resolved[..] {
            [ToolCallContent::Content(content)] => match &content.content {
                ContentBlock::Text(text) => assert_eq!(text.text, "done"),
                other => panic!("expected text content, got {other:?}"),
            },
            other => panic!("expected one resolved content block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retains_output_after_release() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(request("sh", &["-c", "printf kept"]))
            .await
            .unwrap();
        let id = id.0.to_string();
        manager.wait_for_exit("s", &id).await;

        manager.release("s", &id).await;
        assert!(manager.output("s", &id).is_none());
        assert_eq!(manager.resolve_output(&id).as_deref(), Some("kept"));
    }

    #[tokio::test]
    async fn nonzero_exit_code_is_reported() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(request("sh", &["-c", "exit 3"]))
            .await
            .unwrap();
        let status = manager.wait_for_exit("s", id.0.as_ref()).await.unwrap();
        assert_eq!(status.exit_code, Some(3));
    }

    #[tokio::test]
    async fn terminals_are_isolated_by_session() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(request("sh", &["-c", "printf private"]))
            .await
            .unwrap();
        manager.wait_for_exit("s", id.0.as_ref()).await;

        assert!(manager.output("other", id.0.as_ref()).is_none());
        assert_eq!(
            manager.output("s", id.0.as_ref()).unwrap().output,
            "private"
        );
    }

    #[tokio::test]
    async fn kill_stops_a_running_terminal() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(request("sh", &["-c", "sleep 30"]))
            .await
            .unwrap();

        manager.kill("s", id.0.as_ref());
        let status = timeout(
            Duration::from_secs(2),
            manager.wait_for_exit("s", id.0.as_ref()),
        )
        .await
        .expect("killed terminal should exit promptly")
        .unwrap();

        assert!(status.exit_code.is_none() || status.exit_code != Some(0));
    }

    #[tokio::test]
    async fn release_waits_for_final_output() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(request("sh", &["-c", "printf ready; sleep 30"]))
            .await
            .unwrap();
        let id = id.0.to_string();

        timeout(Duration::from_secs(2), async {
            loop {
                if manager
                    .output("s", &id)
                    .is_some_and(|output| output.output == "ready")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal should publish initial output");

        manager.release("s", &id).await;
        assert_eq!(manager.resolve_output(&id).as_deref(), Some("ready"));
    }

    #[tokio::test]
    async fn closing_a_session_kills_its_terminals() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let id = manager
            .create(request("sh", &["-c", "sleep 30"]))
            .await
            .unwrap();
        let exit_rx = manager.session("s", id.0.as_ref()).unwrap().exit_rx.clone();

        manager.close_session("s");

        timeout(Duration::from_secs(2), wait_for_exit(exit_rx))
            .await
            .expect("session cleanup should stop its terminals")
            .unwrap();
        assert!(manager.output("s", id.0.as_ref()).is_none());
    }

    #[tokio::test]
    async fn concurrent_terminals_keep_separate_output() {
        let manager = TerminalManager::new(PathBuf::from("."));
        let first = manager
            .create(request("sh", &["-c", "printf first"]))
            .await
            .unwrap();
        let second = manager
            .create(request("sh", &["-c", "printf second"]))
            .await
            .unwrap();

        let (first_status, second_status) = tokio::join!(
            manager.wait_for_exit("s", first.0.as_ref()),
            manager.wait_for_exit("s", second.0.as_ref())
        );

        assert_eq!(first_status.unwrap().exit_code, Some(0));
        assert_eq!(second_status.unwrap().exit_code, Some(0));
        assert_eq!(
            manager.output("s", first.0.as_ref()).unwrap().output,
            "first"
        );
        assert_eq!(
            manager.output("s", second.0.as_ref()).unwrap().output,
            "second"
        );
    }

    #[test]
    fn buffer_truncates_from_front() {
        let mut buffer = TerminalBuffer {
            data: Vec::new(),
            truncated: false,
            byte_limit: 4,
        };
        buffer.append(b"abcdef");
        assert_eq!(buffer.output(), "cdef");
        assert!(buffer.truncated);
    }

    #[test]
    fn buffer_truncates_to_utf8_boundary() {
        let mut buffer = TerminalBuffer {
            data: Vec::new(),
            truncated: false,
            byte_limit: 4,
        };
        buffer.append("ééé".as_bytes());
        assert_eq!(buffer.output(), "éé");
        assert!(buffer.truncated);
    }
}
