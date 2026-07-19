use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::Mutex;
use serde_json::json;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, ToUnicodeEx};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetWindowTextW,
    GetWindowThreadProcessId, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, PM_REMOVE, PeekMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN,
};

use crate::clock::qpc_now_100ns;
use crate::store::{SessionWriter, public_input_event};

#[derive(Clone, Copy)]
enum RawInput {
    Flush,
    Mouse {
        at: i64,
        message: u32,
        x: i32,
        y: i32,
        data: u32,
        flags: u32,
    },
    Keyboard {
        at: i64,
        message: u32,
        vk: u32,
        scan: u32,
        flags: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyMeaning {
    Printable(char),
    Backspace,
    Command(String),
    Modifier,
    Ignore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SemanticInput {
    TextLine {
        text: String,
        start_offset_100ns: i64,
        end_offset_100ns: i64,
    },
    Command {
        name: String,
        offset_100ns: i64,
    },
}

#[derive(Default)]
struct TextAssembler {
    buffer: String,
    start_offset_100ns: Option<i64>,
    end_offset_100ns: Option<i64>,
}

impl TextAssembler {
    fn feed(&mut self, meaning: KeyMeaning, offset_100ns: i64) -> Vec<SemanticInput> {
        match meaning {
            KeyMeaning::Printable(value) => {
                self.start_offset_100ns.get_or_insert(offset_100ns);
                self.end_offset_100ns = Some(offset_100ns);
                self.buffer.push(value);
                Vec::new()
            }
            KeyMeaning::Backspace => {
                self.buffer.pop();
                Vec::new()
            }
            KeyMeaning::Command(name) => {
                let mut output = self.flush();
                output.push(SemanticInput::Command { name, offset_100ns });
                output
            }
            KeyMeaning::Modifier | KeyMeaning::Ignore => Vec::new(),
        }
    }

    fn flush(&mut self) -> Vec<SemanticInput> {
        if self.buffer.is_empty() {
            self.start_offset_100ns = None;
            self.end_offset_100ns = None;
            return Vec::new();
        }
        let text = std::mem::take(&mut self.buffer);
        let start_offset_100ns = self.start_offset_100ns.take().unwrap_or_default();
        let end_offset_100ns = self.end_offset_100ns.take().unwrap_or(start_offset_100ns);
        vec![SemanticInput::TextLine {
            text,
            start_offset_100ns,
            end_offset_100ns,
        }]
    }
}

static CALLBACK_SENDER: OnceLock<Mutex<Option<Sender<RawInput>>>> = OnceLock::new();

unsafe extern "system" fn mouse_callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let details = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
        if let Some(sender) = CALLBACK_SENDER.get().and_then(|slot| slot.lock().clone()) {
            let _ = sender.try_send(RawInput::Mouse {
                at: qpc_now_100ns().unwrap_or_default(),
                message: wparam.0 as u32,
                x: details.pt.x,
                y: details.pt.y,
                data: details.mouseData,
                flags: details.flags,
            });
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let details = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if let Some(sender) = CALLBACK_SENDER.get().and_then(|slot| slot.lock().clone()) {
            let _ = sender.try_send(RawInput::Keyboard {
                at: qpc_now_100ns().unwrap_or_default(),
                message: wparam.0 as u32,
                vk: details.vkCode,
                scan: details.scanCode,
                flags: details.flags.0,
            });
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

pub struct InputObserver {
    running: std::sync::Arc<AtomicBool>,
    sender: Sender<RawInput>,
    hook_thread: Option<JoinHandle<Result<()>>>,
    writer_thread: Option<JoinHandle<()>>,
}

impl InputObserver {
    pub fn start(writer: std::sync::Arc<SessionWriter>) -> Result<Self> {
        let (sender, receiver) = bounded(4096);
        let slot = CALLBACK_SENDER.get_or_init(|| Mutex::new(None));
        if slot.lock().is_some() {
            bail!("input observer is already running");
        }
        *slot.lock() = Some(sender.clone());

        let running = std::sync::Arc::new(AtomicBool::new(true));
        let hook_running = running.clone();
        let hook_thread = thread::Builder::new()
            .name("cdx-input-hooks".into())
            .spawn(move || install_hooks(hook_running))?;
        let writer_running = running.clone();
        let writer_thread = thread::Builder::new()
            .name("cdx-input-writer".into())
            .spawn(move || persist_events(writer, receiver, writer_running))?;
        Ok(Self {
            running,
            sender,
            hook_thread: Some(hook_thread),
            writer_thread: Some(writer_thread),
        })
    }

    /// Flushes the worker-side semantic line buffer without doing work on the
    /// low-level hook thread. Turn boundaries use this to keep text scoped to
    /// the turn in which it was observed.
    pub fn flush_pending_text(&self) -> Result<()> {
        self.sender
            .send_timeout(RawInput::Flush, Duration::from_millis(100))
            .context("input worker did not accept the text flush")
    }

    pub fn stop(mut self) -> Result<()> {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.hook_thread.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("input hook thread panicked"))??;
        }
        if let Some(slot) = CALLBACK_SENDER.get() {
            *slot.lock() = None;
        }
        if let Some(handle) = self.writer_thread.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("input writer thread panicked"))?;
        }
        Ok(())
    }
}

fn install_hooks(running: std::sync::Arc<AtomicBool>) -> Result<()> {
    let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
    let instance = HINSTANCE(module.0);
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_callback), Some(instance), 0) }
        .context("failed to install the low-level mouse hook")?;
    let keyboard =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_callback), Some(instance), 0) }
            .context("failed to install the low-level keyboard hook")?;
    pump_messages(running);
    unsafe {
        let _ = UnhookWindowsHookEx(mouse);
        let _ = UnhookWindowsHookEx(keyboard);
    }
    Ok(())
}

