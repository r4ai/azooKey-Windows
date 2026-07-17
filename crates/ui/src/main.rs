use std::{ffi::OsString, path::PathBuf, sync::Arc};

use anyhow::Context as _;
use azookey_server::TonicNamedPipeServer;
use ipc::{WindowAction, WindowController, WindowService};
use shared::proto::window_service_server::WindowServiceServer;
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::platform::windows::{EventLoopBuilderExtWindows, WindowExtWindows};
use tao::{
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tonic::transport::Server;
use uiaccess::prepare_uiaccess_token;
use utils::get_candidate_window_position;
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE},
};
use wry::WebContext;

pub mod candidate;
pub mod indicator;
pub mod ipc;
pub mod uiaccess;
pub mod utils;

#[derive(Debug)]
pub enum UserEvent {
    UpdateHeight(i32),
    WindowAction(WindowAction),
}

const CANDIDATE_WINDOW_MIN_WIDTH: u32 = 225;
const CANDIDATE_WINDOW_MAX_WIDTH: u32 = 720;
const CANDIDATE_WINDOW_BASE_WIDTH: u32 = 120;
const CANDIDATE_CHARACTER_WIDTH: u32 = 18;

fn webview_data_directory_from_local_app_data(
    local_app_data: Option<OsString>,
) -> anyhow::Result<PathBuf> {
    let local_app_data = local_app_data
        .filter(|value| !value.is_empty())
        .context("LOCALAPPDATA is unavailable for the WebView2 data directory")?;
    Ok(PathBuf::from(local_app_data)
        .join("Azookey")
        .join("WebView2"))
}

fn create_webview_context() -> anyhow::Result<WebContext> {
    let data_directory =
        webview_data_directory_from_local_app_data(std::env::var_os("LOCALAPPDATA"))?;
    std::fs::create_dir_all(&data_directory).with_context(|| {
        format!(
            "Failed to create the WebView2 data directory at {}",
            data_directory.display()
        )
    })?;
    Ok(WebContext::new(Some(data_directory)))
}

fn candidate_window_width(candidates: &[String]) -> u32 {
    let measured_character_limit = ((CANDIDATE_WINDOW_MAX_WIDTH - CANDIDATE_WINDOW_BASE_WIDTH)
        / CANDIDATE_CHARACTER_WIDTH
        + 1) as usize;
    let max_len = candidates
        .iter()
        .map(|candidate| candidate.chars().take(measured_character_limit).count())
        .max()
        .unwrap_or(0);
    let estimated = CANDIDATE_WINDOW_BASE_WIDTH.saturating_add(
        u32::try_from(max_len)
            .unwrap_or(u32::MAX)
            .saturating_mul(CANDIDATE_CHARACTER_WIDTH),
    );

    estimated.clamp(CANDIDATE_WINDOW_MIN_WIDTH, CANDIDATE_WINDOW_MAX_WIDTH)
}

#[derive(Debug)]
struct CandidateWindowState {
    width: u32,
    content_height: Option<u32>,
    anchor: Option<(i32, i32, i32, i32)>,
    show_requested: bool,
}

impl CandidateWindowState {
    fn new(width: u32) -> Self {
        Self {
            width,
            content_height: None,
            anchor: None,
            show_requested: false,
        }
    }

    fn set_width(&mut self, width: u32) {
        self.width = width;
    }

    fn set_content_height(&mut self, height: u32) {
        if height > 0 {
            self.content_height = Some(height);
        }
    }

    fn set_anchor(&mut self, anchor: (i32, i32, i32, i32)) {
        self.anchor = Some(anchor);
    }

    fn anchor(&self) -> Option<(i32, i32, i32, i32)> {
        self.anchor
    }

    fn logical_size(&self) -> Option<(u32, u32)> {
        self.content_height.map(|height| (self.width, height))
    }

    fn request_show(&mut self) {
        self.show_requested = true;
    }

    fn should_show(&self) -> bool {
        self.show_requested && self.anchor.is_some() && self.content_height.is_some()
    }

