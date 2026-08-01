//! PTY runtime: spawns the child on a pseudo-TTY, runs a reader thread, and
//! exposes a non-blocking event stream + writer.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{anyhow, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const READ_BUF: usize = 4096;

/// ConPTY (via portable-pty) enables `PSEUDOCONSOLE_INHERIT_CURSOR`, which makes
/// the host emit a Device Status Report request (`ESC [ 6 n`) and **blocks the
/// child until the terminal answers** with a Cursor Position Report
/// (`ESC [ row ; col R`). Without an auto-reply, Windows embedded sessions hang
/// forever after spawn — ssh never even prints its version banner.
const DSR_REQUEST: &[u8] = b"\x1b[6n";
/// CPR reply: cursor at 1;1 is enough to unblock ConPTY; the real grid position
/// is tracked by our vt100 parser for display, not for this handshake.
const CPR_REPLY: &[u8] = b"\x1b[1;1R";

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
    /// Shared with the reader thread so it can auto-answer ConPTY DSR queries.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
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
        let writer = Arc::new(Mutex::new(
            pair.master.take_writer().context("take pty writer")?,
        ));
        let writer_for_reader = Arc::clone(&writer);

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
                // Carry incomplete ESC-sequences across reads so a split
                // `\x1b` / `[6n` still triggers a CPR reply.
                let mut dsr_window: Vec<u8> = Vec::with_capacity(8);
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            let _ = tx.send(PtyEvent::Exited("eof".into()));
                            break;
                        }
                        Ok(n) => {
                            let chunk = &buf[..n];
                            if maybe_answer_dsr(&mut dsr_window, chunk, &writer_for_reader) {
                                // CPR written; keep draining app output.
                            }
                            if tx.send(PtyEvent::Bytes(chunk.to_vec())).is_err() {
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
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| anyhow!("pty writer lock poisoned"))?;
        guard.write_all(bytes)?;
        guard.flush().ok();
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

/// Scan `chunk` (with a small carry-over window) for ConPTY DSR requests and
/// answer each with a CPR. Returns true if at least one reply was written.
fn maybe_answer_dsr(
    window: &mut Vec<u8>,
    chunk: &[u8],
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
) -> bool {
    window.extend_from_slice(chunk);
    let mut answered = false;
    while let Some(pos) = find_subslice(window, DSR_REQUEST) {
        if let Ok(mut w) = writer.lock() {
            if w.write_all(CPR_REPLY).is_ok() {
                let _ = w.flush();
                answered = true;
            }
        }
        // Drop the matched request so we don't re-answer it.
        let end = pos + DSR_REQUEST.len();
        window.drain(..end);
    }
    // Keep only a short tail — enough to match a DSR split across reads.
    const KEEP: usize = 3; // len(DSR_REQUEST) - 1
    if window.len() > KEEP {
        let drain = window.len() - KEEP;
        window.drain(..drain);
    }
    answered
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
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
        // Dropping the master/writer closes the ConPTY pipe so a blocked
        // reader.read() unblocks with EOF instead of hanging the UI thread.
        // portable-pty's reader does not always see child-exit alone on Windows.
        let _ = self.writer.lock().map(|mut w| {
            let _ = w.flush();
        });
        // Replace writer with a sink so further locks succeed briefly, then
        // drop the master handle by taking it out of the option-less field —
        // we can't easily drop master without restructuring; join with a
        // timeout-style detach instead: if the reader is still blocked after
        // kill, abandon the join so Drop cannot freeze the app.
        if let Some(handle) = self.reader_thread.take() {
            // Fast path: reader already finished.
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                // Last resort: spawn a reaper so we never block Drop. Leaking
                // the JoinHandle is intentional — better than a stuck quit.
                thread::spawn(move || {
                    let _ = handle.join();
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

    fn sink() -> (Arc<Mutex<Vec<u8>>>, SharedWriter) {
        let data = Arc::new(Mutex::new(Vec::new()));
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(Buf(Arc::clone(&data)))));
        (data, writer)
    }

    #[test]
    fn find_subslice_locates_dsr() {
        assert_eq!(find_subslice(b"abc\x1b[6ndef", DSR_REQUEST), Some(3));
        assert_eq!(find_subslice(b"nope", DSR_REQUEST), None);
    }

    #[test]
    fn answers_dsr_split_across_chunks() {
        let (data, writer) = sink();
        let mut window = Vec::new();
        // Split ESC [ 6 n across two reads.
        assert!(!maybe_answer_dsr(&mut window, b"\x1b[", &writer));
        assert!(data.lock().unwrap().is_empty());
        assert!(maybe_answer_dsr(&mut window, b"6n", &writer));
        assert_eq!(data.lock().unwrap().as_slice(), CPR_REPLY);
    }

    #[test]
    fn answers_multiple_dsr_in_one_chunk() {
        let (data, writer) = sink();
        let mut window = Vec::new();
        let mut chunk = Vec::new();
        chunk.extend_from_slice(DSR_REQUEST);
        chunk.extend_from_slice(b"noise");
        chunk.extend_from_slice(DSR_REQUEST);
        assert!(maybe_answer_dsr(&mut window, &chunk, &writer));
        let out = data.lock().unwrap().clone();
        assert_eq!(out, [CPR_REPLY, CPR_REPLY].concat());
    }
}
