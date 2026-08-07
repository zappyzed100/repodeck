//! `WM_DISPLAYCHANGE` を受け取るためだけの、RepoDeck 自前の不可視ウィンドウ。
//!
//! 以前はこれを Slint の設定ウィンドウの subclass で実装していた。設定
//! ウィンドウはプロセスと同じ寿命だから安全、という理屈だったが——winit は
//! ウィンドウを最初の `show()` まで遅延生成するので、一度も開かれていない
//! 設定ウィンドウには HWND が無い。`SetWindowLongPtrW` の相手が居らず、
//! 登録は毎回黙って失敗していた (2026-07-24 に実機ログで確認: 起動から
//! 一度も `WM_DISPLAYCHANGE` を処理しておらず、`runtime.json` の
//! `last_seen_monitor_fingerprint` はずっと `null` のままだった)。
//!
//! そこで監視対象のウィンドウを他人から借りるのをやめ、自分で作る。
//! `WM_DISPLAYCHANGE` はすべての *トップレベル* ウィンドウへブロードキャスト
//! されるので、表示しないままでも届く (メッセージ専用ウィンドウ
//! `HWND_MESSAGE` はブロードキャストを受け取れないため、それは使わない)。
//!
//! 生成も破棄も UI スレッドで行うこと。メッセージは Slint のイベントループが
//! そのままディスパッチするので、`on_change` は `Rc` を掴んで構わない。

use std::cell::RefCell;
use std::collections::HashMap;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HMENU, RegisterClassW, WINDOW_EX_STYLE,
    WM_DISPLAYCHANGE, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{HSTRING, PCWSTR};

const WINDOW_CLASS_NAME: &str = "RepoDeckDisplayWatch";

thread_local! {
    /// HWND → コールバック。ウィンドウはこのスレッドでしか作らないので
    /// 同期は要らない。
    static WATCHERS: RefCell<HashMap<isize, Box<dyn Fn()>>> = RefCell::new(HashMap::new());
    /// ウィンドウクラスの登録は一度だけ。
    static CLASS_REGISTERED: RefCell<bool> = const { RefCell::new(false) };
}

unsafe extern "system" fn watch_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DISPLAYCHANGE {
        // コールバックの実行中に `WATCHERS` を borrow したままにしない
        // (`on_change` から監視を張り直せるように)。
        let callback = WATCHERS.with(|map| {
            map.borrow()
                .get(&(hwnd.0 as isize))
                .map(|c| std::ptr::from_ref(c.as_ref()))
        });
        if let Some(callback) = callback {
            // SAFETY: `callback` は `WATCHERS` が所有する `Box` の中身を指す。
            // この関数は UI スレッド上で同期的に呼ばれ、その間に当該
            // エントリを消せるのは同じスレッドだけ (= この呼び出しより後)。
            unsafe { (*callback)() };
        }
    }

    // SAFETY: 自前のウィンドウクラスなので、既定の処理へ渡すのが正しい。
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 監視を張っている間だけ生かしておくガード。drop でウィンドウを破棄する。
pub struct DisplayChangeWatch {
    hwnd: HWND,
}

impl Drop for DisplayChangeWatch {
    fn drop(&mut self) {
        WATCHERS.with(|map| {
            map.borrow_mut().remove(&(self.hwnd.0 as isize));
        });
        // SAFETY: `hwnd` は `watch_display_changes` がこのスレッドで作った
        // ウィンドウで、まだ破棄していない。
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn ensure_class_registered() -> bool {
    CLASS_REGISTERED.with(|registered| {
        if *registered.borrow() {
            return true;
        }
        // SAFETY: 自プロセスのモジュールハンドルを取るだけ。
        let Ok(instance) = (unsafe { GetModuleHandleW(PCWSTR::null()) }) else {
            return false;
        };
        let class_name = HSTRING::from(WINDOW_CLASS_NAME);
        let class = WNDCLASSW {
            lpfnWndProc: Some(watch_window_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // SAFETY: `class` のポインタ類は `class_name`/`instance` が生きている
        // この呼び出しの間だけ参照される。
        let atom = unsafe { RegisterClassW(&class) };
        if atom == 0 {
            return false;
        }
        *registered.borrow_mut() = true;
        true
    })
}

/// `WM_DISPLAYCHANGE` を受け取る不可視のトップレベルウィンドウを作り、
/// 届くたびに `on_change` を呼ぶ。作成に失敗したら `None`。
///
/// UI スレッドから呼ぶこと。
pub fn watch_display_changes(on_change: impl Fn() + 'static) -> Option<DisplayChangeWatch> {
    if !ensure_class_registered() {
        return None;
    }
    let class_name = HSTRING::from(WINDOW_CLASS_NAME);

    // SAFETY: 登録済みのクラス名と NUL 終端文字列を渡すだけ。表示しないので
    // `WS_VISIBLE` は付けない (ブロードキャストは不可視でも届く)。
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None::<HMENU>,
            None,
            None,
        )
    }
    .ok()?;

    WATCHERS.with(|map| {
        map.borrow_mut()
            .insert(hwnd.0 as isize, Box::new(on_change));
    });

    Some(DisplayChangeWatch { hwnd })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn creates_a_window_that_receives_broadcast_messages() {
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SendMessageW};

        let hits = Rc::new(std::cell::Cell::new(0));
        let counter = hits.clone();
        let watch = watch_display_changes(move || counter.set(counter.get() + 1))
            .expect("the watch window should be creatable");

        // SAFETY: `watch.hwnd` は今作ったばかりの自前ウィンドウ。
        unsafe {
            assert!(IsWindow(Some(watch.hwnd)).as_bool());
            SendMessageW(watch.hwnd, WM_DISPLAYCHANGE, None, None);
        }
        assert_eq!(hits.get(), 1, "WM_DISPLAYCHANGE should reach the callback");

        let hwnd = watch.hwnd;
        drop(watch);
        // SAFETY: 破棄済みかどうかを問い合わせるだけ。
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
    }
}
