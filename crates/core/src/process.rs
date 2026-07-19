use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

pub const MANAGED_FFMPEG_VERSION: &str = "8.1.2";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(crate) fn background_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        command.creation_flags(CREATE_NO_WINDOW);
    }

    command
}

pub(crate) fn ffmpeg_command() -> Command {
    background_command(media_tool_path("ffmpeg.exe"))
}

pub(crate) fn ffprobe_command() -> Command {
    background_command(media_tool_path("ffprobe.exe"))
}

pub fn ffmpeg_path() -> PathBuf {
    media_tool_path("ffmpeg.exe")
}

pub fn ffprobe_path() -> PathBuf {
    media_tool_path("ffprobe.exe")
}

fn media_tool_path(file_name: &str) -> PathBuf {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let managed = PathBuf::from(local_app_data)
            .join("CdxVidExt")
            .join("runtime")
            .join("ffmpeg")
            .join(MANAGED_FFMPEG_VERSION)
            .join("bin")
            .join(file_name);
        if managed.is_file() {
            return managed;
        }
    }

    if let Ok(executable) = std::env::current_exe()
        && let Some(bin_dir) = executable.parent()
        && let Some(plugin_root) = bin_dir.parent()
    {
        let bundled = plugin_root
            .join("runtime")
            .join("ffmpeg")
            .join("bin")
            .join(file_name);
        if bundled.is_file() {
            return bundled;
        }
    }

    PathBuf::from(file_name.trim_end_matches(".exe"))
}
