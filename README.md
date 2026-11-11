# Super3D A3 远程打印控制（`super3d-a3-remote-ui`）

**Super3D A3 打印机的远程打印控制工具**：局域网内自动发现设备、上传 G-code、
开始/暂停/停止打印、查看温度与进度，并可挂到 **OrcaSlicer 的后处理**上——
切片导出结束后自动把文件送进界面，问一句「要不要现在打」。

技术栈：Rust + Makepad 2.0（原生 GUI，单文件 exe，字体已内嵌）。
通信走设备的 starfish-net 协议（UDP 11221 发现 + TCP 7777 控制），
目标机型 Super3D A3 / 星鱼 A 系列（Snapmaker Luban 定制固件）。

> **仓库名和可执行文件名不一样，这是故意的。** 仓库叫 `super3d-a3-remote-ui`，
> 编出来的程序仍是 `starfish-control.exe`——后处理脚本路径是写死在 OrcaSlicer
> 配置里的，改名会让已经配好的导出流程失效。

## 功能

- 自动发现设备（UDP 广播），设备列表页切换
- 状态实时显示：喷头/热床温度、进度、剩余时间、位置、风扇
- 机内文件管理：列出、上传、删除、发起打印
- 手动控制：回原点、风扇、温度、移动、挤出、急停
- 剩余时间：本软件上传的文件用 G-code 头部总时长（切片器的真实值，准）；
  机内老文件拿不到头部，退回按实测进度外推，界面标「实测估算」
- 命令行：可当 OrcaSlicer 后处理脚本，也能 headless 跑
  `--list` / `--status` / `--delete`

## 构建

Makepad 是原生 GUI，链接需要 C/C++ 工具链，二选一：

1. **Visual Studio Build Tools**（勾「使用 C++ 的桌面开发」）→ 提供 MSVC 的 `cl.exe` / `link.exe`；
2. GNU 路线：`rustup target add x86_64-pc-windows-gnu` + MinGW-w64，
   构建时加 `--target x86_64-pc-windows-gnu`。

```bash
cargo build --release                # 产物：target/release/starfish-control.exe
cargo run                            # GUI，自动发现设备
cargo run -- --host 192.168.0.227    # 直接连指定设备
```

发布时只拷这一个 exe 就行，字体已经编进二进制。

## 接到 OrcaSlicer（推荐用法）

OrcaSlicer → 打印设置 → 输出 → **后处理脚本**，填 exe 的完整路径：

```
"C:\path\to\starfish-control.exe"
```

导出时 Orca 会以 `starfish-control.exe <gcode路径>` 调用它，正好对应本程序的
「后处理 → 交给界面 → 退出」，**切片过程不会被上传拖住**。

常用参数（拼在脚本路径后面）：

| 参数 | 作用 |
|---|---|
| `--yes` / `-y` | 跳过「是否开始打印」，上传完直接开打 |
| `--host 192.168.0.227` | 固定连哪台设备，不用每次扫描 |
| `--rewrite` | 把改写后的头部写回原文件（留 `.orca-original` 备份） |
| `--material 黑色-PETG` | 覆盖写进头部的材料名 |

不接后处理也能用：把 `.gcode` 拖进窗口，或点「从本地选择」。

## 使用提示

- **连不上设备时文件不会丢**：请求留在队列里，交接副本保留在
  `%TEMP%\starfish-handoff\`，每 5 秒后台重连一次；连上后自动上传并弹
  「是否开始打印」，不用回切片器重新导出。
- **上传名会自动加时间戳**，例如
  `Clip S1_PETG_43m31s_20261111-133822.gcode`。设备端同名不覆盖，不加戳会出现
  「传了新文件、打的还是老刀路」这种极难发现的问题。
- **手动点「断开」会暂停自动重连**（文件仍保留），直到下一次导出或手动连接，
  免得刚断开又被连回去。
- **日志**在 `%LOCALAPPDATA%\starfish-control\starfish-control.log`，
  可用 `STARFISH_LOG_DIR` 改目录。`starfish-control.exe --dump-env` 只抓一份
  环境快照就退出，不开窗口、不碰设备。
- **导出时不会弹错误框**：退出码只反映后处理成没成，交接失败不算失败
  （文件已经改好了，手动拖进界面也能传）。

## 已知问题

- 机内**删除**的文件名解析还没验证：实测出现过「删一个掉好几个」。删除前先刷新
  列表，并对着一台不重要的设备试。
- 暂停 / 恢复 / 停止打印：协议命令已实现，界面还没有按钮（打印中只能急停）。
- 单轴回零：命令已实现，界面未接。

## 更多细节

实现层面的东西——协议报文、Orca 交接时序、上传名怎么反推、字体内嵌原理、
目录结构——都在 [`docs/调研与设计.md`](docs/调研与设计.md)。

## 版权

代码以 **MIT** 授权，全文见 [LICENSE](LICENSE)。Copyright (c) 2026 andelf。

`assets/fonts/` 下的字体**不适用 MIT**，各自按其原授权分发（均为 SIL OFL 1.1
或同等可再分发协议）：LXGW WenKai（中文）、IBM Plex Sans、Noto Sans、
JetBrains Mono、Liberation Mono、Font Awesome Free。
