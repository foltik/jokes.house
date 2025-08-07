use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::{Child, Command};

#[derive(Default)]
pub struct FFmpeg {
    viewers: AtomicU64,
    proc: Mutex<Option<Child>>,
}

impl FFmpeg {
    pub fn inc_viewers(&self) {
        if self.viewers.fetch_add(1, Ordering::SeqCst) == 0 {
            eprintln!("Starting FFmpeg");

            #[rustfmt::skip]
            let proc = Command::new("ffmpeg")
                .args([
                    "-rtsp_transport", "tcp",
                    "-i", crate::CAMERA_STREAM,
                    "-an",
                    "-c:v", "libx264",
                    "-preset", "ultrafast",
                    "-tune", "zerolatency",
                    "-force_key_frames", "expr:gte(t,n_forced*2)",
                    "-f", "rtp",
                    &format!("rtp://{}", crate::FFMPEG_STREAM),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn ffmpeg");

            *self.proc.lock().unwrap() = Some(proc);
        }
    }

    pub fn dec_viewers(&self) {
        if self.viewers.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Some(mut proc) = self.proc.lock().unwrap().take() {
                eprintln!("Stopping FFmpeg");
                proc.start_kill().expect("kill ffmpeg");
            }
        }
    }
}
