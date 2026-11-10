//! 内嵌字体：把界面要用的字面塞进 exe，单文件拷走也能正常显示中文。
//!
//! ## 为什么必须内嵌
//!
//! Makepad 的字体是「按资源路径读文件」：`register_crate_resource_path` 在**编译期**
//! 就把 `makepad_widgets/resources/xxx.ttf` 解析成了本机 cargo 源码树里的绝对路径
//! （`…/.cargo/git/checkouts/makepad-*/…/widgets/resources/xxx.ttf`）。
//! 非打包模式（`cargo build` 出来的 exe 就是这种）运行时的取字节顺序是：
//!
//! 1. 依赖表（`cx.dependencies`，普通构建里是空的）；
//! 2. 直接按那个**绝对路径**读文件；
//! 3. 失败 → 该字面被标记为 unavailable，日志里是
//!    `font "…/IBMPlexSans-Text.ttf" (member …) is not packaged with this app,
//!     so its text will show as boxes`。
//!
//! 于是换一台没装过 makepad 源码的机器、或者只把 exe 拷给别人，**所有文字都变方块**
//! （图形块照常画，因为那不走字体）。这就是「缺少字体，无法显示内容」的根因。
//!
//! ## 怎么解决
//!
//! 把 ttf 用 `include_bytes!` 编进二进制，启动时填回脚本资源表
//! （`cx.script_data.resources`）。`get_resource_font_bytes_by_path` 在按路径读文件
//! 失败后会回落到「资源表里已 Loaded 的数据」，也就是我们塞进去的这几份；
//! `script_resource_unavailable` 也因此变 false，不会再把字面判死。
//!
//! 好处：exe 单文件可分发自足，不依赖 Fonts 目录、也不依赖系统字体（不必假设
//! 对方装了微软雅黑）。代价：exe 大约 +19 MB（主要是中文字面）。
//!
//! ## 取舍
//!
//! - 只内嵌一份中文常规字面（LXGW WenKai Regular，OFL 授权可再分发）。
//!   粗体/斜体都回落到同一份：我们没有真正的粗体，但**有字**远比「看着是粗的」重要，
//!   而且能省 18 MB。
//! - 不内嵌 NotoColorEmoji（10 MB）：界面里不再使用 emoji 区字符
//!   （`⚠` 已换成「注意」），缺了也不会掉字。

use makepad_widgets::makepad_platform::script::res::CxScriptResourceData;
use makepad_widgets::Cx;
use std::rc::Rc;

const PLEX_TEXT: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-Text.ttf");
const NOTO_SANS: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const UI_SYMBOLS: &[u8] = include_bytes!("../assets/fonts/jetbrains_mono_variable.ttf");
const MONO: &[u8] = include_bytes!("../assets/fonts/LiberationMono-Regular.ttf");
const ICONS: &[u8] = include_bytes!("../assets/fonts/fa-solid-900.ttf");
const CJK: &[u8] = include_bytes!("../assets/fonts/LXGWWenKaiRegular.ttf");

/// 字体文件名 → 内嵌字节。粗细/斜体一律回落到同族的常规字面。
fn face(file: &str) -> Option<&'static [u8]> {
    Some(match file {
        "IBMPlexSans-Text.ttf"
        | "IBMPlexSans-SemiBold.ttf"
        | "IBMPlexSans-Italic.ttf"
        | "IBMPlexSans-BoldItalic.ttf" => PLEX_TEXT,
        "NotoSans-Regular.ttf" => NOTO_SANS,
        "jetbrains_mono_variable.ttf" => UI_SYMBOLS,
        "LiberationMono-Regular.ttf" => MONO,
        "fa-solid-900.ttf" => ICONS,
        // 粗体也用常规字面：省 18 MB，观感差别只在标题。
        "LXGWWenKaiRegular.ttf" | "LXGWWenKaiBold.ttf" => CJK,
        // 彩色 emoji 字面有 10 MB，不内嵌。用中文字面顶上：emoji 区的符号字形
        // 它大多也有（我们界面里本来就不放 emoji），关键是**别让这个成员缺失**——
        // 缺了会在日志里刷一条「is not packaged with this app」，还会让整族字号
        // 反复重建。
        "NotoColorEmoji.ttf" => CJK,
        _ => return None,
    })
}

/// 把内嵌字体填进脚本资源表，返回补上的资源数。
///
/// 返回 0 说明资源表里还没有字体条目（脚本尚未求值），调用方该稍后重试。
pub fn install(cx: &mut Cx) -> usize {
    let store = cx.script_data.resources.resources.clone();
    let mut patched = 0usize;
    for res in store.borrow_mut().iter_mut() {
        // 已经读到真文件的就别覆盖（本机开发时读的是源码树里的同一份）。
        if matches!(res.data, CxScriptResourceData::Loaded(_)) {
            continue;
        }
        let Some(name) = res.abs_path.rsplit(['/', '\\']).next() else {
            continue;
        };
        if let Some(bytes) = face(name) {
            res.data = CxScriptResourceData::Loaded(Rc::new(bytes.to_vec()));
            patched += 1;
        }
    }
    if patched > 0 {
        // 让缓存了「这个资源读不到」的消费者重新问一次。
        cx.script_data.resources.bump_generation();
    }
    patched
}
