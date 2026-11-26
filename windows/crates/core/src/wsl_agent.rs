//! The Windows side of the WSL helper: one persistent `wsl.exe` per project. It reports inotify changes and runs
//! git requests over the same stdin/stdout, so status, diffs and history no longer start a process per call.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::Duration;

use crate::wsl::{self, Location};

const REQUEST: u8 = 1;
const RESPONSE: u8 = 2;
const CHANGE: u8 = 3;
const READY: u8 = 4;
const FULL: u8 = 5;
const READY_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone, Debug)]
pub struct Response {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum AgentError {
    /// The agent was not running when the request was made; the caller may use the wsl.exe fallback.
    Unavailable(String),
    /// The request was sent but no usable answer came back.
    Failed(String),
}

type ChangeCallback = Box<dyn Fn(Vec<String>, bool) + Send + Sync>;

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn take_bytes<'a>(data: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    let head = data.get(*at..*at + 4)?;
    let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    *at += 4;
    let bytes = data.get(*at..*at + len)?;
    *at += len;
    Some(bytes)
}

fn write_frame(out: &mut ChildStdin, kind: u8, id: u32, payload: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 9];
    header[0] = kind;
    header[1..5].copy_from_slice(&id.to_le_bytes());
    header[5..9].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    out.write_all(&header)?;
    out.write_all(payload)?;
    out.flush()
}

fn read_frame(input: &mut impl Read) -> Option<(u8, u32, Vec<u8>)> {
    let mut header = [0u8; 9];
    input.read_exact(&mut header).ok()?;
    let kind = header[0];
    let id = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]);
    if len > 256 * 1024 * 1024 {
        return None;
    }
    let mut payload = vec![0u8; len as usize];
    input.read_exact(&mut payload).ok()?;
    Some((kind, id, payload))
}