    fn hide(&mut self) {
        self.show_requested = false;
        self.anchor = None;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // obtain uiaccess token
    prepare_uiaccess_token()?;

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event()
        .with_any_thread(true)
        .build();

    // initialize window controller
    let (tx, mut rx) = mpsc::channel(32);
    let window_controller = WindowController::new(tx.clone());
    let grpc_service = WindowService {
        controller: window_controller.clone(),
    };

    // start grpc server
    tokio::spawn(async move {
        println!("WindowServer listening");
        Server::builder()
            .add_service(WindowServiceServer::new(grpc_service))
            .serve_with_incoming(TonicNamedPipeServer::new("azookey_ui"))
            .await
            .expect("gRPC server failed");
    });

    let event_loop_proxy = event_loop.create_proxy();
    let task_guard: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let mut web_context = create_webview_context()?;

    let proxy_clone = event_loop_proxy.clone();
    let candidate_window = candidate::create_candidate_window(&event_loop)?;
    let candidate_webview_builder = candidate::create_candidate_webview(&mut web_context)?;
    let candidate_webview = candidate_webview_builder
        .with_devtools(true)
        .with_ipc_handler(move |message| {
            if let Ok(message) = serde_json::from_str::<serde_json::Value>(message.body()) {
                if let Some(type_value) = message.get("type") {
                    if type_value == "resize" {
                        if let Some(height) = message.get("height") {
                            let height = height.as_f64().unwrap_or(0.0);
                            proxy_clone
                                .send_event(UserEvent::UpdateHeight(height as i32))
                                .unwrap();
                        }
                    }
                }
            }
        })
        .build(&candidate_window)?;

    let indicator_window = indicator::create_indicator_window(&event_loop)?;
    let indicator_webview =
        indicator::create_indicator_webview(&indicator_window, &mut web_context)?;

    // handle window actions
    let proxy_clone = event_loop_proxy.clone();
    tokio::spawn(async move {
        while let Some(action) = rx.recv().await {
            if proxy_clone
                .send_event(UserEvent::WindowAction(action))
                .is_err()
            {
                break;
            }
        }
    });

    let mut candidate_state = CandidateWindowState::new(CANDIDATE_WINDOW_MIN_WIDTH);
    event_loop.run(move |event, _, control_flow| {
        // Wry requires the shared context to outlive every WebView that uses it.
        let _keep_web_context_alive = &web_context;
        *control_flow = ControlFlow::Wait;

        let indicator_hwnd = indicator_window.hwnd();
        let show_candidate_window = || {
            if let Ok(mut task_guard) = task_guard.try_lock() {
                if let Some(task) = task_guard.take() {
                    task.abort();
                    let _ = unsafe {
                        ShowWindow(HWND(indicator_hwnd as *mut std::ffi::c_void), SW_HIDE)
                    };
                }
            }

            let _ = unsafe {
                ShowWindow(
                    HWND(candidate_window.hwnd() as *mut std::ffi::c_void),
                    SW_SHOWNOACTIVATE,
                )
            };
        };
        let position_windows = |top, left, bottom, right| {
            let (x, y) = get_candidate_window_position(top, left, bottom, right, &candidate_window);

            unsafe {
                let _ = SetWindowPos(
                    HWND(candidate_window.hwnd() as *mut std::ffi::c_void),
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );

                let _ = SetWindowPos(
                    HWND(indicator_hwnd as *mut std::ffi::c_void),
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            candidate_window.set_outer_position(PhysicalPosition::new(x, y));
            indicator_window
                .set_outer_position(PhysicalPosition::new((left - 45) as f64, bottom as f64));
        };
        let apply_candidate_state = |state: &CandidateWindowState| {
            let Some((width, height)) = state.logical_size() else {
                return;
            };

            candidate_window.set_inner_size(LogicalSize::new(width, height));
            if let Some((top, left, bottom, right)) = state.anchor() {
                position_windows(top, left, bottom, right);
            }
            if state.should_show() {
                show_candidate_window();
            }
        };

        match event {
            Event::NewEvents(StartCause::Init) => {}
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::UserEvent(script) => match script {
                UserEvent::UpdateHeight(height) => {
                    if let Ok(height) = u32::try_from(height) {
                        candidate_state.set_content_height(height);
                        apply_candidate_state(&candidate_state);
                    }
                }
                UserEvent::WindowAction(action) => match action {
                    WindowAction::Show => {
                        candidate_state.request_show();
                        apply_candidate_state(&candidate_state);
                    }
                    WindowAction::Hide => {
                        candidate_state.hide();
                        let _ = unsafe {
                            ShowWindow(
                                HWND(candidate_window.hwnd() as *mut std::ffi::c_void),
                                SW_HIDE,
                            )
                        };
                    }
                    WindowAction::SetPosition {
                        top,
                        left,
                        bottom,
                        right,
                    } => {
                        candidate_state.set_anchor((top, left, bottom, right));
                        apply_candidate_state(&candidate_state);
                    }
                    WindowAction::SetCandidate { candidates } => {
                        candidate_state.set_width(candidate_window_width(&candidates));
                        apply_candidate_state(&candidate_state);

                        let candidates = serde_json::to_string(&candidates)
                            .context("Failed to serialize candidates")
                            .unwrap();
                        candidate_webview
                            .evaluate_script(&format!("updateCandidates({candidates})"))
                            .unwrap();
                    }
                    WindowAction::SetCandidateState {
                        candidates,
                        selection,
                    } => {
                        candidate_state.set_width(candidate_window_width(&candidates));
                        apply_candidate_state(&candidate_state);

                        let candidates = serde_json::to_string(&candidates)
                            .context("Failed to serialize candidates")
                            .unwrap();
                        candidate_webview
                            .evaluate_script(&format!(
                                "updateCandidateState({candidates}, {selection})"
                            ))
                            .unwrap();
                        candidate_state.request_show();
                        apply_candidate_state(&candidate_state);
                    }
                    WindowAction::SetSelection { index } => {
                        candidate_webview
                            .evaluate_script(&format!("updateSelection({index})"))
                            .unwrap();
                    }
                    WindowAction::SetInputMode(input_method) => {
                        let input_method = serde_json::to_string(&input_method)
                            .context("Failed to serialize input method")
                            .unwrap();
                        indicator_webview
                            .evaluate_script(&format!("updateInputMethod({input_method})"))
                            .unwrap();

                        let task_guard = task_guard.try_lock();

                        if let Ok(mut task_guard) = task_guard {
                            if let Some(task) = task_guard.take() {
                                task.abort();
                            }

                            *task_guard = Some(tokio::spawn(async move {
                                let _ = unsafe {
                                    ShowWindow(
                                        HWND(indicator_hwnd as *mut std::ffi::c_void),
                                        SW_SHOWNOACTIVATE,
                                    )
                                };
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                let _ = unsafe {
                                    ShowWindow(
                                        HWND(indicator_hwnd as *mut std::ffi::c_void),
                                        SW_HIDE,
                                    )
                                };
                            }));
                        }
                    }
                },
            },
            _ => (),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_width_is_bounded() {
        assert_eq!(candidate_window_width(&[]), CANDIDATE_WINDOW_MIN_WIDTH);
        assert_eq!(
            candidate_window_width(&["あ".repeat(10_000)]),
            CANDIDATE_WINDOW_MAX_WIDTH
        );
    }

    #[test]
    fn candidate_width_counts_characters_not_utf8_bytes() {
        let expected = (CANDIDATE_WINDOW_BASE_WIDTH + 10 * CANDIDATE_CHARACTER_WIDTH)
            .clamp(CANDIDATE_WINDOW_MIN_WIDTH, CANDIDATE_WINDOW_MAX_WIDTH);
        assert_eq!(candidate_window_width(&["あ".repeat(10)]), expected);
    }

    #[test]
    fn initial_show_waits_for_both_caret_position_and_webview_height() {
        let mut state = CandidateWindowState::new(CANDIDATE_WINDOW_MIN_WIDTH);

        state.request_show();
        assert!(!state.should_show());

        state.set_anchor((100, 200, 120, 220));
        assert!(!state.should_show());

        state.set_content_height(180);
        assert!(state.should_show());
        assert_eq!(
            state.logical_size(),
            Some((CANDIDATE_WINDOW_MIN_WIDTH, 180))
        );
    }

    #[test]
    fn hiding_the_window_drops_only_the_composition_scoped_anchor() {
        let mut state = CandidateWindowState::new(CANDIDATE_WINDOW_MIN_WIDTH);
        state.set_content_height(180);
        state.set_anchor((100, 200, 120, 220));
        state.request_show();
        assert!(state.should_show());

        state.hide();
        state.request_show();

        assert!(!state.should_show());
        assert!(state.anchor().is_none());
        assert_eq!(
            state.logical_size(),
            Some((CANDIDATE_WINDOW_MIN_WIDTH, 180))
        );
    }

    #[test]
    fn measured_height_and_candidate_width_share_logical_pixel_units() {
        let mut state = CandidateWindowState::new(CANDIDATE_WINDOW_MIN_WIDTH);
        state.set_content_height(180);
        state.set_width(360);

        assert_eq!(state.logical_size(), Some((360, 180)));
    }

    #[test]
    fn webview_data_is_stored_outside_the_install_directory() {
        let path = webview_data_directory_from_local_app_data(Some(OsString::from(
            r"C:\Users\tester\AppData\Local",
        )))
        .unwrap();

        assert_eq!(
            path,
            PathBuf::from(r"C:\Users\tester\AppData\Local\Azookey\WebView2")
        );
    }

    #[test]
    fn webview_data_requires_a_user_local_directory() {
        assert!(webview_data_directory_from_local_app_data(None).is_err());
        assert!(webview_data_directory_from_local_app_data(Some(OsString::new())).is_err());
    }
}
