# 用户包：语法、主题和片段

Luban Notepad 从配置目录下的 `packages/` 读取用户语法、主题和片段。包是数据
文件，不是插件：它们不会执行代码，安全模式下仍然加载。

## 目录

```text
<config-dir>/packages/
  syntaxes/**.sublime-syntax
  themes/**.tmTheme
  snippets/**.sublime-snippet
  snippets/**.json
```

目录在首次启动或首次打开包管理器时创建。扫描是递归的，按路径排序；只读取
普通文件。单个文件上限 2 MiB。限制：语法最多 1,000 个，主题最多 500 个，
片段最多 10,000 个。非法文件只影响自身，不阻止其他文件或内置资源加载。

改动包后执行“重新加载用户包”（`packages.reload`），或重启应用。

## 用户语法

`syntaxes/` 中的 `.sublime-syntax` 使用 syntect 的 YAML 解析逐文件加载，合并
到内置语法集之后。同名用户语法覆盖内置语法的高亮定义。

```yaml
%YAML 1.2
---
name: MyLang
file_extensions: [myl]
scope: source.myl
contexts:
  main:
    - match: '\b(if|else|while)\b'
      scope: keyword.control.myl
    - match: '"[^"]*"'
      scope: string.quoted.double.myl
```

- `name` 成为语言显示名；`file_extensions` 用于扩展名检测；可选的
  `first_line_match` 用于无扩展名文件的首行检测（内置语言优先按扩展名/文件名
  匹配，只有未识别时才使用首行规则）。
- 与内置语言同名时复用内置语言的注释和缩进元数据；新语言没有行注释元数据，
  注释命令会提示不支持。
- 用户语法用于编辑器高亮和 HTML/RTF 导出；两者使用同一合并语法集。
- 不支持 Sublime 的全部 `extends`/`prototype` 和高级 context 语义；syntect
  能解析的部分生效，失败的文件会记录错误并跳过。

## 用户主题

`themes/` 中的 `.tmTheme` 由 syntect 解析，主题名称取 plist 的 `name`，缺失时
取文件名。主题出现在“首选项 → 语法主题”中，选择后立即应用到编辑器背景、正文
前景（包括没有语法高亮的纯文本文档；主题未定义的语法角色同样继承正文前景）、
语法颜色、当前行和行号槽背景，并持久化到 `settings.json`；导出使用同一主题。
行号槽背景取自主题的 `gutter`；行号文字颜色由 gpui-component 0.8 编辑器固定使用
界面主题的次要文本色，因此 `gutterForeground` 不会改变行号文字。细节见“已知限制”。

主题文件损坏或缺失时在启动阶段回退到 `InspiredGitHub`，状态栏提示并记录错误；
运行中选择损坏的主题会保留当前主题。旧版导入的单个 `.tmTheme`
（`syntax_theme_path`）继续可用，并在语法主题下拉菜单中显示为“自定义: <文件名>”，
即使 `syntax_theme` 仍是旧值也可以重新选中它。包主题同样出现在下拉菜单中。

## 片段

### `.sublime-snippet`

```xml
<snippet>
    <content><![CDATA[println!("${1:value}");]]></content>
    <tabTrigger>pl</tabTrigger>
    <scope>source.rust</scope>
    <description>Print line</description>
</snippet>
```

### `snippets.json`

```json
{
  "snippets": [
    {
      "name": "Rust main",
      "trigger": "main",
      "scope": "source.rust",
      "body": ["fn main() {", "    $0", "}"]
    },
    {
      "trigger": "log",
      "body": "luban.log(\"info\", \"$1\")"
    }
  ]
}
```

顶层也可以是数组。`body` 可以是字符串或字符串数组（数组用换行连接）。

字段：

- `name` / `description`：选择器显示名；缺失时使用 `trigger`。
- `trigger`：可选前缀。
- `scope`：可选，逗号分隔；支持 `source.<token>`、`text.<token>`、
  `source.<语言名>`、`text.<语言名>`，大小写不敏感。复杂 Sublime 选择器和
  排除语法不支持；没有 scope 的片段对所有语言可见。打开选择器时，含有不支持
  选择器的 scope 会写入警告日志；没有片段匹配当前语言时，日志会同时列出当前
  语言、已加载数量和未匹配的 scope。当前语言的片段在选择器中排在全局片段之前，
  组内按名称排序。
- `body`：必填，非空。

### 插入与占位符

`edit.insert-snippet`（默认 `Ctrl+Alt+S`）打开片段选择器。选择后：

1. 有选区时替换选区，否则在光标处插入。
2. 展开 `$1`、`$2`、…、`${1:默认值}`、`$0`；`\$` 表示字面量 `$`。
3. 第一个 tab stop 的默认值被选中；没有 stop 时光标在插入文本末尾。
4. 片段会话中 `Tab` 下一个 stop，`Shift+Tab` 上一个 stop，`Esc` 结束。
5. 一次插入是一次可撤销编辑；撤销、切换标签或点击编辑器结束会话。

限制：不支持 `${1|a,b|}` 等选择语法、正则转换、变量替换和镜像编辑；重复编号
的 stop 按独立 stop 处理。
