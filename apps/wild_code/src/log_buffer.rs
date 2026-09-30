use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// A shared buffer that captures formatted tracing output line-by-line.
pub struct LogBuffer {
    buffer: Arc<Mutex<VecDeque<String>>>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn drain(&self) -> Vec<String> {
        let mut buf = self.buffer.lock().expect("LogBuffer Mutex poisoned");
        buf.drain(..).collect()
    }
}

/// A wrapper around `Arc<LogBuffer>` that implements `MakeWriter`.
/// This is needed because Rust's orphan rule prevents implementing
/// a foreign trait (MakeWriter) for a foreign type (Arc).
pub struct SharedLogBuffer(pub Arc<LogBuffer>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogBuffer {
    type Writer = LogBufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogBufferWriter {
            buffer: self.0.buffer.clone(),
        }
    }
}

pub struct LogBufferWriter {
    buffer: Arc<Mutex<VecDeque<String>>>,
}

impl Write for LogBufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s = String::from_utf8_lossy(buf).to_string();
        let mut buffer = self.buffer.lock().expect("LogBuffer Mutex poisoned");
        for line in s.split_inclusive('\n') {
            let trimmed = line.trim_end_matches('\n');
            if !trimmed.is_empty() {
                buffer.push_back(trimmed.to_string());
            }
        }
        while buffer.len() > 2000 {
            buffer.pop_front();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
