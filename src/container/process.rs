//! Host process and terminal handling for interactive container sessions.

use std::io;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};

/// Signal received by Silo while the container runs. `0` means none.
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Records a signal for the main loop using only async-signal-safe work.
extern "C" fn record_signal(signal: libc::c_int) {
    PENDING_SIGNAL.store(signal, Ordering::Relaxed);
}

/// Installs handlers so Silo can clean up and restore the terminal after its
/// container child exits.
pub(super) fn install_signal_handlers() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // `sighandler_t` is an integer type on macOS, hence the two-step cast.
        if unsafe { libc::signal(signal, record_signal as *const () as libc::sighandler_t) }
            == libc::SIG_ERR
        {
            return Err(anyhow!(
                "failed to install handler for signal {signal}: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// Waits for the child and forwards signals received by Silo.
pub(super) fn wait_for_child(child: &mut Child) -> Result<ExitStatus> {
    let pid = libc::pid_t::try_from(child.id()).map_err(|_| {
        // Avoid leaving an untracked child running if the platform ever
        // exposes a process identifier outside `pid_t`'s range.
        let _ = child.kill();
        let _ = child.wait();
        anyhow!("container child process ID does not fit the platform pid type")
    })?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let signal = PENDING_SIGNAL.swap(0, Ordering::Relaxed);
        if signal != 0 {
            // The child may exit after `try_wait`; ESRCH is harmless here.
            unsafe { libc::kill(pid, signal) };
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Terminal state captured before an interactive container child starts.
///
/// `O_NONBLOCK` lives on the open file description and is outside termios.
pub(super) struct SavedTerminal {
    termios: libc::termios,
    stdio_flags: [Option<libc::c_int>; 3],
}

impl SavedTerminal {
    /// Captures stdin's terminal state, or returns `None` when stdin is not a terminal.
    pub(super) fn capture() -> Option<Self> {
        let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, attrs.as_mut_ptr()) } != 0 {
            return None;
        }
        Some(Self {
            termios: unsafe { attrs.assume_init() },
            stdio_flags: [
                stdio_flags(libc::STDIN_FILENO),
                stdio_flags(libc::STDOUT_FILENO),
                stdio_flags(libc::STDERR_FILENO),
            ],
        })
    }

    /// Restores the captured terminal state on a best-effort basis.
    pub(super) fn restore(&self) {
        // Reapplying the state after a normal child exit is harmless.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.termios) };
        for (fd, flags) in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO]
            .into_iter()
            .zip(self.stdio_flags)
        {
            if let Some(flags) = flags {
                restore_stdio_flags(fd, flags);
            }
        }
    }
}

fn stdio_flags(fd: libc::c_int) -> Option<libc::c_int> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    (flags != -1).then_some(flags)
}

fn restore_stdio_flags(fd: libc::c_int, flags: libc::c_int) {
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
}

#[cfg(test)]
mod tests {
    use super::{restore_stdio_flags, stdio_flags};

    fn pipe_fds() -> [libc::c_int; 2] {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        fds
    }

    fn close(fd: libc::c_int) {
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }

    #[test]
    fn restore_clears_nonblocking_stdio_flags() {
        let [read, write] = pipe_fds();
        let original = stdio_flags(read).expect("pipe flags are readable");
        assert_eq!(
            original & libc::O_NONBLOCK,
            0,
            "pipes start in blocking mode"
        );

        assert_eq!(
            unsafe { libc::fcntl(read, libc::F_SETFL, original | libc::O_NONBLOCK) },
            0
        );
        assert_ne!(
            stdio_flags(read).expect("pipe flags are readable") & libc::O_NONBLOCK,
            0
        );

        restore_stdio_flags(read, original);
        assert_eq!(
            stdio_flags(read).expect("pipe flags are readable") & libc::O_NONBLOCK,
            0
        );

        close(read);
        close(write);
    }
}
