//! PTY runtime: spawns the child on a pseudo-TTY, runs a reader thread, and
//! exposes a non-blocking event stream + writer.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const READ_BUF: usize = 4096;

/// Event from the PTY reader thread to the main thread.
#[derive(Debug)]
pub enum PtyEvent {
    /// Bytes read from the master side of the PTY (stdout / the live shell).
    Bytes(Vec<u8>),
    /// Bytes read from ssh's stderr, routed through a side FIFO so the verbose
    /// `-v` handshake never pollutes the terminal grid.
    /// On Windows there is no FIFO siphon: stderr is merged into the PTY and
    /// this variant is unused.
    Stderr(Vec<u8>),
    /// Child exited; carries a human-readable status string.
    Exited(String),
}

// --- Unix stderr FIFO siphon -------------------------------------------------
// Routes ssh `-v` noise off the PTY grid via mkfifo + `/bin/sh` redirect.
// Windows has no equivalent that portable-pty exposes cleanly, so the
// Windows build always falls back to merged stderr (same as Unix when
// `StderrFifo::create` fails).

#[cfg(unix)]
mod stderr_fifo {
    use std::ffi::CString;
    use std::fs::{File, OpenOptions};
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use anyhow::{anyhow, Context, Result};

    use super::{PtyEvent, READ_BUF};

    /// Env var carrying the stderr FIFO path into the `sh` wrapper.
    pub const STDERR_FIFO_ENV: &str = "SSHUB_STDERR_FIFO";

    static FIFO_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A named FIFO used to siphon the child's stderr away from the PTY.
    pub struct StderrFifo {
        pub path: PathBuf,
        read: File,
    }

