//! Carries on in a rebuilt Kanzo. When `cargo install`, `cargo build` or a package manager
//! replaces the program file, the board and every `kanzo mcp` restart into it in the same
//! process: the board keeps its port, and `kanzo mcp` keeps its agent's session.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::Notify;

/// Where a restarted `kanzo mcp` finds the session it carries on.
const RESUME_ENV: &str = "KANZO_MCP_RESUME";
/// The replayed `initialize` gets this id, so its answer, already given once, isn't sent again.
const RESUME_ID: &str = "kanzo-resume";
const INITIALIZED: &[u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
/// The new program may have other tools; this tells the agent to fetch them again.
const TOOLS_CHANGED: &[u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n";
/// How long a restart waits for the answers to what the agent already asked.
const DRAIN: Duration = Duration::from_secs(30);

/// A replaced program must stay unchanged this long before it runs: a copy may still be writing it.
const SETTLE: Duration = Duration::from_secs(1);

/// Only what the relay needs to know of a JSON-RPC message.
#[derive(Deserialize)]
struct Message {
    id: Option<serde_json::Value>,
    method: Option<String>,
}

fn message(line: &[u8]) -> Option<Message> {
    serde_json::from_slice(line).ok()
}

#[derive(Serialize, Deserialize)]
struct Resume {
    /// The agent's `initialize` request, replayed so the new program knows the session.
    initialize: String,
    /// What the agent sent that the old program read but didn't handle.
    unread: String,
}

/// Requests from the agent that have no answer yet.
#[derive(Default)]
struct Pending {
    ids: Mutex<HashSet<String>>,
    done: Notify,
}

impl Pending {
    fn asked(&self, id: String) {
        self.ids.lock().unwrap().insert(id);
    }

    fn answered(&self, id: &str) {
        let mut ids = self.ids.lock().unwrap();
        if ids.remove(id) && ids.is_empty() {
            self.done.notify_waiters();
        }
    }

    async fn drained(&self) {
        loop {
            let done = self.done.notified();
            tokio::pin!(done);
            done.as_mut().enable();
            if self.ids.lock().unwrap().is_empty() {
                return;
            }
            done.await;
        }
    }
}

/// The MCP server's stdio. Between the agent's messages, a rebuilt program takes over the
/// process, picking up the session: the agent never notices, but for new tools.
pub fn stdio() -> (DuplexStream, DuplexStream) {
    let (stdin, stdout) = rmcp::transport::stdio();
    let (to_server, server_in) = tokio::io::duplex(1 << 16);
    let (server_out, from_server) = tokio::io::duplex(1 << 16);
    let pending = Arc::new(Pending::default());
    tokio::spawn(relay_in(stdin, to_server, pending.clone()));
    tokio::spawn(relay_out(from_server, stdout, pending));
    (server_in, server_out)
}

async fn forward(server: &mut DuplexStream, pending: &Pending, line: &[u8]) -> io::Result<()> {
    if let Some(Message { id: Some(id), method: Some(_) }) = message(line) {
        pending.asked(id.to_string());
    }
    server.write_all(line).await
}

async fn relay_in(stdin: impl AsyncRead + Unpin, mut server: DuplexStream, pending: Arc<Pending>) -> io::Result<()> {
    let mut stdin = BufReader::new(stdin);
    let mut watch = Watch::new();
    let mut initialize = None;
    let mut line = Vec::new();
    if let Some(resume) = take_resume() {
        let mut replay: serde_json::Value = serde_json::from_str(&resume.initialize)?;
        replay["id"] = RESUME_ID.into();
        server.write_all(format!("{replay}\n").as_bytes()).await?;
        server.write_all(INITIALIZED).await?;
        for unread in resume.unread.split_inclusive('\n') {
            if unread.ends_with('\n') {
                forward(&mut server, &pending, unread.as_bytes()).await?;
            } else {
                // The rest of this one is still on stdin.
                line.extend_from_slice(unread.as_bytes());
            }
        }
        initialize = Some(resume.initialize);
    }
    loop {
        if stdin.read_until(b'\n', &mut line).await? == 0 && line.is_empty() {
            return Ok(());
        }
        match &initialize {
            None if message(&line).is_some_and(|m| m.method.as_deref() == Some("initialize")) => {
                initialize = Some(String::from_utf8_lossy(&line).trim_end().to_string());
            }
            Some(init) => {
                if let Some(exe) = watch.as_mut().and_then(Watch::ready) {
                    // The answers to what the agent asked come from this program, before it goes.
                    let _ = tokio::time::timeout(DRAIN, pending.drained()).await;
                    let mut unread = line.clone();
                    unread.extend_from_slice(stdin.buffer());
                    let error = restart(&exe, init, &unread);
                    eprintln!("kanzo: cannot restart into {}: {error}", exe.display());
                    watch = None;
                }
            }
            None => {}
        }
        forward(&mut server, &pending, &line).await?;
        line.clear();
    }
}

async fn relay_out(
    from_server: DuplexStream,
    mut stdout: impl AsyncWrite + Unpin,
    pending: Arc<Pending>,
) -> io::Result<()> {
    let mut lines = BufReader::new(from_server);
    let mut line = Vec::new();
    let resumed = serde_json::Value::from(RESUME_ID).to_string();
    while lines.read_until(b'\n', &mut line).await? > 0 {
        let answer = message(&line).filter(|m| m.method.is_none()).and_then(|m| m.id).map(|id| id.to_string());
        stdout.write_all(if answer.as_ref() == Some(&resumed) { TOOLS_CHANGED } else { &line }).await?;
        stdout.flush().await?;
        if let Some(id) = answer {
            pending.answered(&id);
        }
        line.clear();
    }
    Ok(())
}

fn take_resume() -> Option<Resume> {
    let path = std::env::var_os(RESUME_ENV)?;
    let text = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&text.ok()?).ok()
}

/// Runs the new program in this process, with the session in a file only this user can read:
/// the agent's messages hold its token. Returns only when that fails.
#[cfg(unix)]
fn restart(exe: &Path, initialize: &str, unread: &[u8]) -> io::Error {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    let path = std::env::temp_dir().join(format!("kanzo-mcp-{}.json", std::process::id()));
    let resume = Resume { initialize: initialize.into(), unread: String::from_utf8_lossy(unread).into() };
    let _ = std::fs::remove_file(&path);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(serde_json::to_string(&resume)?.as_bytes()));
    if let Err(e) = written {
        return e;
    }
    let error = Command::new(exe).args(std::env::args_os().skip(1)).env(RESUME_ENV, &path).exec();
    let _ = std::fs::remove_file(&path);
    error
}