fn pump_messages(running: std::sync::Arc<AtomicBool>) {
    let mut message = MSG::default();
    while running.load(Ordering::Acquire) {
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    return;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn persist_events(
    writer: std::sync::Arc<SessionWriter>,
    receiver: Receiver<RawInput>,
    running: std::sync::Arc<AtomicBool>,
) {
    let mut last_move = i64::MIN / 2;
    let mut button_state = 0_u8;
    let mut keyboard_state = [0_u8; 256];
    let mut text_assembler = TextAssembler::default();
    while running.load(Ordering::Acquire) || !receiver.is_empty() {
        let Ok(event) = receiver.recv_timeout(Duration::from_millis(25)) else {
            continue;
        };
        match event {
            RawInput::Flush => persist_semantic(&writer, text_assembler.flush()),
            RawInput::Mouse {
                at,
                message,
                x,
                y,
                data,
                flags,
            } => {
                if message == WM_MOUSEMOVE && at.saturating_sub(last_move) < 166_667 {
                    continue;
                }
                if message == WM_MOUSEMOVE {
                    last_move = at;
                }
                let kind = mouse_kind(message);
                if kind != "pointer_move" {
                    persist_semantic(&writer, text_assembler.flush());
                }
                match message {
                    WM_LBUTTONDOWN => button_state |= 1,
                    WM_LBUTTONUP => button_state &= !1,
                    WM_RBUTTONDOWN => button_state |= 2,
                    WM_RBUTTONUP => button_state &= !2,
                    WM_MBUTTONDOWN => button_state |= 4,
                    WM_MBUTTONUP => button_state &= !4,
                    _ => {}
                }
                let injected = (flags & LLMHF_INJECTED) != 0;
                let offset = at.saturating_sub(writer.origin_100ns);
                let (tool_use_id, confidence) = writer
                    .correlate_requested_action(offset)
                    .unwrap_or((None, None));
                let (window_title, window_pid) = foreground_metadata();
                let public = public_input_event(
                    kind,
                    json!({
                        "x": x, "y": y, "message": message, "button_or_wheel_data": data,
                        "button_state": button_state, "flags": flags, "injected": injected,
                        "foreground_window_title": window_title, "foreground_process_id": window_pid
                    }),
                );
                let _ = writer.add_event(
                    offset,
                    "os_input",
                    kind,
                    &format!("Mouse {kind} at {x},{y}"),
                    confidence,
                    tool_use_id.as_deref(),
                    &public,
                    None,
                );
            }
            RawInput::Keyboard {
                at,
                message,
                vk,
                scan,
                flags,
            } => {
                let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
                let kind = if down { "key_down" } else { "key_up" };
                let injected = (flags & LLKHF_INJECTED.0) != 0;
                let full = serde_json::to_vec(&json!({
                    "message": message, "virtual_key": vk, "scan_code": scan,
                    "flags": flags, "injected": injected
                }))
                .ok();
                let public = public_input_event(kind, json!({ "injected": injected }));
                let offset = at.saturating_sub(writer.origin_100ns);
                let (tool_use_id, confidence) = writer
                    .correlate_requested_action(offset)
                    .unwrap_or((None, None));
                let _ = writer.add_event(
                    offset,
                    "os_input",
                    kind,
                    if down {
                        "Keyboard key down"
                    } else {
                        "Keyboard key up"
                    },
                    confidence,
                    tool_use_id.as_deref(),
                    &public,
                    full.as_deref(),
                );
                update_keyboard_state(&mut keyboard_state, vk, down);
                if down {
                    let meaning = key_meaning(vk, scan, &keyboard_state);
                    persist_semantic(&writer, text_assembler.feed(meaning, offset));
                }
            }
        }
    }
    persist_semantic(&writer, text_assembler.flush());
}

fn persist_semantic(writer: &SessionWriter, events: Vec<SemanticInput>) {
    for event in events {
        match event {
            SemanticInput::TextLine {
                text,
                start_offset_100ns,
                end_offset_100ns,
            } => {
                let public = json!({
                    "redacted": true,
                    "text_length": text.chars().count(),
                    "start_offset_100ns": start_offset_100ns,
                    "end_offset_100ns": end_offset_100ns
                });
                let (tool_use_id, confidence) = writer
                    .correlate_requested_action(end_offset_100ns)
                    .unwrap_or((None, None));
                let _ = writer.add_event(
                    end_offset_100ns,
                    "semantic_input",
                    "text_line",
                    "Text line entered",
                    confidence,
                    tool_use_id.as_deref(),
                    &public,
                    Some(text.as_bytes()),
                );
            }
            SemanticInput::Command { name, offset_100ns } => {
                let public = json!({ "command": name });
                let (tool_use_id, confidence) = writer
                    .correlate_requested_action(offset_100ns)
                    .unwrap_or((None, None));
                let _ = writer.add_event(
                    offset_100ns,
                    "semantic_input",
                    "key_command",
                    &format!("{name} pressed"),
                    confidence,
                    tool_use_id.as_deref(),
                    &public,
                    None,
                );
            }
        }
    }
}

fn update_keyboard_state(state: &mut [u8; 256], vk: u32, down: bool) {
    let Some(slot) = state.get_mut(vk as usize) else {
        return;
    };
    if down {
        *slot |= 0x80;
        if vk == 0x14 {
            *slot ^= 0x01;
        }
    } else {
        *slot &= 0x7f;
    }
}

fn key_meaning(vk: u32, scan: u32, keyboard_state: &[u8; 256]) -> KeyMeaning {
    if is_modifier(vk) {
        return KeyMeaning::Modifier;
    }
    if vk == 0x08 {
        return KeyMeaning::Backspace;
    }
    if let Some(name) = command_name(vk) {
        return KeyMeaning::Command(name.into());
    }
    let ctrl = key_down(keyboard_state, 0x11)
        || key_down(keyboard_state, 0xa2)
        || key_down(keyboard_state, 0xa3);
    let alt = key_down(keyboard_state, 0x12)
        || key_down(keyboard_state, 0xa4)
        || key_down(keyboard_state, 0xa5);
    let win = key_down(keyboard_state, 0x5b) || key_down(keyboard_state, 0x5c);
    if ctrl || alt || win {
        let mut parts = Vec::new();
        if ctrl {
            parts.push("Ctrl".to_owned());
        }
        if alt {
            parts.push("Alt".to_owned());
        }
        if win {
            parts.push("Win".to_owned());
        }
        parts.push(simple_key_name(vk));
        return KeyMeaning::Command(parts.join("+"));
    }
    let mut buffer = [0_u16; 8];
    let window = unsafe { GetForegroundWindow() };
    let thread_id = if window.is_invalid() {
        0
    } else {
        unsafe { GetWindowThreadProcessId(window, None) }
    };
    let layout = unsafe { GetKeyboardLayout(thread_id) };
    let length = unsafe { ToUnicodeEx(vk, scan, keyboard_state, &mut buffer, 0, Some(layout)) };
    if length > 0 {
        String::from_utf16_lossy(&buffer[..length as usize])
            .chars()
            .next()
            .map(KeyMeaning::Printable)
            .unwrap_or(KeyMeaning::Ignore)
    } else {
        KeyMeaning::Ignore
    }
}

fn key_down(state: &[u8; 256], vk: usize) -> bool {
    state.get(vk).is_some_and(|value| value & 0x80 != 0)
}

fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5b | 0x5c | 0xa0..=0xa5)
}

