use proxy_common::encode_lsp;
use serde::Serialize;
use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
};

#[derive(Clone)]
pub struct Output {
    sender: mpsc::Sender<Vec<u8>>,
    failed: Arc<AtomicBool>,
}

impl Output {
    pub fn start() -> Self {
        Self::start_with_writer(io::stdout())
    }

    fn start_with_writer(writer: impl Write + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let writer_failed = Arc::clone(&failed);
        thread::spawn(move || write_loop(writer, receiver, writer_failed));
        Self { sender, failed }
    }

    pub fn send_raw(&self, raw: Vec<u8>) -> bool {
        let sent = self.sender.send(raw).is_ok();
        if !sent {
            self.failed.store(true, Ordering::Relaxed);
        }
        sent
    }

    pub fn send_value(&self, value: &impl Serialize) -> bool {
        self.send_raw(encode_lsp(value).into_bytes())
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

fn write_loop(mut writer: impl Write, receiver: mpsc::Receiver<Vec<u8>>, failed: Arc<AtomicBool>) {
    while let Ok(message) = receiver.recv() {
        if writer.write_all(&message).is_err() || writer.flush().is_err() {
            failed.store(true, Ordering::Relaxed);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn serializes_complete_frames_in_submission_order() {
        let writer = SharedWriter::default();
        let bytes = Arc::clone(&writer.0);
        let output = Output::start_with_writer(writer);

        assert!(output.send_raw(b"first".to_vec()));
        assert!(output.send_raw(b"second".to_vec()));
        drop(output);

        for _ in 0..100 {
            if bytes.lock().unwrap().len() == 11 {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }

        assert_eq!(*bytes.lock().unwrap(), b"firstsecond");
    }

    #[test]
    fn reports_writer_failure() {
        let output = Output::start_with_writer(FailingWriter);
        assert!(output.send_raw(b"message".to_vec()));

        for _ in 0..100 {
            if output.failed() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }

        assert!(output.failed());
    }
}
