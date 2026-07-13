use std::sync::Arc;

use anyhow::Context as _;
use azookey_server::TonicNamedPipeServer;
use ipc::{WindowAction, WindowController, WindowService};
use shared::proto::window_service_server::WindowServiceServer;
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
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

    let proxy_clone = event_loop_proxy.clone();
    let candidate_window = candidate::create_candidate_window(&event_loop)?;
    let candidate_webview_builder = candidate::create_candidate_webview()?;
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
    let indicator_webview = indicator::create_indicator_webview(&indicator_window)?;

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

    let mut candidate_anchor = None;
    event_loop.run(move |event, _, control_flow| {
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

        match event {
            Event::NewEvents(StartCause::Init) => {}
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::UserEvent(script) => match script {
                UserEvent::UpdateHeight(height) => {
                    let width = candidate_window.inner_size().width as i32;
                    candidate_window.set_inner_size(LogicalSize::new(width, height));
                }
                UserEvent::WindowAction(action) => match action {
                    WindowAction::Show => {
                        show_candidate_window();
                    }
                    WindowAction::Hide => {
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
                        candidate_anchor = Some((top, left, bottom, right));
                        position_windows(top, left, bottom, right);
                    }
                    WindowAction::SetCandidate { candidates } => {
                        let height = candidate_window.inner_size().height as i32;
                        candidate_window.set_inner_size(PhysicalSize::new(
                            candidate_window_width(&candidates),
                            height as u32,
                        ));
                        if let Some((top, left, bottom, right)) = candidate_anchor {
                            position_windows(top, left, bottom, right);
                        }

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
                        show_candidate_window();
                        let height = candidate_window.inner_size().height as i32;
                        candidate_window.set_inner_size(PhysicalSize::new(
                            candidate_window_width(&candidates),
                            height as u32,
                        ));
                        if let Some((top, left, bottom, right)) = candidate_anchor {
                            position_windows(top, left, bottom, right);
                        }

                        let candidates = serde_json::to_string(&candidates)
                            .context("Failed to serialize candidates")
                            .unwrap();
                        candidate_webview
                            .evaluate_script(&format!(
                                "updateCandidateState({candidates}, {selection})"
                            ))
                            .unwrap();
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
}
