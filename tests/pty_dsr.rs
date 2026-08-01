use sshub::session::pty::{PtyEvent, PtyRuntime};
use std::time::{Duration, Instant};

#[test]
fn ptyruntime_ssh_v_produces_banner() {
    let argv = vec!["ssh".into(), "-V".into()];
    let rt = match PtyRuntime::spawn(&argv, 24, 80, &[]) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("skip: cannot spawn ssh on PTY ({e:#})");
            return;
        }
    };
    let mut out = String::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(8) {
        while let Some(ev) = rt.try_recv() {
            match ev {
                PtyEvent::Bytes(b) => out.push_str(&String::from_utf8_lossy(&b)),
                PtyEvent::Stderr(b) => out.push_str(&String::from_utf8_lossy(&b)),
                PtyEvent::Exited(s) => {
                    eprintln!("exit={s} out={out:?}");
                    assert!(
                        out.contains("OpenSSH"),
                        "expected OpenSSH banner after DSR reply, got: {out:?}"
                    );
                    return;
                }
            }
        }
        if out.contains("OpenSSH") {
            eprintln!("got banner without exit yet: {out:?}");
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("timeout; out={out:?}");
}
