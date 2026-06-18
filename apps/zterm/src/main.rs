//! zterm — ZenOS's native terminal, a Wayland client.
//!
//! Linux-only (PTY + Wayland). Milestone layout:
//!   M0/M1 (done): pty + mono font modules, plus this headless smoke main that
//!     proves pty → vt100 → screen wiring without a window.
//!   M2+: replace `main` with the sctk window + shm render loop, drawing the
//!     vt100 grid with `font::Font` and forwarding keys back to the pty.

mod font;
mod pty;

use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Font: confirm a monospace face loads and report the cell box.
    let font = font::Font::load(16.0).ok_or("no monospace font found (set $ZTERM_FONT)")?;
    eprintln!("zterm: cell {}x{} px", font.cell_w, font.cell_h);

    // PTY: spawn the shell, run one command, feed output through vt100, dump the
    // resulting screen. This is the smoke test for M1.
    let (rows, cols) = (24u16, 80u16);
    let pty = pty::Pty::spawn(rows, cols)?;
    let mut reader = pty.reader()?;
    let mut writer = pty.writer()?;
    let mut parser = vt100::Parser::new(rows, cols, 0);

    writer.write_all(b"echo hello from zterm; exit\n")?;
    drop(writer);

    let mut buf = [0u8; 4096];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        match reader.read(&mut buf) {
            Ok(0) => break, // shell exited → EOF
            Ok(n) => parser.process(&buf[..n]),
            Err(_) => break,
        }
    }

    print!("{}", parser.screen().contents());
    Ok(())
}
