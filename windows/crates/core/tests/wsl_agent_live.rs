//! Against a real WSL distro and the built helper. Ignored by default. On Windows:
//!
//! ```text
//! set DIFFCODE_WSL_SHARE=\\wsl.localhost\Ubuntu\home\me\scratch
//! set DIFFCODE_WSL_AGENT=D:\path\to\diffcode-wsl-agent
//! cargo test -p diffcode-core --test wsl_agent_live -- --ignored --nocapture
//! ```
#![cfg(windows)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use diffcode_core::commit::{self, ChangeLine, PartialFile};
use diffcode_core::{git, wsl_agent};

fn share() -> PathBuf {
    PathBuf::from(std::env::var("DIFFCODE_WSL_SHARE").expect("set DIFFCODE_WSL_SHARE"))
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = share().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore]
fn the_agent_watches_and_runs_git() {
    let scratch = Scratch::new("agent");
    let dir = scratch.0.clone();
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git::run(&dir, ["init", "-q", "-b", "main"]).unwrap();

    let (tx, rx) = mpsc::channel();
    let agent = wsl_agent::Agent::start(&dir, move |paths, git_state| {
        let _ = tx.send((paths, git_state));
    })
    .expect("start the WSL agent");
    let status = git::run_string(&dir, ["status", "--porcelain=v2", "--branch"]).unwrap();
    assert!(status.contains("branch.head main"), "{status}");

    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    let (paths, _) = rx.recv_timeout(Duration::from_secs(8)).expect("no inotify event");
    assert!(paths.iter().any(|path| path == "b.txt"), "{paths:?}");
    drop(agent);
}

#[test]
#[ignore]
fn the_agent_runs_a_partial_commit() {
    let scratch = Scratch::new("agent-part");
    let dir = scratch.0.clone();
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git::run(&dir, ["init", "-q", "-b", "main"]).unwrap();
    let (tx, _rx) = mpsc::channel();
    let agent = wsl_agent::Agent::start(&dir, move |paths, git_state| {
        let _ = tx.send((paths, git_state));
    })
    .expect("start the WSL agent");

    let status = git::status(&dir, None).unwrap();
    commit::commit("First", &status.changes, false, &dir).unwrap();
    std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let status = git::status(&dir, None).unwrap();
    let change = status.changes.iter().find(|c| c.path == "a.txt").cloned().expect("a.txt changed");
    let excluded = HashSet::from([ChangeLine { removed: false, line: 3, text: "three".into() }]);
    commit::commit_partial("Part", &[], &[PartialFile { change, excluded }], false, &dir).unwrap();
    assert_eq!(git::blob("HEAD", "a.txt", &dir).as_deref(), Some("one\ntwo\n".as_bytes()));
    drop(agent);
}
