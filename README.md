# Starfish Control

基于 **Makepad 2.0** 的 3D 打印机控制端，目标设备为 Super3D / Snapmaker Luban 定制版
（星鱼 A 系列），使用 **starfish-net** 协议（UDP 11221 发现 + TCP 7777 控制）。

设计依据：`github.com/Zhanghandong/makepad-skills`（Makepad 2.0 技能集）。
协议依据：抓包逐字节验证（见上层目录 `starfish-net-协议文档.md`、`starfish.py`）。

## 功能

- 设备发现（UDP 广播）+ 设备列表页
- 设备控制页：状态实时显示、文件列表（列出/删除）、上传 G-code、开始/暂停/恢复/停止打印
- 打印剩余时间：协议不上报时间。文件是本软件上传的 → 用头部 `;TIME:` 总时长（切片器的真实值，准）；机内老文件拿不到头部 → 退回 `(时刻, 进度)` 最小二乘斜率外推，界面标「实测估算」（`src/eta.rs`）
- 手动控制：回原点、单轴回零、风扇、温度、电机移动、关电机、挤出、回抽、急停
- **命令行**：作为 OrcaSlicer 的后处理脚本被调用。命令行进程只做后处理，然后把
  「传这个文件」交接给常驻的图形界面并**立刻退出**，切片器不会被拖住（见下节）；
  也支持 `--list` / `--status` / `--delete` headless 运行
- 协议层与 UI 解耦：同一份协议代码被 GUI 与 CLI 共用

## 构建（重要）

本工程**不能**在未安装 C/C++ 链接器的机器上链接（Makepad 是原生 GUI）。
需要以下之一：

1. **Visual Studio Build Tools**（"使用 C++ 的桌面开发"工作负载）→ 提供 `cl.exe`/`link.exe`（MSVC）。
2. 或 GNU 路线：`rustup target add x86_64-pc-windows-gnu` + 安装 MinGW-w64，再
   `cargo build --target x86_64-pc-windows-gnu`。

```bash
# 在已装链接器的开发机：
cargo build            # 调试
cargo run              # GUI 模式，自动发现设备
cargo run -- --host 192.168.0.227        # 直接连指定设备
cargo run -- --list  --host 192.168.0.227  # 列出设备文件后退出
cargo run -- --status --host 192.168.0.227 # 打印状态后退出
cargo run -- print.gcode --host 192.168.0.227  # 后处理 → 交给界面上传 → 自己退出
```

## 命令行与图形界面怎么分工（重要）

OrcaSlicer 的后处理脚本是**同步**调的：脚本不退出，切片器就一直等。所以带文件启动时
**不能**在同一个进程里做上传——那要几秒到几十秒，切片器就卡成「无响应」。

现在的分工：

1. 命令行进程（`starfish-control.exe xxx.gcode`，也就是 Orca 调的那条）：
   - 做 G-code 后处理（纯本地，几十毫秒），`--rewrite` 时写回并留 `.orca-original` 备份；
   - 把结果**另存一份到自己的临时目录**（`%TEMP%\starfish-handoff\<pid>-<时间戳>\<真实导出名>`），
     交给界面的是这个副本的路径，不是切片器给的路径。原因：切片器在脚本退出后会立刻
     收尾（挪文件、重开写句柄、清临时目录），界面是异步接活的（冷启动还要 1–3 秒），
     等它真去读原路径就会「读取文件失败」；
   - 副本名不是切片器的临时名，而是**算回真实导出名**（见下节）；
   - 探测有没有图形界面在跑（`src/ipc.rs`，本地 127.0.0.1 服务）；
     - 有 → 把「传这个文件」发过去；
     - 没有 → 拉起一个界面（**stdio 全置 null**，否则管道不关，切片器照样等），
       然后重试交接，最多等 15 秒；
   - **立刻退出**（实测：界面已在跑 140ms，需要冷启动界面约 1 秒）。
2. 图形界面进程：常驻，本地服务监听 51721（`STARFISH_CTL_PORT` 可改）。
   收到请求后在自己的事件循环里上传，传完再问「是否开始打印」；`--yes` 则直接开打。
   上传期间再来请求只排队，不插队（插进去会让 `wait_ack` 的应答序列错位）。
   副本在上传收尾（成功/失败/连不上设备）后删除，不占 `%TEMP%`。

退出码只反映**后处理**成没成：交接失败不算失败（文件已经改好了，手动拖进界面也能传），
否则 Orca 会在导出结束时弹一个吓人的错误框。

### 交接时把窗口抬到前台（`src/raise.rs`）

