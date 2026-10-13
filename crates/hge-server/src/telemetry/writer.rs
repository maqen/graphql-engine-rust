//! Where log lines go: a dedicated thread writing to stdout, so request handlers only pay
//! for formatting a line and a channel send, never for a blocking write.

use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Lines buffered before senders block (backpressure rather than dropping logs).
const CAPACITY: usize = 16 * 1024;

pub(crate) enum Msg {
    Line(Vec<u8>),
    Flush(SyncSender<()>),
}

#[derive(Clone)]
pub(crate) enum Sink {
    Stdout(SyncSender<Msg>),
    /// Lines kept in memory (tests).
    Capture(Arc<Mutex<Vec<String>>>),
}

impl Sink {
    pub(crate) fn stdout() -> Sink {
        let (tx, rx) = std::sync::mpsc::sync_channel(CAPACITY);
        std::thread::Builder::new()
            .name("hge-log-writer".to_owned())
            .spawn(move || run(rx, std::io::stdout()))
            .expect("spawn the log writer thread");
        Sink::Stdout(tx)
    }

    pub(crate) fn write(&self, line: Vec<u8>) {
        match self {
            Sink::Stdout(tx) => match tx.try_send(Msg::Line(line)) {
                Ok(()) => {}
                Err(TrySendError::Full(msg)) => {
                    let _ = tx.send(msg);
                }
                Err(TrySendError::Disconnected(_)) => {}
            },
            Sink::Capture(lines) => {
                let text = String::from_utf8_lossy(&line).trim_end().to_owned();
                lines.lock().expect("log capture").push(text);
            }
        }
    }

    /// Wait (at most `timeout`) until everything sent so far is written.
    pub(crate) fn flush(&self, timeout: Duration) {
        if let Sink::Stdout(tx) = self {
            let (ack, done) = std::sync::mpsc::sync_channel(1);
            if tx.send(Msg::Flush(ack)).is_ok() {
                let _ = done.recv_timeout(timeout);
            }
        }
    }
}

fn run(rx: Receiver<Msg>, out: impl Write) {
    let mut out = std::io::BufWriter::with_capacity(64 * 1024, out);
    loop {
        // Block for the next message, then drain whatever else is queued before flushing,
        // so bursts become few large writes.
        let msg = match rx.recv() {
            Ok(m) => m,
            Err(_) => break,
        };
        let mut next = Some(msg);
        while let Some(msg) = next.take() {
            match msg {
                Msg::Line(line) => {
                    let _ = out.write_all(&line);
                }
                Msg::Flush(ack) => {
                    let _ = out.flush();
                    let _ = ack.send(());
                }
            }
            next = match rx.recv_timeout(Duration::ZERO) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => None,
            };
        }
        let _ = out.flush();
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_in_order_and_flushes() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let out = Shared::default();
        let writer = {
            let out = out.clone();
            std::thread::spawn(move || run(rx, out))
        };
        let sink = Sink::Stdout(tx);
        for i in 0..100 {
            sink.write(format!("{i}\n").into_bytes());
        }
        sink.flush(Duration::from_secs(5));
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let expected: String = (0..100).map(|i| format!("{i}\n")).collect();
        assert_eq!(text, expected);
        drop(sink);
        writer.join().unwrap();
    }
}
