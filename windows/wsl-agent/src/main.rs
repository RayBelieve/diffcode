//! The Linux side of DiffCode's WSL support. One process per project:
//! - watches the work tree with inotify and reports changed paths;
//! - runs git requests sent by the Windows app over stdin/stdout, so a git call does not start `wsl.exe`.
//!
//! Frames are `[kind: u8][id: u32][length: u32]` followed by the payload, all little-endian.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};

const REQUEST: u8 = 1;
const RESPONSE: u8 = 2;
const CHANGE: u8 = 3;
const READY: u8 = 4;
const FULL: u8 = 5;

type Out = Sender<Vec<u8>>;

fn main() {
    let mut root = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => {
                println!("diffcode-wsl-agent 0.1.0");
                return;
            }
            "--watch" => root = args.next().map(PathBuf::from),
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(root) = root else {
        eprintln!("usage: diffcode-wsl-agent --watch <root>");
        std::process::exit(2);
    };
    let root = match root.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("cannot open {}: {error}", root.display());
            std::process::exit(1);
        }
    };

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut out = io::stdout().lock();
        while let Ok(frame) = rx.recv() {
            if out.write_all(&frame).and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    });

    let watches = setup_watches(&root, &tx);
    send_frame(&tx, READY, 0, b"diffcode-wsl-agent 0.1.0");

    let requests = tx.clone();
    std::thread::spawn(move || serve_stdin(requests));
    watch_loop(watches, root, tx);
}

fn send_frame(out: &Out, kind: u8, id: u32, payload: &[u8]) {
    let mut frame = Vec::with_capacity(9 + payload.len());
    frame.push(kind);
    frame.extend_from_slice(&id.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    let _ = out.send(frame);
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn read_frame(input: &mut impl Read) -> Option<(u8, u32, Vec<u8>)> {
    let mut head = [0u8; 9];
    input.read_exact(&mut head).ok()?;
    let kind = head[0];
    let id = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
    let len = u32::from_le_bytes([head[5], head[6], head[7], head[8]]);
    if len > 256 * 1024 * 1024 {
        return None;
    }
    let mut payload = vec![0u8; len as usize];
    input.read_exact(&mut payload).ok()?;
    Some((kind, id, payload))
}

fn take_u32(data: &[u8], at: &mut usize) -> Option<u32> {
    let bytes = data.get(*at..*at + 4)?;
    *at += 4;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn take_bytes<'a>(data: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    let len = take_u32(data, at)? as usize;
    let bytes = data.get(*at..*at + len)?;
    *at += len;
    Some(bytes)
}

fn take_string(data: &[u8], at: &mut usize) -> Option<String> {
    String::from_utf8(take_bytes(data, at)?.to_vec()).ok()
}

struct Reply {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn serve_stdin(out: Out) {
    let mut input = io::stdin().lock();
    while let Some((kind, id, payload)) = read_frame(&mut input) {
        if kind != REQUEST {
            continue;
        }
        let tx = out.clone();
        std::thread::spawn(move || {
            let reply = run_git(&payload);
            let mut body = Vec::new();
            body.extend_from_slice(&reply.status.to_le_bytes());
            put_bytes(&mut body, &reply.stdout);
            put_bytes(&mut body, &reply.stderr);
            send_frame(&tx, RESPONSE, id, &body);
        });
    }
    std::process::exit(0);
}

fn run_git(payload: &[u8]) -> Reply {
    let bad = |message: &str| Reply { status: -1, stdout: Vec::new(), stderr: message.as_bytes().to_vec() };
    let mut at = 0;
    let Some(cwd) = take_string(payload, &mut at) else { return bad("bad request: cwd") };
    let Some(argc) = take_u32(payload, &mut at) else { return bad("bad request: argc") };
    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        let Some(arg) = take_string(payload, &mut at) else { return bad("bad request: argument") };
        args.push(arg);
    }
    let Some(envc) = take_u32(payload, &mut at) else { return bad("bad request: envc") };
    let mut envs = Vec::with_capacity(envc as usize);
    for _ in 0..envc {
        let Some(key) = take_string(payload, &mut at) else { return bad("bad request: env key") };
        let Some(value) = take_string(payload, &mut at) else { return bad("bad request: env value") };
        envs.push((key, value));
    }
    let Some(input) = take_bytes(payload, &mut at) else { return bad("bad request: stdin") };

    let mut command = Command::new("git");
    command
        .args(&args)
        .current_dir(if cwd.is_empty() { "/" } else { &cwd })
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return bad(&format!("could not run git: {error}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    match child.wait_with_output() {
        Ok(output) => Reply {
            status: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        },
        Err(error) => bad(&format!("git failed: {error}")),
    }
}

struct Watches {
    fd: i32,
    map: HashMap<i32, PathBuf>,
}

fn setup_watches(root: &Path, out: &Out) -> Watches {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        eprintln!("inotify: {}", io::Error::last_os_error());
        std::process::exit(1);
    }
    let mut map = HashMap::new();
    if !add_tree(fd, root, &mut map) {
        let _ = send_frame(out, FULL, 0, b"cannot watch");
        std::process::exit(1);
    }
    Watches { fd, map }
}

fn skip(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some("objects")
        && path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some(".git")
}

fn add_tree(fd: i32, path: &Path, map: &mut HashMap<i32, PathBuf>) -> bool {
    if skip(path) {
        return true;
    }
    if add_watch(fd, path, map).is_none() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return true };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            add_tree(fd, &entry.path(), map);
        }
    }
    true
}

