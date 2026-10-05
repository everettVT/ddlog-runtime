//! Best-effort process telemetry must not hold world ownership indefinitely.
use std::io::{ErrorKind, Read};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SAMPLE_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_SAMPLE_BYTES: usize = 256;

struct Sampler(Child);
impl Drop for Sampler {
    fn drop(&mut self) {
        // This child alone belongs to the sampler. Never kill its process group
        // or a descendant that happened to inherit an output pipe.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn output(command: &mut Command) -> Option<Vec<u8>> {
    let deadline = Instant::now() + SAMPLE_TIMEOUT;
    let mut sampler = Sampler(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?,
    );
    let mut stdout = sampler.0.stdout.take()?;
    super::workers::nonblocking(&stdout).ok()?;
    let mut bytes = Vec::new();
    let mut eof = false;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let mut chunk = [0; MAX_SAMPLE_BYTES + 1];
        match stdout.read(&mut chunk) {
            Ok(0) => eof = true,
            Ok(count) => {
                if bytes.len() + count > MAX_SAMPLE_BYTES {
                    return None;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
        if let Some(status) = sampler.0.try_wait().ok()? {
            if !status.success() {
                return None;
            }
            if eof {
                return Some(bytes);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(super) fn sample(command: &mut Command) -> Option<(f64, u64)> {
    let output = output(command)?;
    let text = std::str::from_utf8(&output).ok()?;
    let mut fields = text.split_whitespace();
    let cpu = fields.next()?.parse::<f64>().ok()?;
    let rss = fields.next()?.parse::<u64>().ok()?;
    if fields.next().is_some() || !cpu.is_finite() || cpu < 0.0 {
        return None;
    }
    Some((cpu, rss))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn marker(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ddlog-sampler-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn exited_sampler_with_inherited_stdout_returns_without_waiting_for_holder() {
        let pid_file = marker("holder");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "sleep 5 & echo $! > \"$1\"; printf '1.0 1024\\n'",
                "sampler",
            ])
            .arg(&pid_file);
        let started = Instant::now();
        assert!(output(&mut command).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let holder: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            unsafe { libc::kill(holder, 0) },
            0,
            "pipe holder must remain alive"
        );
        // This PID was created by this test and retained in its private marker.
        unsafe {
            libc::kill(holder, libc::SIGTERM);
        }
        std::fs::remove_file(pid_file).unwrap();
    }

    #[test]
    fn stalled_sampler_is_killed_and_reaped_within_the_deadline() {
        let pid_file = marker("sampler");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "echo $$ > \"$1\"; exec sleep 5", "sampler"])
            .arg(&pid_file);
        let started = Instant::now();
        assert!(output(&mut command).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        std::fs::remove_file(pid_file).unwrap();
    }

    #[test]
    fn malformed_samples_are_unavailable() {
        for text in ["not metrics", "NaN 1024", "-1 1024", "1 -1", "1 1024 extra"] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "printf '%s' \"$1\"", "sampler", text]);
            assert!(sample(&mut command).is_none(), "{text}");
        }
    }

    #[test]
    fn excessive_output_is_bounded_and_a_small_success_is_retained() {
        let mut flood = Command::new("/bin/sh");
        flood.args([
            "-c",
            "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
        ]);
        let started = Instant::now();
        assert!(output(&mut flood).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let mut valid = Command::new("/bin/sh");
        valid.args(["-c", "printf '1.0 1024\\n'"]);
        assert_eq!(output(&mut valid), Some(b"1.0 1024\n".to_vec()));
    }
}