fn command_name(vk: u32) -> Option<&'static str> {
    match vk {
        0x09 => Some("Tab"),
        0x0d => Some("Enter"),
        0x1b => Some("Escape"),
        0x21 => Some("Page Up"),
        0x22 => Some("Page Down"),
        0x23 => Some("End"),
        0x24 => Some("Home"),
        0x25 => Some("Left Arrow"),
        0x26 => Some("Up Arrow"),
        0x27 => Some("Right Arrow"),
        0x28 => Some("Down Arrow"),
        0x2d => Some("Insert"),
        0x2e => Some("Delete"),
        0x70..=0x87 => Some(match vk - 0x70 {
            0 => "F1",
            1 => "F2",
            2 => "F3",
            3 => "F4",
            4 => "F5",
            5 => "F6",
            6 => "F7",
            7 => "F8",
            8 => "F9",
            9 => "F10",
            10 => "F11",
            11 => "F12",
            12 => "F13",
            13 => "F14",
            14 => "F15",
            15 => "F16",
            16 => "F17",
            17 => "F18",
            18 => "F19",
            19 => "F20",
            20 => "F21",
            21 => "F22",
            22 => "F23",
            _ => "F24",
        }),
        _ => None,
    }
}

fn simple_key_name(vk: u32) -> String {
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5a).contains(&vk) {
        char::from_u32(vk).unwrap_or('?').to_string()
    } else {
        command_name(vk)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("Key {vk}"))
    }
}