fn add_watch(fd: i32, path: &Path, map: &mut HashMap<i32, PathBuf>) -> Option<i32> {
    let mask = (libc::IN_MODIFY
        | libc::IN_ATTRIB
        | libc::IN_CLOSE_WRITE
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_ONLYDIR) as u32;
    let name = CString::new(path.as_os_str().as_bytes()).ok()?;
    let wd = unsafe { libc::inotify_add_watch(fd, name.as_ptr(), mask) };
    if wd < 0 {
        return None;
    }
    map.insert(wd, path.to_path_buf());
    Some(wd)
}

fn send_change(out: &Out, git_state: bool, paths: &[String]) {
    let mut body = Vec::new();
    body.push(git_state as u8);
    body.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    for path in paths {
        put_bytes(&mut body, path.as_bytes());
    }
    send_frame(out, CHANGE, 0, &body);
}

fn watch_loop(mut watches: Watches, root: PathBuf, out: Out) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = unsafe {
            libc::read(watches.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        let n = n as usize;
        let mut at = 0;
        let mut paths: Vec<String> = Vec::new();
        let mut git_state = false;
        let mut full = false;
        while at + std::mem::size_of::<libc::inotify_event>() <= n {
            let event = unsafe { &*(buf.as_ptr().add(at) as *const libc::inotify_event) };
            let wd = event.wd;
            let mask = event.mask;
            let name_len = event.len as usize;
            let name_at = at + std::mem::size_of::<libc::inotify_event>();
            at = name_at + name_len;
            let name = if name_len == 0 {
                String::new()
            } else {
                unsafe { CStr::from_ptr(buf.as_ptr().add(name_at) as *const libc::c_char) }
                    .to_string_lossy()
                    .into_owned()
            };
            if mask & libc::IN_Q_OVERFLOW != 0 {
                full = true;
                continue;
            }
            if mask & libc::IN_IGNORED != 0 {
                watches.map.remove(&wd);
                continue;
            }
            let Some(base) = watches.map.get(&wd).cloned() else { continue };
            let path = if name.is_empty() { base } else { base.join(&name) };
            let is_dir = mask & libc::IN_ISDIR != 0;
            if is_dir && mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                if !add_tree(watches.fd, &path, &mut watches.map) {
                    full = true;
                }
            }
            let relative = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if relative.is_empty() {
                if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
                    full = true;
                }
                continue;
            }
            if relative == ".git" || relative.starts_with(".git/") {
                if relative.starts_with(".git/objects/") {
                    continue;
                }
                git_state = true;
            } else {
                paths.push(relative);
            }
        }
        if full {
            send_frame(&out, FULL, 0, b"overflow");
        }
        if git_state || !paths.is_empty() {
            send_change(&out, git_state, &paths);
        }
    }
}