切片器导出完，用户的眼睛还在切片器上；界面上马上会出现「是否开始打印」这种必须有人
回答的问题，压在后面等于没有。所以收到新交接时抬一次窗口：

- **只在交接到达那一刻抬**，上传进度、上传完成都不抬（那时用户已经在看了）；
- **2 秒冷却**：一次导出可能连着交来多个文件（多 plate 每 plate 一次），只抬第一次；
- 已经在最前面就什么都不做，避免无意义闪烁；
- 最小化会先还原；
- 被系统拒绝（Windows 默认禁止后台进程抢前台）时，抬一次 z 序再请求，仍不行就
  **闪任务栏**——这是系统的「有人找你」语义，不打断别人的输入；
- 命令行进程在交接前调 `AllowSetForegroundWindow(ASFW_ANY)` 转授许可。它通常由
  切片器（当时的前台程序）拉起，有这份许可；界面进程没有，不给许可就只能闪任务栏。

不做 `AttachThreadInput` 或常驻 `topmost`：那会永久改变 z 序、干扰别人的输入焦点。

## 上传名从哪来（`src/naming.rs`）

Orca 调后处理脚本时，传的是它**内部的临时文件**，不是你要导出的那个：

```
%TEMP%\orcaslicer_model\Tue_Nov_10\14_44_36#11596#237\Metadata\.11596.1.gcode.pp
```

真实导出文件（`Downloads\Part 1 (16)_PETG_6m4s.gcode`）是**脚本退出之后**才由 Orca
写出来的，脚本里拿不到那个路径。所以直接沿用临时名，界面上就是一串看不懂的点号数字。

Orca 在同一个会话目录里留了两样东西，够我们把名字一模一样算出来：

| 来源 | 内容 |
|---|---|
| `<会话>/origin.txt` | 原始 3MF 路径 → `input_filename_base` = `Part 1 (16)` |
| `<会话>/_temp_<n>.config` | `filename_format` 模板 + `filament_type` = `PETG` |
| gcode 头 | `; estimated printing time (normal mode) = 6m 4s` → `print_time` = `6m4s` |

模板是**读出来的**（`{input_filename_base}_{filament_type[initial_tool]}_{print_time}.gcode`），
不是硬编码，所以你在 Orca 里改命名规则也能跟上。`print_time` 用 Orca 自己的短格式
（`6m4s` / `1h21m`，不带前导零），和 `header::fmt_duration` 的 `6m04s` 不是一回事。

任一环缺失就退回 `origin.txt` 的 3MF 名 + `.gcode`；再不行才用原始临时名（去掉 `.pp`）。
**宁可退化，也不拼一个看着像真的、实际对不上的名字。**

已知限制：多 plate 时 Orca 对每个 plate 都调一次脚本，推导出的名字相同 —— 但上传时会自动
缀时间戳（见下），所以不会互相覆盖。

## 上传名一律带时间戳

设备端**同名不覆盖**：新内容传上去，`开始打印` 还是上一次的刀路，界面上看文件列表、大小、
进度全都"正常"——这种错极难发现。所以每次上传都在扩展名前插一个本地时间戳：

```
Part 1 (16)_PETG_6m4s.gcode
  → Part 1 (16)_PETG_6m4s_20261110-154030.gcode
```

- 位置在 `prepare_upload`（所有上传路径的唯一出口：命令行交接、拖入、文件选择器都走它）；
- **点前缀一律去掉**：设备端把 `.` 开头的文件当隐藏文件，列表接口不列出来 ——
  传上去了也看不见、选不到去打印，等于白传。切片器的中间文件恰好是
  `.11596.1.gcode` 这种形状，所以 `sanitize_name`、`naming::stamp`、`handoff::strip_pp`
  三处都去点；
- 名字超过设备端 64 字节上限时**掐主体保时间戳和扩展名**（`naming::stamp`）；
- 状态栏显示 `原名 → 上传名 …`，免得误以为时间戳是切片器起的名字；
- 交接落地的副本（`%TEMP%\starfish-handoff\…`）仍用不带时间戳的干净名字，
  只有传给设备的那个名字加戳。

## 构建与测试状态

- `cargo build --release` / `cargo test --release` 在本机（MSVC + VS Build Tools）通过，
  47 个单测通过、1 个 `ignored`（`header::real_gcode` 需要 `SF_GCODE` 指向真实切片文件）。
- `makepad-widgets` 走 git `dev` 分支，`Cargo.lock` 已锁定 commit。
- 剩余编译警告都是 dead_code（协议层预留字段）与 `protocol/mod.rs` 的冗余 re-export。