fn foreground_metadata() -> (Option<String>, Option<u32>) {
    let window = unsafe { GetForegroundWindow() };
    if window.is_invalid() {
        return (None, None);
    }
    let mut title = [0_u16; 512];
    let length = unsafe { GetWindowTextW(window, &mut title) };
    let title = (length > 0).then(|| String::from_utf16_lossy(&title[..length as usize]));
    let mut pid = 0_u32;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut pid));
    }
    (title, (pid != 0).then_some(pid))
}

fn mouse_kind(message: u32) -> &'static str {
    match message {
        WM_MOUSEMOVE => "pointer_move",
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => "button_down",
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => "button_up",
        WM_MOUSEWHEEL => "wheel",
        _ => "mouse_other",
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyMeaning, SemanticInput, TextAssembler, key_meaning};

    #[test]
    fn text_assembler_applies_backspace_and_flushes_line_before_enter() {
        let mut assembler = TextAssembler::default();
        let mut output = Vec::new();
        output.extend(assembler.feed(KeyMeaning::Printable('H'), 100));
        output.extend(assembler.feed(KeyMeaning::Printable('i'), 200));
        output.extend(assembler.feed(KeyMeaning::Backspace, 300));
        output.extend(assembler.feed(KeyMeaning::Printable('o'), 400));
        output.extend(assembler.feed(KeyMeaning::Command("Enter".into()), 500));

        assert_eq!(
            output,
            vec![
                SemanticInput::TextLine {
                    text: "Ho".into(),
                    start_offset_100ns: 100,
                    end_offset_100ns: 400,
                },
                SemanticInput::Command {
                    name: "Enter".into(),
                    offset_100ns: 500,
                },
            ]
        );
    }

    #[test]
    fn modifier_combinations_and_function_keys_become_commands() {
        let mut state = [0_u8; 256];
        state[0x11] = 0x80;
        state[0x56] = 0x80;
        assert_eq!(
            key_meaning(0x56, 0, &state),
            KeyMeaning::Command("Ctrl+V".into())
        );
        assert_eq!(
            key_meaning(0x70, 0, &[0; 256]),
            KeyMeaning::Command("F1".into())
        );
    }
}
