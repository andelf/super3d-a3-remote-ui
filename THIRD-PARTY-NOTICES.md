# 第三方资产声明（THIRD-PARTY NOTICES）

本仓库的**源代码**以 MIT 授权（见 [LICENSE](LICENSE)）。

但 `assets/fonts/` 下的字体不是本项目原创，**不适用 MIT**，各自按其原授权分发。
这些字体都被 `src/fonts.rs` 用 `include_bytes!` 编进了可执行文件，所以随二进制一起分发时，
下列授权条件同样需要遵守（主要是保留版权声明与授权文本）。

| 文件 | 字体 | 授权 |
|---|---|---|
| `LXGWWenKaiRegular.ttf` | [LXGW WenKai / 霞鹜文楷](https://github.com/lxgw/LxgwWenKai) | SIL Open Font License 1.1 |
| `IBMPlexSans-Text.ttf` | [IBM Plex Sans](https://github.com/IBM/plex) | SIL Open Font License 1.1 |
| `NotoSans-Regular.ttf` | [Noto Sans](https://github.com/notofonts/latin-greek-cyrillic) | SIL Open Font License 1.1 |
| `jetbrains_mono_variable.ttf` | [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono) | SIL Open Font License 1.1 |
| `LiberationMono-Regular.ttf` | [Liberation Mono](https://github.com/liberationfonts/liberation-fonts) | SIL Open Font License 1.1 |
| `fa-solid-900.ttf` | [Font Awesome Free](https://github.com/FortAwesome/Font-Awesome) | 图标：CC BY 4.0；字体：SIL OFL 1.1；代码：MIT |

## SIL Open Font License 1.1 要点

- 允许自由使用、修改、再分发，**包括随商业软件一起分发**；
- 分发时必须保留字体原有的版权声明与 OFL 授权文本；
- **不得单独售卖字体本身**；
- 修改后的字体不得继续使用原保留字体名称（Reserved Font Name）。

完整授权文本见上述各项目的 `OFL.txt`。