#[cfg(not(unix))]
fn restart(_exe: &Path, _initialize: &str, _unread: &[u8]) -> io::Error {
    io::Error::other("a program can't replace itself here")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    modified: SystemTime,
    len: u64,
    /// A program replaced by renaming another file over it keeps its size and maybe its time.
    #[cfg(unix)]
    inode: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: m.modified().ok()?,
        len: m.len(),
        #[cfg(unix)]
        inode: std::os::unix::fs::MetadataExt::ino(&m),
    })
}

/// This build, told apart from the next one so open boards reload when the board restarts into
/// it. `main` reads it first thing, before the file can be replaced.
pub static BUILD: LazyLock<String> = LazyLock::new(|| {
    let stamp = std::env::current_exe().ok().and_then(|exe| stamp(&exe));
    let nanos = stamp.and_then(|s| s.modified.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    format!("{}-{nanos:x}-{:x}", crate::update::CURRENT, stamp.map_or(0, |s| s.len))
});

/// Watches the program file this process was started from.
pub struct Watch {
    exe: PathBuf,
    running: Option<Stamp>,
}

impl Watch {
    /// Only where a process can replace itself; Windows also locks a running program's file.
    pub fn new() -> Option<Self> {
        if !cfg!(unix) {
            return None;
        }
        Self::of(std::env::current_exe().ok()?)
    }

    fn of(exe: PathBuf) -> Option<Self> {
        let running = Some(stamp(&exe)?);
        Some(Self { exe, running })
    }

    /// The program to restart into: a new one nothing has written to for a moment, and that runs.
    pub fn ready(&mut self) -> Option<PathBuf> {
        let now = stamp(&self.exe)?;
        if Some(now) == self.running || now.modified.elapsed().is_ok_and(|age| age < SETTLE) {
            return None;
        }
        // A half-written or broken program would leave nothing running.
        let runs = Command::new(&self.exe)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if !runs {
            eprintln!("kanzo: {} changed but doesn't run; staying on this version", self.exe.display());
            self.running = Some(now);
            return None;
        }
        Some(self.exe.clone())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Like `cargo install`: the new file is renamed over the old one. `age` back-dates it.
    fn program(path: &Path, script: &str, age: Duration) {
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, script).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = std::fs::File::options().write(true).open(&tmp).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[test]
    fn restarts_only_into_a_settled_program_that_runs() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("kanzo");
        let ok = "#!/bin/sh\nexit 0\n";
        program(&exe, ok, SETTLE);
        let mut watch = Watch::of(exe.clone()).unwrap();
        assert_eq!(watch.ready(), None, "nothing changed");

        program(&exe, ok, Duration::ZERO);
        assert_eq!(watch.ready(), None, "just written: it may not be finished");
        program(&exe, ok, SETTLE);
        assert_eq!(watch.ready(), Some(exe.clone()));

        program(&exe, "#!/bin/sh\nexit 1\n", SETTLE);
        assert_eq!(watch.ready(), None, "a broken program");
        program(&exe, ok, SETTLE * 2);
        assert_eq!(watch.ready(), Some(exe.clone()), "then a fixed one");
    }
}
