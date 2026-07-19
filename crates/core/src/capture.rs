use std::io::Write;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use crossbeam_channel::{Sender, bounded};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::clock::qpc_now_100ns;
use crate::model::{CaptureQuality, CaptureResolution, MonitorInfo};
use crate::process::ffmpeg_command;
use crate::store::{MEDIA_FILE, SessionWriter};

struct CapturedFrame {
    pixels: Vec<u8>,
    source_timestamp_100ns: i64,
    dropped_before: i64,
}

struct CaptureFlags {
    writer: Arc<SessionWriter>,
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    quality: CaptureQuality,
    resolution: CaptureResolution,
}

struct WgcHandler {
    sender: Option<Sender<CapturedFrame>>,
    worker: Option<JoinHandle<()>>,
    dropped: Arc<AtomicI64>,
}

impl Drop for WgcHandler {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl GraphicsCaptureApiHandler for WgcHandler {
    type Flags = CaptureFlags;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        let (sender, receiver) = bounded::<CapturedFrame>(3);
        let dropped = Arc::new(AtomicI64::new(0));
        let worker = thread::Builder::new()
            .name("cdx-frame-encoder".into())
            .spawn(move || {
                let flags = ctx.flags;
                let part_path = flags.writer.dir.join("capture.mp4.part");
                let final_path = flags.writer.dir.join(MEDIA_FILE);
                let (mut child, encoder_name) = match spawn_ffmpeg(
                    flags.source_width,
                    flags.source_height,
                    flags.output_width,
                    flags.output_height,
                    flags.quality,
                    &part_path,
                ) {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = flags.writer.add_event(
                            0,
                            "recorder",
                            "encoder_error",
                            &error.to_string(),
                            None,
                            None,
                            &serde_json::json!({}),
                            None,
                        );
                        return;
                    }
                };
                if let Err(error) =
                    flags
                        .writer
                        .set_capture_profile(encoder_name, flags.quality, flags.resolution)
                {
                    let _ = child.kill();
                    let _ = flags.writer.add_event(
                        0,
                        "recorder",
                        "encoder_error",
                        &error.to_string(),
                        None,
                        None,
                        &serde_json::json!({}),
                        None,
                    );
                    return;
                }
                let mut stdin = child.stdin.take();
                let mut frame_number = 0_i64;
                let mut next_slot = 0_i64;
                let frame_period = 333_333_i64;
                let mut last_pixels = None::<Vec<u8>>;
                let mut last_source = 0_i64;
                let mut scheduler_drops = 0_i64;
                let mut previous_sample = None::<u64>;
                for frame in receiver {
                    let source_offset = frame
                        .source_timestamp_100ns
                        .saturating_sub(flags.writer.origin_100ns);
                    if source_offset < next_slot {
                        scheduler_drops += 1;
                        continue;
                    }
                    while next_slot.saturating_add(frame_period) <= source_offset {
                        let Some(pixels) = last_pixels.as_ref() else {
                            break;
                        };
                        if stdin
                            .as_mut()
                            .map(|pipe| pipe.write_all(pixels).is_err())
                            .unwrap_or(true)
                        {
                            break;
                        }
                        let _ = flags.writer.add_frame(
                            frame_number,
                            next_slot,
                            last_source,
                            true,
                            0,
                            Some(0.0),
                        );
                        frame_number += 1;
                        next_slot = next_slot.saturating_add(frame_period);
                    }
                    if let Some(pipe) = stdin.as_mut() {
                        if pipe.write_all(&frame.pixels).is_err() {
                            break;
                        }
                    }
                    let sample = visual_sample(&frame.pixels);
                    let change =
                        previous_sample.map(|last| sample.abs_diff(last) as f64 / u64::MAX as f64);
                    previous_sample = Some(sample);
                    let _ = flags.writer.add_frame(
                        frame_number,
                        next_slot,
                        frame.source_timestamp_100ns,
                        false,
                        frame.dropped_before.saturating_add(scheduler_drops),
                        change,
                    );
                    frame_number += 1;
                    next_slot = next_slot.saturating_add(frame_period);
                    last_source = frame.source_timestamp_100ns;
                    last_pixels = Some(frame.pixels);
                    scheduler_drops = 0;
                }
                if let Some(pixels) = last_pixels.as_ref() {
                    let end_offset = qpc_now_100ns()
                        .unwrap_or(flags.writer.origin_100ns)
                        .saturating_sub(flags.writer.origin_100ns);
                    while next_slot <= end_offset {
                        if stdin
                            .as_mut()
                            .map(|pipe| pipe.write_all(pixels).is_err())
                            .unwrap_or(true)
                        {
                            break;
                        }
                        let _ = flags.writer.add_frame(
                            frame_number,
                            next_slot,
                            last_source,
                            true,
                            0,
                            Some(0.0),
                        );
                        frame_number += 1;
                        next_slot = next_slot.saturating_add(frame_period);
                    }
                }
                drop(stdin);
                if child.wait().map(|s| s.success()).unwrap_or(false) {
                    let _ = std::fs::rename(&part_path, &final_path);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            dropped,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        let source_timestamp_100ns = frame.timestamp().map_err(|e| e.to_string())?.Duration;
        let mut no_padding = Vec::new();
        let buffer = frame.buffer().map_err(|e| e.to_string())?;
        let pixels = buffer.as_nopadding_buffer(&mut no_padding).to_vec();
        let dropped_before = self.dropped.swap(0, Ordering::AcqRel);
        if self
            .sender
            .as_ref()
            .unwrap()
            .try_send(CapturedFrame {
                pixels,
                source_timestamp_100ns,
                dropped_before,
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

pub struct CaptureSession {
    control: CaptureControl<WgcHandler, String>,
}

impl CaptureSession {
    pub fn start(
        monitor_index: usize,
        writer: Arc<SessionWriter>,
        resolution: CaptureResolution,
        quality: CaptureQuality,
    ) -> Result<Self> {
        let monitor =
            Monitor::from_index(monitor_index).context("selected monitor is unavailable")?;
        let (source_width, source_height) = (monitor.width()?, monitor.height()?);
        let (output_width, output_height) =
            output_dimensions(source_width, source_height, resolution);
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Exclude,
            MinimumUpdateIntervalSettings::Custom(Duration::from_nanos(33_333_333)),
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            CaptureFlags {
                writer,
                source_width,
                source_height,
                output_width,
                output_height,
                quality,
                resolution,
            },
        );
        let control = WgcHandler::start_free_threaded(settings)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(Self { control })
    }

    pub fn stop(self) -> Result<()> {
        self.control
            .stop()
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let primary = Monitor::primary().ok();
    Monitor::enumerate()?
        .into_iter()
        .map(|monitor| {
            Ok(MonitorInfo {
                index: monitor.index()?,
                name: monitor
                    .name()
                    .or_else(|_| monitor.device_string())
                    .unwrap_or_else(|_| "Windows display".into()),
                device_name: monitor.device_name()?,
                width: monitor.width()?,
                height: monitor.height()?,
                primary: primary == Some(monitor),
            })
        })
        .collect()
}

pub fn output_dimensions(width: u32, height: u32, resolution: CaptureResolution) -> (u32, u32) {
    let (limit_width, limit_height) = match resolution {
        CaptureResolution::Hd1080 => (1920, 1080),
        CaptureResolution::Qhd2k => (2560, 1440),
        CaptureResolution::Native => return ((width & !1).max(2), (height & !1).max(2)),
    };
    if width <= limit_width && height <= limit_height {
        return (width & !1, height & !1);
    }
    let scale = (limit_width as f64 / width as f64).min(limit_height as f64 / height as f64);
    let output_width = ((width as f64 * scale).round() as u32) & !1;
    let output_height = ((height as f64 * scale).round() as u32) & !1;
    (output_width.max(2), output_height.max(2))
}

fn spawn_ffmpeg(
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    quality: CaptureQuality,
    output: &std::path::Path,
) -> Result<(Child, &'static str)> {
    let encoder = select_encoder(output_width, output_height, quality)?;
    let input_size = format!("{source_width}x{source_height}");
    let scale = format!("scale={output_width}:{output_height}:flags=lanczos,fps=30");
    let mut command = ffmpeg_command();
    command.args([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-f",
        "rawvideo",
        "-pixel_format",
        "bgra",
        "-video_size",
        &input_size,
        "-framerate",
        "30",
        "-i",
        "pipe:0",
        "-an",
        "-vf",
        &scale,
        "-c:v",
        encoder,
    ]);
    command.args(quality_arguments(encoder, quality));
    let child = command
        .args([
            "-pix_fmt",
            "yuv420p",
            "-g",
            "30",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            "-y",
        ])
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start FFmpeg")?;
    Ok((child, encoder))
}

fn select_encoder(
    output_width: u32,
    output_height: u32,
    quality: CaptureQuality,
) -> Result<&'static str> {
    for encoder in ["h264_nvenc", "h264_amf", "libx264"] {
        let size = format!("size={}x{}:rate=1", output_width, output_height);
        let mut command = ffmpeg_command();
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("color={size}"),
            "-frames:v",
            "1",
            "-c:v",
            encoder,
        ]);
        command.args(quality_arguments(encoder, quality));
        let available = command
            .args(["-pix_fmt", "yuv420p", "-f", "null", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if available.map(|status| status.success()).unwrap_or(false) {
            return Ok(encoder);
        }
    }
    bail!("FFmpeg is unavailable or no working H.264 encoder was found")
}

fn quality_arguments(encoder: &str, quality: CaptureQuality) -> Vec<&'static str> {
    let quantizer = match quality {
        CaptureQuality::Low => "30",
        CaptureQuality::Medium => "23",
        CaptureQuality::High => "18",
    };
    match encoder {
        "h264_nvenc" => vec![
            "-preset",
            match quality {
                CaptureQuality::Low => "p3",
                CaptureQuality::Medium => "p4",
                CaptureQuality::High => "p6",
            },
            "-tune",
            "hq",
            "-rc",
            "vbr",
            "-cq",
            quantizer,
            "-b:v",
            "0",
        ],
        "h264_amf" => vec![
            "-quality",
            match quality {
                CaptureQuality::Low => "speed",
                CaptureQuality::Medium => "balanced",
                CaptureQuality::High => "quality",
            },
            "-rc",
            "qvbr",
            "-qvbr_quality_level",
            quantizer,
        ],
        _ => vec![
            "-preset",
            match quality {
                CaptureQuality::Low => "veryfast",
                CaptureQuality::Medium => "fast",
                CaptureQuality::High => "medium",
            },
            "-crf",
            quantizer,
        ],
    }
}

fn visual_sample(pixels: &[u8]) -> u64 {
    pixels
        .chunks(4096)
        .fold(0xcbf29ce484222325_u64, |hash, chunk| {
            (hash ^ u64::from(chunk.first().copied().unwrap_or(0))).wrapping_mul(0x100000001b3)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CaptureQuality, CaptureResolution};

    #[test]
    fn primary_1920_by_1200_scales_to_expected_size() {
        assert_eq!(
            output_dimensions(1920, 1200, CaptureResolution::Hd1080),
            (1728, 1080)
        );
    }

    #[test]
    fn resolution_modes_preserve_aspect_ratio_and_native_pixels() {
        assert_eq!(
            output_dimensions(3840, 2160, CaptureResolution::Hd1080),
            (1920, 1080)
        );
        assert_eq!(
            output_dimensions(3840, 2160, CaptureResolution::Qhd2k),
            (2560, 1440)
        );
        assert_eq!(
            output_dimensions(3841, 2161, CaptureResolution::Native),
            (3840, 2160)
        );
    }

    #[test]
    fn quality_profiles_map_to_real_encoder_specific_arguments() {
        assert_eq!(
            quality_arguments("h264_nvenc", CaptureQuality::High),
            vec![
                "-preset", "p6", "-tune", "hq", "-rc", "vbr", "-cq", "18", "-b:v", "0"
            ]
        );
        assert_eq!(
            quality_arguments("h264_amf", CaptureQuality::Low),
            vec![
                "-quality",
                "speed",
                "-rc",
                "qvbr",
                "-qvbr_quality_level",
                "30"
            ]
        );
        assert_eq!(
            quality_arguments("libx264", CaptureQuality::Medium),
            vec!["-preset", "fast", "-crf", "23"]
        );
    }

    #[test]
    fn selected_medium_profile_encodes_a_real_ffmpeg_frame() {
        let encoder = select_encoder(96, 64, CaptureQuality::Medium).unwrap();
        assert!(["h264_nvenc", "h264_amf", "libx264"].contains(&encoder));
    }
}
