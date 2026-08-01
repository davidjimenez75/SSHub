# Building SSHub on Windows

SSHub is primarily developed for Linux and macOS. Windows is supported via
conditional compilation: the TUI, embedded SSH sessions (ConPTY), SFTP, and
most CLI features work natively. A few Unix-only details (stderr FIFO siphon,
local `chmod` modes, process-group signals for tunnels) are degraded or
stubbed on Windows.

Prebuilt release binaries are **not** currently published for Windows. Build
from source as below.

## Prerequisites

| Tool | Why |
|------|-----|
| [Rust](https://rustup.rs/) (MSVC toolchain) | Compiler |
| [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with “Desktop development with C++” | MSVC linker and C toolchain for native crates |
| [Strawberry Perl](https://strawberryperl.com/) (or any Perl on `PATH`) | Required to compile the **vendored OpenSSL** used by `ssh2` |
| [Git](https://git-scm.com/download/win) | Clone the repository |
| [OpenSSH Client](https://learn.microsoft.com/windows-server/administration/openssh/openssh_install_firstuse) | Runtime: SSHub spawns `ssh.exe` for host sessions |

### Install OpenSSH Client (runtime)

**Settings → Apps → Optional features → Add a feature → OpenSSH Client**,  
or from an elevated PowerShell:

```powershell
Add-WindowsCapability -Online -Name OpenSSH.Client~~~~0.0.1.0
```

Confirm:

```powershell
ssh -V
```

### Install Strawberry Perl (build)

Either:

- Installer: https://strawberryperl.com/  
- winget: `winget install --id StrawberryPerl.StrawberryPerl -e`  
- Portable: extract the portable zip and add `perl\bin` (and optionally `c\bin`) to `PATH` for the build session.

Verify:

```powershell
perl -v
```

If Perl was installed after the terminal was opened, restart the terminal so `PATH` updates.

## Build

```powershell
git clone https://github.com/<your-fork-or-upstream>/SSHub.git
cd SSHub

# Ensure Perl is visible in this shell, e.g.:
# $env:Path = "C:\Strawberry\perl\bin;C:\Strawberry\c\bin;" + $env:Path

cargo build --release
```

The first build compiles OpenSSL from source and can take **10–20+ minutes**.
Later builds are much faster.

Binary output:

```text
target\release\sshub.exe
```

Debug build (faster compile, larger binary):

```powershell
cargo build
# → target\debug\sshub.exe
```

Install into Cargo’s bin directory (`%USERPROFILE%\.cargo\bin`):

```powershell
cargo install --path .
```

## Run

```powershell
.\target\release\sshub.exe --version
.\target\release\sshub.exe --help
.\target\release\sshub.exe
```

The TUI requires an interactive terminal (Windows Terminal, ConHost, etc.).

### Single-file copy

`sshub.exe` is **self-contained** for app libraries:

- OpenSSL is **vendored** (statically linked)
- SQLite is **bundled**
- No project DLLs need to sit next to the executable

You can copy only the exe, for example:

```powershell
Copy-Item .\target\release\sshub.exe C:\ulb\sshub.exe
```

It still depends on:

- Normal Windows system libraries (`kernel32`, `ws2_32`, `crypt32`, …)
- The **Visual C++ runtime** (`VCRUNTIME140.dll` / Universal CRT) — usually already present; install the [VC++ Redistributable (x64)](https://learn.microsoft.com/cpp/windows/latest-supported-vc-redist) if the OS reports a missing runtime DLL
- **`ssh.exe`** on `PATH` for SSH sessions

### Config and data directories

Without overrides:

| Purpose | Default on Windows |
|---------|-------------------|
| Config | `%APPDATA%\sshub` |
| Data / SQLite | `%LOCALAPPDATA%\sshub` |

Override with `SSHUB_CONFIG_DIR` and `SSHUB_DATA_DIR` (same as on Unix).

`HOME` is not required; if needed, SSHub falls back to `USERPROFILE`.

## Troubleshooting

### `Command 'perl' not found` / OpenSSL configure failed

Perl is missing from `PATH`. Install Strawberry Perl and reopen the shell, or prepend its `bin` directory for the session, then rebuild:

```powershell
cargo clean -p openssl-sys
cargo build --release
```

### `std::os::unix` / `mkfifo` / `localtime_r` errors

You are on an older tree without the Windows port. Build from a branch that includes the Windows `cfg` changes (or upstream once merged).

### TUI starts but connect fails with `Command not found: 'ssh'`

Older Windows builds preflighted with Unix `which`, which is not on Windows PATH,
so SSHub reported `ssh` missing even when OpenSSH was installed. Current trees
resolve executables by walking `PATH` + `PATHEXT` instead.

Still check:

- `where.exe ssh` finds `ssh.exe` (usually `C:\Windows\System32\OpenSSH\ssh.exe`)
- Test the same host with `ssh user@host` outside SSHub
- Host keys / agent: Windows OpenSSH uses `%USERPROFILE%\.ssh` and the Windows OpenSSH Authentication Agent when enabled
- If you only added OpenSSH to PATH in the current shell, restart SSHub from that same shell (or set PATH system-wide and open a new terminal)

### TUI connects hang on “connecting…” (CLI `sshub host connect` works)

Windows ConPTY (via `portable-pty`) emits a cursor-position query (`ESC [ 6 n`)
and **blocks the child until the terminal answers**. SSHub auto-replies with a
Cursor Position Report. If you are on a build from before that fix, sessions
spawn but never show the SSH banner/prompt even though the same host works in
PowerShell or `sshub host connect`.

### Link / MSVC errors

Install the MSVC C++ build tools and use the default host triple:

```powershell
rustup show
# expect: x86_64-pc-windows-msvc (or aarch64-pc-windows-msvc)
```

### First OpenSSL build is very slow

Expected. Vendored OpenSSL is compiled with `nmake`. Keep the machine awake until the build finishes; subsequent builds reuse artifacts.

## Platform differences (summary)

| Feature | Windows behavior |
|---------|------------------|
| Embedded PTY | ConPTY via `portable-pty` |
| SSH verbose stderr split | Not available (stderr merged into the PTY stream) |
| Local SFTP pane `chmod` | Not supported (remote `chmod` still works) |
| Ping latency | Uses `ping -n 1 -w 1000` |
| Keyring | Windows Credential Manager (`keyring` `windows-native`) |
| Tunnel PID stop | Limited / may report unsupported for some paths |

## Related docs

- General install and usage: [README.md](README.md)
- Contributing: [CONTRIBUTING.md](CONTRIBUTING.md)
