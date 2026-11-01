# 插件商店

Luban Notepad 的插件商店由一个 Git 目录仓库和一个应用内客户端组成。目录仓库
保存插件元数据、JSON Schema、校验脚本和贡献说明；插件压缩包由插件作者托管在
不可变的 HTTPS 地址（通常是 GitHub Release）。编辑器读取目录、缓存结果，并在
插件管理器中提供浏览、搜索、安装和更新。

商店只分发现有 Lua 插件模型支持的包：zip 中包含 `plugin.json` 和 Lua 源码。
安装后由现有插件管理器加载，权限批准、依赖校验、安全模式和卸载流程完全复用。

默认目录仓库：<https://github.com/yyn0210/luban-notepad-store>

```text
https://raw.githubusercontent.com/yyn0210/luban-notepad-store/main/catalog/index.json
```

## 入口

- 菜单：插件 → 插件商店…
- 命令面板：`plugins.store`
- 插件管理器顶部“已安装 / 商店”页签
- 商店页可以直接修改目录地址；“应用”保存并刷新，“重置”恢复默认地址

打开商店时在后台获取目录，不阻塞编辑器。应用启动时不会联网；已经加载且未刷新
的目录在本次会话内复用，点击“刷新”可重新获取。

## 目录地址

设置项 `plugin_store_url` 支持：

- `https://` 绝对 URL（生产环境）；
- `file://` 绝对 URL 或本地绝对路径（开发、测试和离线镜像）。

其他协议（包括 `http://`）会被拒绝，保存失败不会改变当前值。修改地址后会清空
当前目录状态并重新获取。

## 信任模型

版本 1 使用 HTTPS + 内容校验：

- 目录和产物必须通过 HTTPS 获取；`file://` 仅用于本地开发和测试。
- 每个产物在安装前核对目录声明的 SHA-256 和字节数。
- 目录和产物只作为数据读取，安装前不执行插件代码。
- 目录中的 `permissions` 只用于展示，不作为批准结果；安装后以 `plugin.json`
  为准，首次安装或权限变化仍进入现有权限批准流程。
- 版本 1 不实现 Ed25519 签名或密钥管理。

## 目录格式

