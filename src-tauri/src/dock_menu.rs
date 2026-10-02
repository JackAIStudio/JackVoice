//! macOS 程序坞（Dock）右键菜单。
//!
//! 设置窗口里已经有「实时预览胶囊 → 移到屏幕中央」，但胶囊丢了的时候，
//! 用户往往连设置窗口都不想翻。这里把同一个动作放进程序坞右键菜单，
//! 右击 Dock 图标就能把胶囊叫回当前屏幕中央。
//!
//! Tauri 只提供应用菜单栏（`set_menu`）和托盘菜单，没有程序坞菜单 API，
//! 因此这里实现 AppKit 文档里的委托方法 `applicationDockMenu:`：
//! tao 的 delegate 类在编译期就定好了，只能在运行期把该方法注入它的类，
//! 再重新 setDelegate 一次，让 AppKit 重新读取 delegate 实现了哪些方法。

use std::sync::OnceLock;
use tauri::AppHandle;

/// 程序坞菜单项标题。
pub const MENU_ITEM_TITLE: &str = "将预览胶囊移到屏幕中央";

/// 菜单项点击后要执行的动作。存成闭包，Objective-C 那边就不必知道 Tauri 的类型。
static RECENTER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// 把「将预览胶囊移到屏幕中央」挂到程序坞右键菜单上。非 macOS 平台为空实现。
pub fn setup(app: &AppHandle) {
    let app = app.clone();
    let _ = RECENTER.set(Box::new(move || {
        if let Err(error) = crate::overlay::reset_overlay_position(app.clone()) {
            eprintln!("[dock-menu] 将预览胶囊移到屏幕中央失败：{error}");
        }
    }));

    #[cfg(target_os = "macos")]
    imp::install();
}

#[cfg(target_os = "macos")]
mod imp {
    use std::mem::transmute;
    use std::ptr;
    use std::sync::atomic::{AtomicPtr, Ordering};
    use std::sync::Once;

    use objc2::ffi::class_addMethod;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::{NSObject, NSString};

    use super::{MENU_ITEM_TITLE, RECENTER};

    /// 程序坞菜单只在弹出期间被 AppKit 持有，菜单项对 target 又是弱引用，
    /// 所以菜单和 target 都要自己留到进程结束。
    static DOCK_MENU: AtomicPtr<NSMenu> = AtomicPtr::new(ptr::null_mut());
    static INSTALL: Once = Once::new();

    define_class!(
        /// 程序坞菜单项的 target：点击后回到 Rust 侧。
        #[unsafe(super(NSObject))]
        #[name = "JackVoiceDockMenuTarget"]
        #[thread_kind = MainThreadOnly]
        struct DockMenuTarget;

        impl DockMenuTarget {
            #[unsafe(method(recenterCapsule:))]
            fn recenter_capsule(&self, _sender: Option<&AnyObject>) {
                if let Some(recenter) = RECENTER.get() {
                    recenter();
                }
            }
        }
    );

    type ApplicationDockMenuFn =
        unsafe extern "C-unwind" fn(&AnyObject, Sel, &NSApplication) -> *mut NSMenu;

    /// `NSApplicationDelegate.applicationDockMenu:`：AppKit 每次弹出程序坞菜单都会问一次。
    unsafe extern "C-unwind" fn application_dock_menu(
        _this: &AnyObject,
        _cmd: Sel,
        _sender: &NSApplication,
    ) -> *mut NSMenu {
        eprintln!("[dock-menu] 程序坞右键菜单已弹出");
        DOCK_MENU.load(Ordering::Acquire)
    }

    /// 主线程安装，重复调用只生效一次。
    pub fn install() {
        INSTALL.call_once(|| {
            let Some(mtm) = MainThreadMarker::new() else {
                eprintln!("[dock-menu] 程序坞菜单只能在主线程安装，跳过");
                return;
            };
            let ns_app = NSApplication::sharedApplication(mtm);
            if !unsafe { inject_delegate_method(&ns_app) } {
                return;
            }
            let menu = unsafe { build_menu(mtm) };
            DOCK_MENU.store(Retained::into_raw(menu), Ordering::Release);
            eprintln!("[dock-menu] 程序坞右键菜单已安装：{MENU_ITEM_TITLE}");
        });
    }

    /// 把 `applicationDockMenu:` 注入 NSApp 当前的 delegate 类。返回是否可以继续装菜单。
    unsafe fn inject_delegate_method(ns_app: &NSApplication) -> bool {
        let delegate: Option<Retained<AnyObject>> = unsafe { msg_send![ns_app, delegate] };
        let Some(delegate) = delegate else {
            eprintln!("[dock-menu] NSApplication 还没有 delegate，跳过程序坞菜单");
            return false;
        };

        let class = delegate.class() as *const AnyClass as *mut AnyClass;
        let imp: Imp = unsafe { transmute(application_dock_menu as ApplicationDockMenuFn) };
        let added =
            unsafe { class_addMethod(class, sel!(applicationDockMenu:), imp, c"@@:@".as_ptr()) }
                .as_bool();

        if !added {
            eprintln!("[dock-menu] NSApplication delegate 已有 applicationDockMenu:，不覆盖");
            return false;
        }

        // delegate 是在注入之前设置的，重新设置一次让 AppKit 重新读取它的方法表。
        let _: () = unsafe { msg_send![ns_app, setDelegate: &*delegate] };
        true
    }

    unsafe fn build_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);

        let target: Retained<DockMenuTarget> =
            unsafe { msg_send![super(mtm.alloc::<DockMenuTarget>().set_ivars(())), init] };

        let title = NSString::from_str(MENU_ITEM_TITLE);
        let key_equivalent = NSString::from_str("");
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &title,
                Some(sel!(recenterCapsule:)),
                &key_equivalent,
            )
        };
        unsafe { item.setTarget(Some(&target)) };
        item.setEnabled(true);
        menu.addItem(&item);

        // 菜单项对 target 是弱引用；菜单本身也只在弹出期间被 AppKit 持有。
        let _ = Retained::into_raw(target);
        menu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_item_title_matches_settings_wording() {
        assert_eq!(MENU_ITEM_TITLE, "将预览胶囊移到屏幕中央");
    }

    /// 非主线程调用不能崩：macOS 上安静跳过（cargo test 的测试线程不是主线程）。
    #[cfg(target_os = "macos")]
    #[test]
    fn install_off_the_main_thread_is_a_no_op() {
        assert!(
            objc2::MainThreadMarker::new().is_none(),
            "测试线程不应被当成主线程"
        );
        imp::install();
        imp::install();
    }
}
