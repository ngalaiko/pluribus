use pluribus_runtime_wasm::CancellationHandle;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub fn request(data: &crate::Paths) -> Result<(), Box<dyn Error>> {
    fs::write(data.state.join("STOP"), b"")?;
    println!("stop requested");
    Ok(())
}

/// Reports whether the emergency stop file is present.
#[must_use]
pub fn is_stopped(data: &crate::Paths) -> bool {
    data.state.join("STOP").try_exists().unwrap_or(false)
}

pub fn prepare(data: &crate::Paths, resume: bool) -> Result<(), Box<dyn Error>> {
    let path = data.state.join("STOP");
    if resume {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    } else if path.try_exists()? {
        return Err("emergency stop is active; use `run --resume`".into());
    }
    Ok(())
}

pub struct Monitor {
    finished: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn new(path: PathBuf, stopped: Arc<AtomicBool>, handles: Vec<CancellationHandle>) -> Self {
        let finished = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&finished);
        let worker = thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                if path.try_exists().unwrap_or(true) {
                    stopped.store(true, Ordering::Release);
                    for handle in &handles {
                        handle.shutdown();
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        Self {
            finished,
            worker: Some(worker),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_halts_cognition_and_requires_explicit_resume() {
        let dir = tempfile::TempDir::new().unwrap();
        prepare(&crate::Paths::under(dir.path()), false).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let _monitor = Monitor::new(dir.path().join("STOP"), Arc::clone(&stopped), vec![]);
        request(&crate::Paths::under(dir.path())).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !stopped.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(prepare(&crate::Paths::under(dir.path()), false).is_err());
        prepare(&crate::Paths::under(dir.path()), true).unwrap();
        prepare(&crate::Paths::under(dir.path()), false).unwrap();
    }
}
