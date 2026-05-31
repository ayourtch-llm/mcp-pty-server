use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type SessionId = String;

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("max sessions reached: {0}")]
    MaxSessions(usize),
    #[error("session already exited")]
    Exited,
    #[error("pty spawn error: {0}")]
    Spawn(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, PtyError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Exited(i32),
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMetadata {
    pub id: SessionId,
    pub command: String,
    pub status: SessionStatus,
    pub cols: u16,
    pub rows: u16,
}

/// Internal state protected by a mutex — owned by both the reader thread
/// and the foreground tool handlers.
struct SharedState {
    parser: vt100::Parser,
    status: SessionStatus,
    last_output: Instant,
}

impl SharedState {
    fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            status: SessionStatus::Running,
            last_output: Instant::now(),
        }
    }
}

pub struct PtySession {
    pub id: SessionId,
    command: String,
    state: Arc<Mutex<SharedState>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    cols: u16,
    rows: u16,
}

impl PtySession {
    /// Spawn a new PTY running `command args...` with the given dimensions.
    pub fn spawn(
        id: SessionId,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let mut cmd = CommandBuilder::new(command);
        for arg in args {
            cmd.arg(arg);
        }
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        } else if let Ok(current) = std::env::current_dir() {
            cmd.cwd(current);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let state = Arc::new(Mutex::new(SharedState::new(cols, rows, 5000)));
        let child = Arc::new(Mutex::new(child));

        // Reader thread: blocking reads on the PTY master, feed bytes into
        // the vt100 parser. Exits when the child closes the PTY (EOF).
        {
            let state = Arc::clone(&state);
            let child = Arc::clone(&child);
            thread::spawn(move || reader_loop(reader, state, child));
        }

        Ok(Self {
            id,
            command: command.to_string(),
            state,
            master: pair.master,
            writer,
            child,
            cols,
            rows,
        })
    }

    pub fn metadata(&self) -> SessionMetadata {
        let state = self.state.lock().unwrap();
        SessionMetadata {
            id: self.id.clone(),
            command: self.command.clone(),
            status: state.status.clone(),
            cols: self.cols,
            rows: self.rows,
        }
    }

    pub fn status(&self) -> SessionStatus {
        self.state.lock().unwrap().status.clone()
    }

    pub fn write_bytes(&mut self, data: &[u8]) -> Result<()> {
        if matches!(self.status(), SessionStatus::Exited(_)) {
            return Err(PtyError::Exited);
        }
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn get_screen(&self) -> String {
        self.state.lock().unwrap().parser.screen().contents()
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        self.state.lock().unwrap().parser.screen().cursor_position()
    }

    /// Walk back through the vt100 scrollback by paging the visible window
    /// and stitching the unique top rows together. Returns up to `max_lines`
    /// lines in chronological order (oldest first).
    pub fn get_scrollback(&self, max_lines: usize) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }
        let mut state = self.state.lock().unwrap();
        let original = state.parser.screen().scrollback();
        let rows = self.rows as usize;

        // Probe the actual scrollback depth by requesting an absurdly large
        // offset — vt100 clamps to the real max and reports it via scrollback().
        state.parser.set_scrollback(usize::MAX / 2);
        let depth = state.parser.screen().scrollback();

        let mut history: Vec<String> = Vec::new();
        if depth > 0 && rows > 0 {
            // Page through history from oldest to newest. At offset = depth,
            // the very top row of the visible window is the OLDEST scrollback
            // line. As we decrement the offset by `rows`, we shift forward in
            // time. We only keep the TOP row of each page so we don't have to
            // dedupe overlap with the live screen.
            let mut offset = depth;
            loop {
                state.parser.set_scrollback(offset);
                let page = state.parser.screen().contents();
                let lines: Vec<&str> = page.lines().collect();
                let take_n = rows.min(offset);
                for line in lines.iter().take(take_n) {
                    history.push((*line).to_string());
                }
                if offset <= rows {
                    break;
                }
                offset -= rows;
            }
        }

        state.parser.set_scrollback(original);

        if history.len() > max_lines {
            let drop = history.len() - max_lines;
            history.drain(..drop);
        }
        history
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let mut state = self.state.lock().unwrap();
        state.parser.set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    pub fn kill(&mut self) -> Result<()> {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        Ok(())
    }

    pub fn idle_seconds(&self) -> f64 {
        self.state.lock().unwrap().last_output.elapsed().as_secs_f64()
    }

    /// Snapshot the parser state and produce screen contents.
    /// Cheap; used by polling loops in wait_for / wait_for_idle.
    pub fn snapshot_screen(&self) -> String {
        self.state.lock().unwrap().parser.screen().contents()
    }
}

fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    state: Arc<Mutex<SharedState>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
) {
    let mut buf = [0u8; 32768];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF — child closed PTY
            Ok(n) => {
                let mut s = state.lock().unwrap();
                s.parser.process(&buf[..n]);
                s.last_output = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // Reader ended — child likely exited. Reap exit status.
    let code = {
        let mut child = child.lock().unwrap();
        match child.wait() {
            Ok(status) => {
                if status.success() {
                    0
                } else {
                    status.exit_code() as i32
                }
            }
            Err(_) => -1,
        }
    };
    let mut s = state.lock().unwrap();
    s.status = SessionStatus::Exited(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn wait_for_screen(session: &PtySession, needle: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if session.get_screen().contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn spawn_and_capture_output() {
        let mut s = PtySession::spawn(
            "t1".into(),
            "/bin/sh",
            &["-c".into(), "echo hello-world".into()],
            None,
            80,
            24,
        )
        .unwrap();
        assert!(wait_for_screen(&s, "hello-world", Duration::from_secs(3)));
        let _ = s.kill();
    }

    #[test]
    fn metadata_reports_dimensions() {
        let mut s = PtySession::spawn(
            "t2".into(),
            "/bin/sh",
            &["-c".into(), "sleep 5".into()],
            None,
            100,
            30,
        )
        .unwrap();
        let m = s.metadata();
        assert_eq!(m.cols, 100);
        assert_eq!(m.rows, 30);
        assert_eq!(m.status, SessionStatus::Running);
        let _ = s.kill();
    }

    #[test]
    fn resize_updates_parser() {
        let mut s = PtySession::spawn(
            "t3".into(),
            "/bin/sh",
            &["-c".into(), "sleep 5".into()],
            None,
            80,
            24,
        )
        .unwrap();
        s.resize(120, 40).unwrap();
        let m = s.metadata();
        assert_eq!(m.cols, 120);
        assert_eq!(m.rows, 40);
        let _ = s.kill();
    }

    #[test]
    fn write_after_exit_errors() {
        let mut s = PtySession::spawn(
            "t4".into(),
            "/bin/sh",
            &["-c".into(), "exit 0".into()],
            None,
            80,
            24,
        )
        .unwrap();
        // Give the reader a moment to observe EOF and mark exited.
        for _ in 0..50 {
            if matches!(s.status(), SessionStatus::Exited(_)) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(s.status(), SessionStatus::Exited(_)));
        assert!(s.write_bytes(b"hi").is_err());
    }
}
