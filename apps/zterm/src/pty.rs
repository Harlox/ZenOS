//! PTY: spawn the user's shell on a pseudo-terminal and expose read/write/resize.
//! Wraps `portable-pty` so the rest of zterm never touches platform pty details.

use std::error::Error;
use std::io::{Read, Write};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// A live shell on a pty. Hold it for the session; drop kills the master and the
/// child sees EOF.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    #[allow(dead_code)]
    child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    /// Open a pty sized `rows`x`cols` and spawn `$SHELL` (fallback `/bin/sh`) in it.
    pub fn spawn(rows: u16, cols: u16) -> Result<Self, Box<dyn Error>> {
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd)?;
        // Drop the slave handle: only the child should hold it, so closing the
        // shell propagates EOF on the master instead of hanging.
        drop(pair.slave);
        Ok(Pty {
            master: pair.master,
            child,
        })
    }

    /// A clonable reader over the master — read shell output here.
    pub fn reader(&self) -> Result<Box<dyn Read + Send>, Box<dyn Error>> {
        Ok(self.master.try_clone_reader()?)
    }

    /// The writer to the master — send keystrokes here. Call once.
    pub fn writer(&self) -> Result<Box<dyn Write + Send>, Box<dyn Error>> {
        Ok(self.master.take_writer()?)
    }

    /// Tell the kernel the new grid size so the shell reflows (SIGWINCH).
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}