## 字体内嵌（分发必读）

**exe 是自足的单文件，不需要带字体目录，也不依赖系统字体。**

Makepad 的字体是按资源路径读文件的：`register_crate_resource_path` 在**编译期**就把
`makepad_widgets/resources/xxx.ttf` 解析成了**本机 cargo 源码树里的绝对路径**
（`~/.cargo/git/checkouts/makepad-*/…/widgets/resources/xxx.ttf`）。普通 `cargo build`
出来的 exe 运行时按那个绝对路径找文件，找不到就把字面判死：

```
[E] font "…/widgets/resources/IBMPlexSans-Text.ttf" (member ibm_plex_text)
    is not packaged with this app, so its text will show as boxes
```

表现就是「有图形块、但没有文字」。所以 `src/fonts.rs` 把 ttf 用 `include_bytes!` 编进
二进制，并在启动时填回脚本资源表（`cx.script_data.resources`）：Makepad 按路径读文件失败后
会回落到资源表里已 `Loaded` 的数据，正好就是这几份。代价是 exe 约 +19 MB（主要是中文字面）。

内嵌的字面都在 `assets/fonts/`：

| 文件 | 用途 | 大小 |
|---|---|---|
| `IBMPlexSans-Text.ttf` | 拉丁正文（粗体/斜体也复用它） | 0.17 MB |
| `NotoSans-Regular.ttf` | 拉丁兜底 | 0.19 MB |
| `jetbrains_mono_variable.ttf` | 界面符号 | 0.29 MB |
| `LiberationMono-Regular.ttf` | 等宽（代码） | 0.10 MB |
| `fa-solid-900.ttf` | 图标 | 0.41 MB |
| `LXGWWenKaiRegular.ttf` | **中文**（粗体/emoji 成员也复用它） | 18.19 MB |

换字体的办法：把新 ttf 放进 `assets/fonts/`，在 `src/fonts.rs` 的 `face()` 里把文件名映射过去，
`cargo build --release` 即可（真正要改的是「成员名 → 字节」这张表）。

- 中文用的是 **LXGW WenKai**，OFL 1.1 授权，可随二进制再分发。
- 刻意**没有**内嵌 `NotoColorEmoji.ttf`（10 MB）：界面里不用 emoji，`⚠` 之类已换成「注意」。
- 验证方式（模拟别人的机器）：把 `~/.cargo/git/checkouts/makepad-*/…/widgets/resources`
  临时改名，再跑 exe —— 文字照常显示、日志里没有 `not packaged` 就是对的。

## 目录结构

```
starfish-control/
  Cargo.toml
  assets/fonts/        # 内嵌字体（见上）
  src/
    main.rs            # 入口，app_main!(App, configure: …) —— 后处理模式在这里先拦下
    cli.rs             # 命令行参数
    handoff.rs         # 命令行侧：后处理 → 交给界面 → 退出（含单测）
    ipc.rs             # 单实例本地服务（127.0.0.1:51721，含单测）
    fonts.rs           # 把内嵌字体填进脚本资源表（换机器不变方块）
    eta.rs             # 剩余时间（有切片器总时长就用它，没有才退到实测斜率）
    app.rs             # App / MatchEvent / AppMain / Splash UI
    protocol/
      mod.rs
      frame.rs         # 帧编解码（已用抓包验证，含单测）
      status.rs        # 39 字节状态解析（含单测）
      connection.rs    # UDP 发现 + TCP 后台线程 + 通道桥 + 全部命令构造
      header.rs        # G-code 头部重写（缩略图/时长/键值归一化，含单测）
      gcode.rs         # G-code 解析（预览用，含单测）
      sacp.rs          # SACP 占位（本设备不使用）
```

## 协议约束提醒

starfish-net **没有"执行 G-code 字符串"的命令**。G-code 只能作为文件经 cmd 9 上传、
再经 cmd 11/1 开始打印。因此"命令行直接上传 Gcode"实现为：命令行做后处理 → 交给界面
→ 上传文件 → 弹出预览 → 确认（或 `--yes` 直接）开始打印。这与需求体验一致。

## 已知问题

- 机内**删除**的文件名解析还没验证：实测出现过「删一个掉好几个」，确认弹窗里也写着这句。
  要动删除之前先刷新列表、并对着一台不重要的设备试。
- 暂停 / 恢复 / 停止打印：`Connection::print_pause / print_resume / print_stop` 已实现，
  界面还没有按钮（打印中只能急停）。
- 单轴回零：`Connection::control_home_axis` 已实现，界面未接。
