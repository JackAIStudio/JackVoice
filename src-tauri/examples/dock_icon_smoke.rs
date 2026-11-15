//! macOS 原生回归检查：模拟录音状态，不启动麦克风、不读业务数据。
//! cargo run --manifest-path src-tauri/Cargo.toml --example dock_icon_smoke

#[cfg(target_os = "macos")]
use objc2::{AnyThread, MainThreadMarker};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSBitmapImageFileType, NSBitmapImageRep, NSImage};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSData, NSDictionary};
#[cfg(target_os = "macos")]
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
#[cfg(target_os = "macos")]
use tauri::{Emitter, Manager};

// 仅用于原生验收的模拟状态，不启动麦克风、不访问用户文件或识别服务。
#[cfg(target_os = "macos")]
mod session {
    #[derive(Clone, serde::Serialize)]
    pub struct UiState {
        pub phase: String,
        pub input_signal: crate::input_signal::InputSignal,
    }
    pub struct AppState(pub parking_lot::Mutex<UiState>);
    impl AppState {
        pub fn snapshot(&self) -> UiState {
            self.0.lock().clone()
        }
    }
}
#[cfg(target_os = "macos")]
#[path = "../src/dock_icon.rs"]
mod dock_icon;
#[cfg(target_os = "macos")]
#[path = "../src/input_signal.rs"]
#[allow(dead_code)] // 原生图标验收直接注入状态；音频分类器由其单元测试覆盖。
mod input_signal;
#[cfg(target_os = "macos")]
use input_signal::InputSignal;

#[cfg(target_os = "macos")]
static PASSED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
fn set_state(app: &tauri::AppHandle, phase: &str, signal: InputSignal) {
    let app_for_main = app.clone();
    let phase = phase.to_string();
    app.run_on_main_thread(move || {
        let state = app_for_main.state::<session::AppState>();
        let mut ui = state.0.lock();
        ui.phase = phase;
        ui.input_signal = signal;
        // 和真实应用一样，在锁内发事件，覆盖主线程重入死锁的风险。
        app_for_main.emit("jackvoice://state", ui.clone()).unwrap();
    })
    .unwrap();
}

#[cfg(target_os = "macos")]
fn native_icon(app: &tauri::AppHandle, label: &str) -> Vec<u8> {
    let (tx, rx) = mpsc::channel();
    app.run_on_main_thread(move || {
        let mtm = MainThreadMarker::new().unwrap();
        let image = NSApplication::sharedApplication(mtm)
            .applicationIconImage()
            .unwrap();
        let bitmap = NSBitmapImageRep::initWithData(
            NSBitmapImageRep::alloc(),
            &image.TIFFRepresentation().unwrap(),
        )
        .unwrap();
        let bytes = unsafe {
            bitmap.representationUsingType_properties(
                NSBitmapImageFileType::PNG,
                &NSDictionary::new(),
            )
        }
        .unwrap()
        .to_vec();
        tx.send(bytes).unwrap();
    })
    .unwrap();
    let bytes = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("主线程应及时响应");
    let directory = std::env::var_os("JACKVOICE_DOCK_ICON_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("jackvoice-dock-icon-check"));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{label}.png")), &bytes).unwrap();
    tauri::image::Image::from_bytes(&bytes)
        .unwrap()
        .rgba()
        .to_vec()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Dock 图标原生检查仅适用于 macOS。");
}

#[cfg(target_os = "macos")]
fn main() {
    std::env::set_var("JACKVOICE_DOCK_ICON_TRACE", "1");
    tauri::Builder::default()
        .manage(session::AppState(parking_lot::Mutex::new(session::UiState { phase: "idle".into(), input_signal: InputSignal::Quiet })))
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap()
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Ready) {
                let mtm = MainThreadMarker::new().unwrap();
                let original = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(include_bytes!("../icons/icon.icns"))).unwrap();
                unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&original)); }
                dock_icon::setup(app);
                let app = app.clone();
                std::thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let original = native_icon(&app, "idle");
                        set_state(&app, "recording", InputSignal::Quiet);
                        std::thread::sleep(Duration::from_millis(150));
                        let quiet = native_icon(&app, "recording-quiet");
                        assert!(original != quiet, "等待声音也必须区别于待机");
                        for _ in 0..3 {
                            set_state(&app, "recording", InputSignal::Quiet);
                            std::thread::sleep(Duration::from_millis(125));
                        }
                        assert!(quiet == native_icon(&app, "quiet-still"), "无声时必须停止动画");
                        set_state(&app, "recording", InputSignal::Sound);
                        std::thread::sleep(Duration::from_millis(125));
                        let sound = native_icon(&app, "sound-a");
                        set_state(&app, "recording", InputSignal::Sound);
                        std::thread::sleep(Duration::from_millis(125));
                        assert!(sound != native_icon(&app, "sound-b"), "有声时应大幅波动");
                        set_state(&app, "recording", InputSignal::Silent);
                        std::thread::sleep(Duration::from_millis(125));
                        let silent = native_icon(&app, "silent-warning");
                        assert!(silent != quiet && silent != sound, "持续无声应显示独立提示图标");
                        set_state(&app, "recording", InputSignal::Disconnected);
                        std::thread::sleep(Duration::from_millis(125));
                        let disconnected = native_icon(&app, "disconnected");
                        assert!(disconnected != silent && disconnected != quiet, "断连应显示独立图标");
                        set_state(&app, "finalizing", InputSignal::Quiet);
                        std::thread::sleep(Duration::from_millis(150));
                        assert!(original == native_icon(&app, "restored"), "停止后必须恢复原图");
                        for _ in 0..40 { set_state(&app, "recording", InputSignal::Sound); }
                        set_state(&app, "idle", InputSignal::Quiet);
                        std::thread::sleep(Duration::from_millis(200));
                        assert!(original == native_icon(&app, "after-burst"), "积压事件不能覆盖最终停止状态");
                        println!("PASS: AppKit 实际图标完成录音切换、静音停止、无声提醒、断连提示、停止恢复和积压事件检查");
                    }));
                    PASSED.store(result.is_ok(), Ordering::Release);
                    app.exit(if result.is_ok() { 0 } else { 1 });
                });
            }
        });
    if !PASSED.load(Ordering::Acquire) {
        std::process::exit(1);
    }
}
