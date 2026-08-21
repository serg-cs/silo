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
pub(super) struct SavedTerminal(libc::termios);

impl SavedTerminal {
    /// Captures stdin's state, or returns `None` when stdin is not a terminal.
    pub(super) fn capture() -> Option<Self> {
        let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, attrs.as_mut_ptr()) } == 0 {
            Some(Self(unsafe { attrs.assume_init() }))
        } else {
            None
        }
    }

    /// Restores the captured terminal state on a best-effort basis.
    pub(super) fn restore(&self) {
        // Reapplying the state after a normal child exit is harmless.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.0) };
    }
}