    impl StderrFifo {
        pub fn create() -> Result<Self> {
            let mut path = std::env::temp_dir();
            let seq = FIFO_SEQ.fetch_add(1, Ordering::Relaxed);
            path.push(format!("sshub-stderr-{}-{seq}.fifo", std::process::id()));
            let _ = std::fs::remove_file(&path);

            let c_path = CString::new(path.as_os_str().as_bytes()).context("fifo path nul")?;
            // SAFETY: c_path is a valid NUL-terminated C string.
            let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
            if rc != 0 {
                return Err(anyhow!(
                    "mkfifo({}) failed: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }

            // O_RDWR keeps a writer attached so empty FIFO yields EAGAIN, not EOF.
            let read = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
                .with_context(|| format!("open fifo {}", path.display()))?;

            Ok(Self { path, read })
        }

        pub fn spawn_reader(
            &self,
            tx: Sender<PtyEvent>,
            stop: Arc<AtomicBool>,
        ) -> Option<JoinHandle<()>> {
            let mut read = self.read.try_clone().ok()?;
            thread::Builder::new()
                .name("sshub-stderr-reader".into())
                .spawn(move || {
                    let mut buf = [0u8; READ_BUF];
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        match read.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                if tx.send(PtyEvent::Stderr(buf[..n].to_vec())).is_err() {
                                    break;
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(20));
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                })
                .ok()
        }
    }

    impl Drop for StderrFifo {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct PtyRuntime {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: Receiver<PtyEvent>,
    /// Set when the reader has signalled EOF / child exit. Used so we don't
    /// keep spinning on a dead PTY.
    closed: Arc<AtomicBool>,
    reader_thread: Option<JoinHandle<()>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    /// Set on drop to stop the stderr reader promptly even if the child never
    /// opened the FIFO write end.
    stderr_stop: Arc<AtomicBool>,
    stderr_reader: Option<JoinHandle<()>>,
    /// Kept alive so the FIFO is unlinked when the runtime drops (Unix only).
    #[cfg(unix)]
    _stderr_fifo: Option<stderr_fifo::StderrFifo>,
}

impl PtyRuntime {
    pub fn spawn(argv: &[String], rows: u16, cols: u16, env: &[(String, String)]) -> Result<Self> {
        if argv.is_empty() {
            return Err(anyhow!("empty argv"));
        }

        // Unix: route stderr through a side FIFO so ssh `-v` never lands on the
        // PTY grid. Falls back to merged stderr if the FIFO can't be set up.
        // Windows: always spawn the real command directly (merged stderr).
        #[cfg(unix)]
        let stderr_fifo = stderr_fifo::StderrFifo::create().ok();

        #[cfg(unix)]
        let (program, prog_args): (String, Vec<String>) = if stderr_fifo.is_some() {
            let mut args = vec![
                "-c".to_string(),
                format!("exec \"$@\" 2>\"${}\"", stderr_fifo::STDERR_FIFO_ENV),
                "sshub".to_string(),
            ];
            args.extend(argv.iter().cloned());
            ("/bin/sh".to_string(), args)
        } else {
            (argv[0].clone(), argv[1..].to_vec())
        };
        #[cfg(not(unix))]
        let (program, prog_args): (String, Vec<String>) = (argv[0].clone(), argv[1..].to_vec());

        let mut cmd = CommandBuilder::new(&program);
        for arg in &prog_args {
            cmd.arg(arg);
        }
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        if let Some(fifo) = &stderr_fifo {
            cmd.env(stderr_fifo::STDERR_FIFO_ENV, fifo.path.as_os_str());
        }
        // Override TERM. Our vt100 emulator is xterm-compatible; advertising
        // `xterm-kitty` (often inherited from the user's host kitty session)
        // leaves the remote without a matching terminfo entry — breaking
        // `clear`, `tput`, ncurses apps, etc. Force a portable default.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn child on pty slave")?;
        // Slave is no longer needed in this process.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;

        let (tx, rx) = mpsc::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_thread = Arc::clone(&closed);
        let stderr_stop = Arc::new(AtomicBool::new(false));

        // Clone the channel for the Unix stderr siphon *before* moving `tx`
        // into the main PTY reader thread.
        #[cfg(unix)]
        let stderr_reader = stderr_fifo
            .as_ref()
            .and_then(|fifo| fifo.spawn_reader(tx.clone(), Arc::clone(&stderr_stop)));
        #[cfg(not(unix))]
        let stderr_reader = None;

        let reader_thread = thread::Builder::new()
            .name("sshub-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; READ_BUF];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            let _ = tx.send(PtyEvent::Exited("eof".into()));
                            break;
                        }
                        Ok(n) => {
                            if tx.send(PtyEvent::Bytes(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(PtyEvent::Exited(format!("read error: {e}")));
                            break;
                        }
                    }
                }
                closed_thread.store(true, Ordering::Relaxed);
            })
            .context("spawn pty reader thread")?;

        Ok(Self {
            master: pair.master,
            writer,
            rx,
            closed,
            reader_thread: Some(reader_thread),
            child: Some(child),
            stderr_stop,
            stderr_reader,
            #[cfg(unix)]
            _stderr_fifo: stderr_fifo,
        })
    }

    /// Non-blocking poll for one event.
    pub fn try_recv(&self) -> Option<PtyEvent> {
        self.rx.try_recv().ok()
    }

    /// Write bytes to the master side. Called for each forwarded keystroke.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush().ok();
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")?;
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Reap a child that has already exited. Prevents zombies while the
    /// [`Session`] object stays alive in a detached tab.
    pub fn reap_child(&mut self) -> Option<portable_pty::ExitStatus> {
        if let Some(mut child) = self.child.take() {
            child.wait().ok()
        } else {
            None
        }
    }

    fn terminate_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate_child_process(&mut *child);
        }
    }
}

/// Kill the embedded ssh child and its process group, then reap it.
fn terminate_child_process(child: &mut dyn portable_pty::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.process_id() {
        use std::time::Duration;
        let pgid = pid as libc::pid_t;
        // portable-pty calls setsid() in the slave pre_exec, so `-pid` hits the
        // whole session (ssh and any local helpers).
        unsafe {
            libc::kill(-pgid, libc::SIGHUP);
        }
        std::thread::sleep(Duration::from_millis(50));
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for PtyRuntime {
    fn drop(&mut self) {
        self.terminate_child();
        self.stderr_stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.stderr_reader.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}
