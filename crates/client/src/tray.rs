//! Windows 托盘：Shell_NotifyIconW + 一个隐藏窗口的消息泵。
//!
//! 不用 winit/systray 那类框架：菜单只有五项（有更新时多一行），没有窗口、没有
//! 自绘，手写 Win32 让消息泵完全可控，也避免把主线程交给别的事件循环。
//!
//! 菜单在每次右键时按当前模型重建：托盘模型的唯一写入者是 2 秒一次的同步线程，
//! 而弹出菜单本来就不能在打开期间改项，重建比记 HMENU 句柄更简单。

use crate::{platform, APP_TITLE};
use mesh_engine::{Cmd, StateCell};
use mesh_text::id;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, LoadImageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu,
    TranslateMessage, HICON, HMENU, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE, MF_CHECKED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND,
    WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSEXW,
};

/// 托盘图标的字节（Go 的 client/build/windows/tray.ico，构建期嵌进来）。
const TRAY_ICON: &[u8] = include_bytes!("../assets/tray.ico");

/// 托盘回调消息（WM_APP 之后的一个）。
const CALLBACK_MESSAGE: u32 = WM_APP + 1;

/// 菜单项 id。顺序就是菜单顺序，只在本文件里有意义。
const ID_UPDATE: usize = 1;
const ID_PANEL: usize = 2;
const ID_DIR: usize = 3;
const ID_AUTOSTART: usize = 5;
const ID_QUIT: usize = 4;
/// 地址那一行不可点，id 只用于 AppendMenuW 占位。
const ID_ADDRESS: usize = 10;

/// 托盘模型：只有这三项会变，全部由同步线程写。
#[derive(Default)]
struct Model {
    local_ip: String,
    update_state: String,
    update_version: String,
}

struct Context {
    cell: Arc<StateCell>,
    /// mpsc 的 Sender 不是 Sync，而窗口过程要从任意消息里取它，所以包一层。
    tx: Mutex<Sender<Cmd>>,
    url: String,
    root: PathBuf,
    quit: Arc<dyn Fn() + Send + Sync>,
    model: Mutex<Model>,
    /// 开机自启的勾选状态。单独放一个原子量而不是塞进 Model：点击的处理在别的
    /// 线程里，改完要立刻被下一次右键读到。
    autostart: AtomicBool,
    tooltip: String,
    icon: AtomicIsize,
    taskbar_created: u32,
}

/// 一个进程只有一个托盘；窗口过程是静态函数，只能从这里取上下文。
static CONTEXT: OnceLock<Arc<Context>> = OnceLock::new();

pub(crate) struct TrayOptions {
    pub url: String,
    pub root: PathBuf,
    pub version: String,
    pub cell: Arc<StateCell>,
    pub tx: Sender<Cmd>,
    pub quit: Arc<dyn Fn() + Send + Sync>,
    /// 托盘真的立起来之后调用：启动器在等这一刻提交的 `bin/current`。
    pub ready: Box<dyn Fn() + Send + Sync>,
}

pub(crate) fn run(options: TrayOptions) -> Result<(), String> {
    let icon = load_tray_icon(TRAY_ICON)?;
    let taskbar_created =
        unsafe { RegisterWindowMessageW(platform::wide("TaskbarCreated").as_ptr()) };
    // 初值取快照：开机自启已经开着时，第一帧菜单就该是勾上的。
    let autostart = options.cell.get().snapshot.autostart_enabled;
    let context = Arc::new(Context {
        cell: options.cell,
        tx: Mutex::new(options.tx),
        url: options.url,
        root: options.root,
        quit: options.quit,
        model: Mutex::new(Model::default()),
        autostart: AtomicBool::new(autostart),
        tooltip: format!("{APP_TITLE} {}", options.version),
        icon: AtomicIsize::new(icon as isize),
        taskbar_created,
    });
    let _ = CONTEXT.set(Arc::clone(&context));

    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class_name = platform::wide("MeshClientTrayWindow");
    let window_name = platform::wide(APP_TITLE);
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: icon,
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: icon,
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err(mesh_text::format(
            id::CLIENT_ERROR_REGISTER_TRAY_CLASS,
            &[&std::io::Error::last_os_error()],
        ));
    }
    // 普通隐藏窗口（不加 WS_VISIBLE），不是 message-only 窗口：TaskbarCreated 是
    // 广播消息，message-only 窗口收不到，explorer 重启后图标就再也回不来。
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(mesh_text::format(
            id::CLIENT_ERROR_CREATE_TRAY_WINDOW,
            &[&std::io::Error::last_os_error()],
        ));
    }

    let watcher = Arc::clone(&context);
    thread::Builder::new()
        .name("mesh-tray-watch".to_string())
        .spawn(move || watch_state(&watcher))
        .map_err(|err| mesh_text::format(id::CLIENT_ERROR_START_TRAY_THREAD, &[&err]))?;
    allow_shell_messages(hwnd, taskbar_created);
    add_icon_with_retry(Arc::clone(&context), hwnd as usize);
    // 窗口、监听线程与托盘图标都就位了：这一版算能跑起来。
    (options.ready)();

    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    remove_icon(hwnd);
    Ok(())
}

