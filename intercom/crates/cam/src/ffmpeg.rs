use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::sleep;

#[rustfmt::skip]
pub async fn run() {
    loop {
        eprintln!("Starting FFmpeg");

        let mut proc = Command::new("ffmpeg")
            .args([
                "-timeout", "5000000",
                "-rtsp_transport", "tcp",
                "-i", crate::CAMERA_STREAM,
                "-an",
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-tune", "zerolatency",
                "-r", "15",
                "-g", "1",
                "-pkt_size", "1200",
                "-f", "rtp",
                &format!("rtp://{}", crate::FFMPEG_STREAM),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn ffmpeg");

        let status = proc.wait().await.expect("wait ffmpeg");
        eprintln!("FFmpeg exited: {status}");

        sleep(Duration::from_secs(1)).await;
    }
}
