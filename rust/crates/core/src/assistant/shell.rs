//! The real shell: one PowerShell command, with a timeout, no window, and output as UTF-8.
//!
//! Only ever reached through the `run_shell` tool, after the user has said yes.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use api::chat::Cancel;

use super::{ShellExec, ShellOutput};

pub struct SystemShell;

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

impl ShellExec for SystemShell {
    fn run(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<ShellOutput, String> {
        // Output as UTF-8 whatever the console code page is.
        let script = format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {command}");
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start PowerShell: {e}"))?;
        let out = child.stdout.take().map(drain);
        let err = child.stderr.take().map(drain);
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => return Err(format!("the command could not be waited on: {e}")),
            }
            if cancel.is_cancelled() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut text = String::new();
        for pipe in [out, err].into_iter().flatten() {
            let bytes = pipe.join().unwrap_or_default();
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
        match status {
            Some(s) => Ok(ShellOutput {
                code: s.code(),
                output: text,
            }),
            None if cancel.is_cancelled() => Err("cancelled".into()),
            None => Err(format!(
                "the command was stopped after {} s",
                timeout.as_secs().max(1)
            )),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn runs_a_command_and_returns_its_output_and_code() {
        let out = SystemShell
            .run(
                "Write-Output 'caf\u{e9} ok'; exit 3",
                None,
                Duration::from_secs(30),
                &Cancel::new(),
            )
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert!(out.output.contains("caf\u{e9} ok"), "{:?}", out.output);
    }

    #[test]
    fn a_runaway_command_is_stopped() {
        let t = Instant::now();
        let err = SystemShell
            .run(
                "Start-Sleep 30",
                None,
                Duration::from_millis(1500),
                &Cancel::new(),
            )
            .unwrap_err();
        assert!(err.contains("stopped after"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(15));
    }
}
