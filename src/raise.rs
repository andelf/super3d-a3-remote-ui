//! 把窗口抬到前台。
//!
//! # 为什么需要
//!
//! 切片器导出时，用户的眼睛在切片器上；我们的界面多半被压在后面。命令行把文件
//! 交接过来之后，界面上会出现「是否开始打印」这种**必须有人回答**的问题。
//! 不抬窗口，用户就看着切片器等，以为卡住了。
//!
//! # 为什么不能硬抢
//!
//! Windows 默认禁止后台进程抢前台（`SetForegroundWindow` 会失败），这是防
//! 钓鱼的保护。绕过它的常见做法是 `AttachThreadInput` 或定时器刷 `topmost`，
//! 那套做法会永久改变窗口的 z 序、干扰别人的输入焦点，不做。
//!
//! 这里的顺序是：**先请求，被拒就抬 z 序再请求，还不行就闪任务栏**。
//! 闪任务栏是系统的「有人找你」语义，不打断任何人的输入。
//!
//! 另外提供 [`allow_foreground`] 给命令行进程用：它由切片器（当时的**前台**程序）
//! 拉起，有资格把「允许抢前台」的许可转授出去，这样界面那边的第一次请求就能成功。

/// 把本进程的主窗口抬到前台。已经在前面就什么都不做（避免无谓的闪烁）。
pub fn attention() {
    imp::attention();
}

/// 把「允许设置前台窗口」的许可授给任意进程（`ASFW_ANY`）。
///
/// 只对**命令行交接进程**有意义：它通常从切片器（前台程序）被拉起，继承了那份许可。
/// 不调用也不会崩，只是界面那边可能抢不到前台，退化成闪任务栏。
pub fn allow_foreground() {
    imp::allow_foreground();
}

#[cfg(windows)]
mod imp {
    type Hwnd = isize;
    type Bool = i32;

    const SW_RESTORE: i32 = 9;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const HWND_TOPMOST: Hwnd = -1;
    const HWND_NOTOPMOST: Hwnd = -2;
    const GW_OWNER: u32 = 4;
    const FLASHW_TRAY: u32 = 0x0000_0002;
    const FLASHW_TIMERNOFG: u32 = 0x0000_000C;
    const ASFW_ANY: u32 = u32::MAX;

    #[repr(C)]
    struct FlashInfo {
        size: u32,
        hwnd: Hwnd,
        flags: u32,
        count: u32,
        timeout: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> Hwnd;
        fn IsWindowVisible(h: Hwnd) -> Bool;
        fn IsIconic(h: Hwnd) -> Bool;
        fn ShowWindow(h: Hwnd, cmd: i32) -> Bool;
        fn SetForegroundWindow(h: Hwnd) -> Bool;
        fn SetWindowPos(h: Hwnd, after: Hwnd, x: i32, y: i32, cx: i32, cy: i32, flags: u32)
            -> Bool;
        fn EnumWindows(cb: extern "system" fn(Hwnd, isize) -> Bool, lparam: isize) -> Bool;
        fn GetWindowThreadProcessId(h: Hwnd, pid: *mut u32) -> u32;
        fn GetWindow(h: Hwnd, cmd: u32) -> Hwnd;
        fn FlashWindowEx(info: *mut FlashInfo) -> Bool;
        fn AllowSetForegroundWindow(pid: u32) -> Bool;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
    }

    struct Ctx {
        pid: u32,
        found: Hwnd,
    }

    /// 找本进程的「主窗口」：可见、无 owner 的顶层窗口里第一个。
    ///
    /// 不缓存：窗口可能被重建（改 DPI、切全屏），缓存一个旧 HWND 会往死窗口上发消息。
    extern "system" fn pick(h: Hwnd, lparam: isize) -> Bool {
        let ctx = unsafe { &mut *(lparam as *mut Ctx) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        if pid == ctx.pid {
            let visible = unsafe { IsWindowVisible(h) } != 0;
            let unowned = unsafe { GetWindow(h, GW_OWNER) } == 0;
            if visible && unowned {
                ctx.found = h;
                return 0; // 找到一个就停
            }
        }
        1
    }

    fn main_window() -> Hwnd {
        let mut ctx = Ctx {
            pid: unsafe { GetCurrentProcessId() },
            found: 0,
        };
        unsafe { EnumWindows(pick, &mut ctx as *mut Ctx as isize) };
        ctx.found
    }

    fn flash(h: Hwnd) {
        let mut info = FlashInfo {
            size: std::mem::size_of::<FlashInfo>() as u32,
            hwnd: h,
            flags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            count: 3,
            timeout: 0,
        };
        unsafe { FlashWindowEx(&mut info) };
    }

    pub fn attention() {
        let h = main_window();
        if h == 0 {
            return;
        }
        unsafe {
            if GetForegroundWindow() == h {
                return; // 已经在最前，别抖一下
            }
            if IsIconic(h) != 0 {
                ShowWindow(h, SW_RESTORE);
            }
            if SetForegroundWindow(h) != 0 {
                return;
            }
            // 被系统拒了：抬一次 z 序再请求。**立刻落回**，不常驻置顶。
            SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            SetWindowPos(h, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            if SetForegroundWindow(h) == 0 {
                flash(h); // 最后还是不给，就闪任务栏
            }
        }
    }

    pub fn allow_foreground() {
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn attention() {}
    pub fn allow_foreground() {}
}
