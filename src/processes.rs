//! Operator cancellation for trusted compiler/runtime process groups.
use std::collections::BTreeSet;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

#[derive(Clone, Default)]
pub(super) struct ProcessControl {
    inner: Arc<Control>,
}
#[derive(Default)]
struct Control {
    detached: bool,
    stopped: AtomicBool,
    groups: Mutex<BTreeSet<u32>>,
}
impl ProcessControl {
    #[cfg(all(feature = "mcp", unix))]
    pub fn hosted() -> Self {
        Self {
            inner: Arc::new(Control {
                detached: true,
                ..Control::default()
            }),
        }
    }
    pub fn stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }
    #[cfg(all(feature = "mcp", unix))]
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        for &pid in self.inner.groups.lock().unwrap().iter() {
            if self.inner.detached {
                kill_group(pid);
            }
        }
    }
    pub fn track(&self, pid: u32) -> Group {
        self.inner.groups.lock().unwrap().insert(pid);
        if self.stopped() && self.inner.detached {
            kill_group(pid);
        }
        Group {
            pid,
            control: self.clone(),
        }
    }
}
pub(super) struct Group {
    pid: u32,
    control: ProcessControl,
}
impl Group {
    pub fn kill(&self) {
        if self.control.inner.detached {
            kill_group(self.pid);
        }
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
        self.control.inner.groups.lock().unwrap().remove(&self.pid);
    }
}
pub(super) fn separate_group(command: &mut Command, control: &ProcessControl) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if control.inner.detached {
            command.process_group(0);
        }
    }
}
fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        // Each tracked trusted child starts a fresh group whose ID is its PID.
        // Killing the group includes compiler/runtime descendants holding pipe ends.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}