/// 把状态同步到托盘模型；变化不频繁，2 秒一次足够。
fn watch_state(context: &Context) {
    loop {
        let snapshot = context.cell.get().snapshot.clone();
        if let Ok(mut model) = context.model.lock() {
            model.local_ip = snapshot.local_ip;
            model.update_state = snapshot.update_state;
            model.update_version = snapshot.update_version;
        }
        // 面板里改过之后也走这里同步，两个入口永远显示同一个状态。
        context
            .autostart
            .store(snapshot.autostart_enabled, Ordering::SeqCst);
        thread::sleep(Duration::from_secs(2));
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(context) = CONTEXT.get() else {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    };
    // explorer 重启后系统广播 TaskbarCreated：图标要重新加一遍。任务栏可能还没就绪，
    // 加不上就走重试线程。
    if message == context.taskbar_created {
        if !add_icon(context, hwnd) {
            add_icon_with_retry(Arc::clone(context), hwnd as usize);
        }
        return 0;
    }
    match message {
        CALLBACK_MESSAGE => {
            let mouse = lparam as u32;
            if mouse == WM_RBUTTONUP || mouse == WM_LBUTTONUP {
                show_menu(context, hwnd);
            }
            0
        }
        WM_COMMAND => {
            on_command(context, wparam);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// 放行来自 shell 的窗口消息。
///
/// 客户端以「最高权限」运行（建 TUN 要管理员），explorer 是中等完整性：UIPI 默认
/// 会把它的消息挡在门外——图标看得见、点下去没反应，explorer 重启之后也收不到
/// TaskbarCreated，图标就再也回不来。
fn allow_shell_messages(hwnd: HWND, taskbar_created: u32) {
    for message in [CALLBACK_MESSAGE, taskbar_created] {
        unsafe {
            ChangeWindowMessageFilterEx(hwnd, message, MSGFLT_ALLOW, std::ptr::null_mut());
        }
    }
}

/// 把图标交给任务栏。返回 false 表示这次没加上（任务栏还没就绪）。
fn add_icon(context: &Context, hwnd: HWND) -> bool {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = 1;
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = CALLBACK_MESSAGE;
    data.hIcon = context.icon.load(Ordering::SeqCst) as HICON;
    fill_tip(&mut data.szTip, &context.tooltip);
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) != 0 }
}

/// 开机时客户端常常比 explorer 先起来：那时任务栏还没建好，`NIM_ADD` 直接返回
/// FALSE，图标就再也不会出现。失败要重试，直到加上或一分钟过去。
///
/// `hwnd` 走 usize：HWND 是裸指针，不是 Send，但窗口本身属于消息泵那个线程，
/// 别的线程只拿它的值调 Shell_NotifyIconW 与 ChangeWindowMessageFilterEx。
fn add_icon_with_retry(context: Arc<Context>, hwnd: usize) {
    let _ = thread::Builder::new()
        .name("mesh-tray-icon".to_string())
        .spawn(move || {
            for _ in 0..120 {
                if add_icon(&context, hwnd as HWND) {
                    return;
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
}

fn remove_icon(hwnd: HWND) {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = 1;
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

/// 按下右键时按当前模型重建菜单。
fn show_menu(context: &Context, hwnd: HWND) {
    let (address, update_state, update_version) = match context.model.lock() {
        Ok(model) => (
            model.local_ip.clone(),
            model.update_state.clone(),
            model.update_version.clone(),
        ),
        Err(_) => (String::new(), String::new(), String::new()),
    };
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    match update_state.as_str() {
        // 下载中只提示、不可点：此时 ApplyUpdate 还没有可安装的文件。
        "downloading" => append(
            menu,
            MF_STRING | MF_GRAYED,
            ID_UPDATE,
            mesh_text::get(id::CLIENT_TEXT_UPDATE_DOWNLOADING),
        ),
        "ready" => {
            let text = if update_version.is_empty() {
                mesh_text::get(id::CLIENT_TEXT_UPDATE_READY).to_string()
            } else {
                mesh_text::format(id::CLIENT_TEXT_UPDATE_READY_VERSION, &[&update_version])
            };
            append(menu, MF_STRING, ID_UPDATE, &text);
        }
        _ => {}
    }
    let address_text = if address.is_empty() {
        mesh_text::get(id::CLIENT_TEXT_ADDRESS_UNCONNECTED).to_string()
    } else {
        mesh_text::format(id::CLIENT_TEXT_ADDRESS, &[&address])
    };
    append(menu, MF_STRING | MF_GRAYED, ID_ADDRESS, &address_text);
    append(menu, MF_SEPARATOR, 0, "");
    append(
        menu,
        MF_STRING,
        ID_PANEL,
        mesh_text::get(id::CLIENT_MENU_OPEN_PANEL),
    );
    append(
        menu,
        MF_STRING,
        ID_DIR,
        mesh_text::get(id::CLIENT_MENU_OPEN_APP_DIR),
    );
    append(menu, MF_SEPARATOR, 0, "");
    // 自启只在 Windows 桌面形态下有意义：Linux 上客户端由 systemd 拉起。
    let checked = if context.autostart.load(Ordering::SeqCst) {
        MF_CHECKED
    } else {
        0
    };
    append(
        menu,
        MF_STRING | checked,
        ID_AUTOSTART,
        mesh_text::get(id::CLIENT_MENU_AUTOSTART),
    );
    append(menu, MF_SEPARATOR, 0, "");
    append(
        menu,
        MF_STRING,
        ID_QUIT,
        mesh_text::get(id::CLIENT_MENU_QUIT),
    );

    let mut point = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut point);
        // TrackPopupMenu 之前必须把窗口提到前台，否则点菜单外面它不会消失。
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

fn append(menu: HMENU, flags: u32, id: usize, text: &str) {
    let label = platform::wide(text);
    unsafe {
        AppendMenuW(menu, flags, id, label.as_ptr());
    }
}

fn on_command(context: &Arc<Context>, raw: usize) {
    match raw & 0xFFFF {
        ID_PANEL => {
            let url = context.url.clone();
            let _ = thread::Builder::new()
                .name("mesh-open-panel".to_string())
                .spawn(move || {
                    if let Err(err) = platform::open_browser(&url) {
                        platform::notify_error(&mesh_text::format(
                            id::CLIENT_ERROR_OPEN_PANEL,
                            &[&err],
                        ));
                    }
                });
        }
        ID_DIR => {
            let root = context.root.clone();
            let _ = thread::Builder::new()
                .name("mesh-open-dir".to_string())
                .spawn(move || {
                    if let Err(err) = platform::open_dir(&root) {
                        platform::notify_error(&mesh_text::format(
                            id::CLIENT_ERROR_OPEN_APP_DIR,
                            &[&err],
                        ));
                    }
                });
        }
        ID_UPDATE => {
            // 失败已经由引擎的 Notice 走消息框，这里不等回复，免得卡住消息泵。
            let tx = context.tx.lock().ok().map(|tx| tx.clone());
            if let Some(tx) = tx {
                let _ = thread::Builder::new()
                    .name("mesh-tray-update".to_string())
                    .spawn(move || {
                        let (reply, _reply_rx) = std::sync::mpsc::channel();
                        let _ = tx.send(Cmd::ApplyUpdate(reply));
                    });
            }
        }
        ID_AUTOSTART => {
            // 不在消息泵里等回复（引擎可能正忙），也不先改勾选状态：改成功才写，
            // 失败由引擎的 Notice 弹框，菜单保持原状。
            let context = Arc::clone(context);
            let on = !context.autostart.load(Ordering::SeqCst);
            let tx = context.tx.lock().ok().map(|tx| tx.clone());
            if let Some(tx) = tx {
                let _ = thread::Builder::new()
                    .name("mesh-tray-autostart".to_string())
                    .spawn(move || {
                        let (reply, reply_rx) = std::sync::mpsc::channel();
                        if tx.send(Cmd::SetAutostart(on, reply)).is_err() {
                            return;
                        }
                        if let Ok(Ok(snapshot)) = reply_rx.recv() {
                            context
                                .autostart
                                .store(snapshot.autostart_enabled, Ordering::SeqCst);
                        }
                    });
            }
        }
        ID_QUIT => (context.quit)(),
        _ => {}
    }
}

fn fill_tip(slot: &mut [u16; 128], text: &str) {
    let units: Vec<u16> = text.encode_utf16().take(slot.len() - 1).collect();
    slot[..units.len()].copy_from_slice(&units);
    slot[units.len()] = 0;
}

/// 从字节造 HICON。
///
/// 当前 tray.ico 的 8 张图全是 PNG 压缩的（Vista 之后的 .ico 常见），
/// CreateIconFromResourceEx 只认 RT_ICON 的 DIB 格式，遇到 PNG 直接失败。
/// LoadImageW 能读 PNG，但只接受文件路径；Go 的 systray 也是先写临时文件再
/// LoadImage 的，这里照做，读完立刻删。
fn load_tray_icon(bytes: &[u8]) -> Result<HICON, String> {
    let path = std::env::temp_dir().join(format!("mesh-client-tray-{}.ico", std::process::id()));
    std::fs::write(&path, bytes)
        .map_err(|err| mesh_text::format(id::CLIENT_ERROR_WRITE_TRAY_ICON, &[&err]))?;
    let wide = platform::wide(&path.display().to_string());
    let icon = unsafe {
        LoadImageW(
            std::ptr::null_mut(),
            wide.as_ptr(),
            IMAGE_ICON,
            0,
            0,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        )
    };
    let _ = std::fs::remove_file(&path);
    if icon.is_null() {
        return Err(mesh_text::format(
            id::CLIENT_ERROR_LOAD_TRAY_ICON,
            &[&std::io::Error::last_os_error()],
        ));
    }
    Ok(icon)
}
