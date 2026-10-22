# 第三方组件

Luban Notepad 以木兰公共许可证第 2 版（Mulan PubL v2，MulanPubL-2.0）授权。它构建于以下开源 Rust crate 之上。
权威的许可证文本随各 crate 一起发布于 Cargo registry，也可从所链接的仓库获取。

| 组件 | 许可证 | 用途 |
| --- | --- | --- |
| [GPUI Kit](https://github.com/longbridge/gpui-kit)（`gpui-kit`、`gpui-component`、`gpui-base`） | Apache-2.0 | 应用框架、样式化组件、代码编辑器、文件/输入控件。 |
| [GPUI](https://github.com/zed-industries/zed)（`gpui-pre` 快照） | Apache-2.0 | GPU 加速 UI 运行时。 |
| [syntect](https://github.com/trishume/syntect) | MIT | 语法定义与高亮。 |
| [two-face](https://github.com/CosmicHorrorDev/two-face) | MIT/Apache-2.0 | syntect 的扩展语法集。 |
| [regex](https://github.com/rust-lang/regex) | MIT/Apache-2.0 | 正则表达式搜索。 |
| [encoding_rs](https://github.com/hsivonen/encoding_rs) | MIT/Apache-2.0 | 字符编码转换。 |
| [chardetng](https://github.com/hsivonen/chardetng) | MIT/Apache-2.0 | 编码检测。 |
| [serde](https://github.com/serde-rs/serde) / [serde_json](https://github.com/serde-rs/json) | MIT/Apache-2.0 | 设置与会话序列化。 |
| [dirs](https://github.com/dirs-dev/dirs-rs) | MIT/Apache-2.0 | 平台配置目录。 |
| [walkdir](https://github.com/BurntSushi/walkdir) | MIT/Unlicense | 递归文件搜索。 |
| [env_logger](https://github.com/rust-cli/env_logger) | MIT/Apache-2.0 | 日志记录。 |
| [rust-i18n](https://github.com/longbridge/rust-i18n) | MIT | 编译期嵌入的界面翻译。 |
| [sys-locale](https://github.com/1Password/sys-locale) | MIT/Apache-2.0 | 读取操作系统界面语言。 |
| [tempfile](https://github.com/Stebalien/tempfile) | MIT/Apache-2.0 | 仅测试使用的临时目录。 |

Luban Notepad 是一个独立的重新实现，不包含 Notepad++、Scintilla 或 Lexilla
的源代码。Notepad++ 项目以 GPL-3.0 授权；其交互模型启发了本应用。