完整格式见商店仓库的
[`schemas/plugin.schema.json`](https://github.com/yyn0210/luban-notepad-store/blob/main/schemas/plugin.schema.json)
和 `schemas/catalog.schema.json`。

插件条目主要字段：

| 字段 | 说明 |
| --- | --- |
| `id` | 与 `plugin.json` 的 `id` 一致，匹配 `[a-z0-9][a-z0-9._-]*` |
| `name` / `description` / `publisher` | 展示信息；`localizations` 可按语言覆盖名称和描述 |
| `tags` / `permissions` / `license` | 搜索、权限展示和许可证 |
| `source` / `homepage` / `icon` | 可选 HTTPS 地址；版本 1 不在应用内加载远程图片 |
| `latest_version` | 合法 SemVer，必须等于 `versions` 中最高版本 |
| `versions[].version` | 唯一 SemVer |
| `versions[].api_version` | 默认 1；高于宿主支持版本时不可安装 |
| `versions[].release_notes` | 纯文本显示，不渲染 HTML |
| `versions[].artifacts` | 非空；每项包含 `target`、`url`、`sha256`、`size` |

客户端对单个坏条目隔离：跳过该条目并显示错误，其余插件仍可浏览和安装。目录整体
不可解析（缺少 `catalog_version`、`catalog_version` 不支持、超过 8 MiB、不是
UTF-8 等）时显示错误和缓存，不展示部分结果。单条目录错误不会影响已安装插件或
编辑器其他部分。

## 平台与产物选择

支持的产物目标：

```text
universal
linux-x64
linux-arm64
darwin-x64
darwin-arm64
windows-x64
windows-arm64
```

作者可以发布一个 `universal` 包，也可以只覆盖部分平台。每个上架版本至少要有
一个产物；校验脚本拒绝未知目标、重复目标和空产物列表，但不要求覆盖全部平台。

客户端根据 `std::env::consts::OS` 和 `std::env::consts::ARCH` 计算当前目标：

| 平台 | 目标 |
| --- | --- |
| Linux x86_64 | `linux-x64` |
| Linux aarch64 | `linux-arm64` |
| macOS x86_64 | `darwin-x64` |
| macOS aarch64 | `darwin-arm64` |
| Windows x86_64 | `windows-x64` |
| Windows aarch64 | `windows-arm64` |

选择规则：

1. 过滤 `api_version` 高于宿主支持版本的版本；
2. 选择 SemVer 最高且当前平台有产物的兼容版本；
3. 产物优先精确匹配当前目标，没有时回退 `universal`；
4. 没有当前平台可用产物时，插件不出现在商店列表，也不会尝试安装；
5. 所有版本都需要更新的 API 时，插件仍显示，但安装按钮禁用并说明原因。

无法识别当前 OS/CPU 组合时，商店显示“不支持当前平台”并禁用安装，不猜测目标，
也不下载其他平台的包。

## 安装与更新

1. 点击安装/更新后，后台下载到
   `<config-dir>/plugins/.store/downloads/<id>-<version>.zip.part`；
2. 下载大小限制为目录声明的 `size` 加 1 字节，超过立即失败；
3. 校验 SHA-256 和实际字节数；
4. 解包并读取 `plugin.json`，确认清单 `id`、`version` 与目录一致；
5. 调用现有 zip 安装事务原子替换；失败时保留原插件、数据和设置；
6. 安装成功后重新加载插件注册表，状态显示成功或具体失败原因。

更新保留插件 `data/` 目录和设置。权限集合不变时保留原批准哈希；权限变化时重新
进入“需要批准权限”。首次安装或权限变化后，插件不会执行任何 Lua，直到用户在
“已安装”页签批准。

安全模式或全局禁用插件时，商店仍可浏览和安装，但安装后的插件不会执行。

## 缓存

- 目录缓存：`<config-dir>/plugins/.store/catalog.json`
- 缓存元数据：`<config-dir>/plugins/.store/catalog.meta.json`，记录来源、获取
  时间和 schema 版本
- 下载目录：`<config-dir>/plugins/.store/downloads/`

获取成功后原子写入缓存；写入失败只记录日志，不影响本次展示。获取失败时，如果
存在同源缓存，商店显示缓存并标注“离线缓存”和获取时间；没有缓存时显示错误和
重试按钮。损坏的缓存会被忽略并删除，不影响已安装插件。

## 本地校验与发布

商店仓库包含无第三方依赖的 Python 工具：

```bash
python3 scripts/build_catalog.py          # 从 plugins/ 生成 catalog/index.json
python3 scripts/build_catalog.py --check  # 检查生成文件是否最新（CI 使用）
python3 scripts/validate.py               # 校验目录、条目、示例和一致性
python3 scripts/validate.py --download    # 额外下载并核对产物（默认不联网）
```

发布流程见商店仓库的
[CONTRIBUTING.md](https://github.com/yyn0210/luban-notepad-store/blob/main/CONTRIBUTING.md)：
在插件自己的仓库发布版本化 Release，选择 `universal` 或部分目标平台打包，把
产物上传到不可变 HTTPS 地址，提交 `plugins/<id>.json`，运行生成和校验脚本后
发起 Pull Request。`verified` 由商店维护者审核后设置，提交者不得自行设置。

## 故障排查

- **目录加载失败**：检查网络和代理设置。客户端读取 `HTTPS_PROXY`、`ALL_PROXY`、
  `HTTP_PROXY` 和 `NO_PROXY`（`HTTPS_PROXY` 优先于 `ALL_PROXY`）；`socks5://` 按
  远端 DNS（`socks5h`）使用，以兼容常见桌面代理。连接超时 10 秒，目录请求总超时
  30 秒，最多 5 次重定向，最终地址必须是 HTTPS。
- **看不到某个插件**：确认目录中有当前 OS/CPU 的产物或 `universal` 产物，并且
  `api_version` 不高于宿主支持版本。被当前平台过滤的插件不会出现在商店列表。
- **校验失败**：重新计算产物 SHA-256 和大小，确认发布文件没有被覆盖。
- **需要重试**：点击“刷新”重新获取目录；目录地址改动后使用“应用”。
- **离线**：网络失败时会自动显示同源缓存；缓存只用于展示，安装仍会重新下载和
  校验产物。