/// The helper installed beside DiffCode, or the one under development.
fn helper_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DIFFCODE_WSL_AGENT") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("wsl").join("diffcode-wsl-agent"),
        dir.join("diffcode-wsl-agent"),
        dir.join("..").join("wsl").join("diffcode-wsl-agent"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

pub struct Agent {
    location: Location,
    root: PathBuf,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next: AtomicU32,
    pending: Mutex<HashMap<u32, mpsc::Sender<Response>>>,
    watch: ChangeCallback,
    alive: AtomicBool,
}

impl Agent {
    /// Starts the helper in the project's distro. The callback gets work-tree paths relative to the root, and
    /// whether git's own state (HEAD, index, refs) changed.
    pub fn start(
        root: &Path,
        on_change: impl Fn(Vec<String>, bool) + Send + Sync + 'static,
    ) -> Result<Arc<Agent>, String> {
        let location = wsl::location(root).ok_or_else(|| "not a WSL folder".to_string())?;
        let helper = helper_path().ok_or_else(|| "diffcode-wsl-agent was not found beside DiffCode".to_string())?;
        let helper_linux = location
            .linux_path(&helper.to_string_lossy())
            .ok_or_else(|| format!("WSL cannot reach the helper at {}", helper.display()))?;
        let mut command = Command::new("wsl.exe");
        command.args(location.command_line(&helper_linux));
        command.arg("--watch").arg(&location.linux);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        crate::git::hide_console(&mut command);
        let mut child = command.spawn().map_err(|error| format!("could not start wsl.exe: {error}"))?;
        let stdin = child.stdin.take().ok_or_else(|| "wsl.exe has no stdin".to_string())?;
        let stdout = child.stdout.take().ok_or_else(|| "wsl.exe has no stdout".to_string())?;
        let stderr = child.stderr.take();
        let agent = Arc::new(Agent {
            location,
            root: root.to_path_buf(),
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            watch: Box::new(on_change),
            alive: AtomicBool::new(true),
        });
        if let Some(mut stderr) = stderr {
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while stderr.read(&mut buffer).map(|n| n > 0).unwrap_or(false) {}
            });
        }
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let weak = Arc::downgrade(&agent);
        std::thread::Builder::new()
            .name("diffcode-wsl-agent".into())
            .spawn(move || read_loop(weak, stdout, ready_tx))
            .map_err(|error| format!("could not start the WSL agent reader: {error}"))?;
        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => {
                register(&agent);
                Ok(agent)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let _ = agent.child.lock().map(|mut child| child.kill());
                Err("the WSL helper did not become ready".to_string())
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn location(&self) -> &Location {
        &self.location
    }

    pub fn is_alive(&self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        let Ok(mut child) = self.child.lock() else { return false };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => {
                self.alive.store(false, Ordering::SeqCst);
                false
            }
            Err(_) => false,
        }
    }

    pub fn run(
        &self,
        cwd: &str,
        args: &[String],
        envs: &[(String, String)],
        stdin: &[u8],
    ) -> Result<Response, AgentError> {
        if !self.is_alive() {
            return Err(AgentError::Unavailable("the WSL agent is not running".to_string()));
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().map_err(|_| AgentError::Failed("the WSL agent lock failed".into()))?.insert(id, tx);
        let mut payload = Vec::new();
        put_bytes(&mut payload, cwd.as_bytes());
        payload.extend_from_slice(&(args.len() as u32).to_le_bytes());
        for arg in args {
            put_bytes(&mut payload, arg.as_bytes());
        }
        payload.extend_from_slice(&(envs.len() as u32).to_le_bytes());
        for (key, value) in envs {
            put_bytes(&mut payload, key.as_bytes());
            put_bytes(&mut payload, value.as_bytes());
        }
        put_bytes(&mut payload, stdin);
        let sent = self
            .stdin
            .lock()
            .map_err(|_| AgentError::Failed("the WSL agent lock failed".into()))
            .and_then(|mut input| write_frame(&mut input, REQUEST, id, &payload).map_err(|error| AgentError::Failed(error.to_string())));
        if let Err(error) = sent {
            self.pending.lock().ok().map(|mut pending| pending.remove(&id));
            return Err(error);
        }
        match rx.recv() {
            Ok(response) => Ok(response),
            Err(_) => Err(AgentError::Failed("the WSL agent stopped before answering".to_string())),
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_loop(agent: Weak<Agent>, mut stdout: std::process::ChildStdout, ready: mpsc::Sender<Result<(), String>>) {
    let mut ready = Some(ready);
    while let Some((kind, id, payload)) = read_frame(&mut stdout) {
        let Some(agent) = agent.upgrade() else { break };
        match kind {
            READY => {
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok(()));
                }
            }
            RESPONSE => {
                if let Some(response) = parse_response(&payload) {
                    if let Some(tx) = agent.pending.lock().ok().and_then(|mut pending| pending.remove(&id)) {
                        let _ = tx.send(response);
                    }
                }
            }
            CHANGE => {
                if let Some((paths, git_state)) = parse_change(&payload) {
                    (agent.watch)(paths, git_state);
                }
            }
            FULL => (agent.watch)(Vec::new(), true),
            _ => {}
        }
    }
    if let Some(agent) = agent.upgrade() {
        agent.alive.store(false, Ordering::SeqCst);
        if let Ok(mut pending) = agent.pending.lock() {
            pending.clear();
        }
    }
    if let Some(tx) = ready.take() {
        let _ = tx.send(Err("the WSL helper exited".to_string()));
    }
}

fn parse_response(payload: &[u8]) -> Option<Response> {
    let status = i32::from_le_bytes(payload.get(0..4)?.try_into().ok()?);
    let mut at = 4;
    let stdout = take_bytes(payload, &mut at)?.to_vec();
    let stderr = take_bytes(payload, &mut at)?.to_vec();
    Some(Response { status, stdout, stderr })
}

fn parse_change(payload: &[u8]) -> Option<(Vec<String>, bool)> {
    let git_state = *payload.first()? != 0;
    let count = u32::from_le_bytes(payload.get(1..5)?.try_into().ok()?);
    let mut at = 5;
    let mut paths = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let bytes = take_bytes(payload, &mut at)?;
        paths.push(String::from_utf8_lossy(bytes).into_owned());
    }
    Some((paths, git_state))
}

static AGENTS: Mutex<Vec<Weak<Agent>>> = Mutex::new(Vec::new());

fn register(agent: &Arc<Agent>) {
    if let Ok(mut agents) = AGENTS.lock() {
        agents.retain(|weak| weak.upgrade().is_some());
        agents.push(Arc::downgrade(agent));
    }
}

fn norm(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// The agent whose root contains `dir`, if one is running. Falls back to `wsl.exe` when None.
pub fn for_dir(dir: &Path) -> Option<Arc<Agent>> {
    let at = wsl::location(dir)?;
    let target = norm(dir);
    let agents = AGENTS.lock().ok()?;
    let mut best: Option<(usize, Arc<Agent>)> = None;
    for weak in agents.iter() {
        let Some(agent) = weak.upgrade() else { continue };
        if !agent.location.distro.eq_ignore_ascii_case(&at.distro) || !agent.is_alive() {
            continue;
        }
        let root = norm(agent.root());
        if target != root && !target.starts_with(&(root.clone() + "/")) {
            continue;
        }
        if best.as_ref().is_none_or(|(length, _)| *length < root.len()) {
            best = Some((root.len(), agent));
        }
    }
    best.map(|(_, agent)| agent)
}

