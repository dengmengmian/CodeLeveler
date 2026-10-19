# CodeLeveler 1.0.19

英文版：[`RELEASE.md`](RELEASE.md)

网页搜索现在是独立能力：搜索后端与对话模型分开选择，回答问题的模型和执行搜索
的服务是两个独立的配置项。本次补丁还修复了首条搜索结果的展示，并让搜索凭据
缺失变得可诊断。

## 网页搜索

- `~/.leveler/config.toml` 中的 `[web.search]` 是 `web_search` 的唯一开关。旧的 `LEVELER_SEARCH_API_KEY` 回退已移除：没有这个配置段，搜索不可用。
- 两种后端：DeepSeek 官方 Search API（`POST https://api.deepseek.com/v1/tools/search`）与 Tavily。
- 后端独立于 `[providers.*]` 与 `[models.*]`。切换对话模型（包括换到其它厂商）不会改变搜索后端；模型 Provider 的 `base_url` 不会被当作搜索端点。
- 凭据只从配置的 `api_key_env` 读取。模型网关的 Key 不会被复用到搜索；该环境变量会从子进程环境中清除，其值不会进入日志或工具输出。
- 每次调用只发起一次 HTTP 请求，遵守配置的超时，不重试，不切换到其它 Provider，也不会发起模型请求。
- 结果包含标题、URL、摘要与发布时间。TUI 直接展示首条结果，而不是外部数据占位符。

## 配置

```toml
[web.search]
provider = "deepseek"                     # deepseek | tavily
api_key_env = "LEVELER_OFFICIAL_SEARCH_KEY"
base_url = "https://api.deepseek.com/v1/tools"
max_results = 5
timeout_ms = 15000
```

在应用启动时的环境里导出 `LEVELER_OFFICIAL_SEARCH_KEY`。不要把 Key 写进仓库或
TOML 文件。若该变量未设置，`web_search` 保持不可用，模型与其它功能照常工作，
`~/.leveler/leveler.log` 会记录一条点名该变量的警告。

## 终端界面

- 用户消息改为一整条铺满阅读宽度的背景带：深色主题使用比 `#111315` 主背景略亮的 `#171A1E`，柔和蓝色 `❯` 位于首行，正文保持主色。短消息与长消息宽度一致，背景带高度随消息自然增长，不显示身份文字、边框或分割线。
- 选中背景带复制时仍只得到原始文本；背景、标记与标记栏不会进入剪贴板。

## 更新

- Homebrew 安装的版本不再自行替换 Cellar 中的文件。启动时的自动更新与 `leveler update` 都改为提示执行 `brew upgrade leveler`，让 Formula 的记录与实际安装文件保持一致；`leveler update --check` 仍只做只读检查。安装脚本或手动复制的版本保留原有自更新能力。

## 验收

- Dogfood 覆盖率审计在给出 pre-RC 结论前，先把本轮各层产物写入运行目录。L1 已经通过的测试不会再被报告为“L1 did not run for this candidate”。
- 覆盖证据与审计命令行指定的候选绑定：运行目录声明的候选不一致时，该证据被拒绝，而不是记到当前候选头上。

## 修复

- TUI 展示首条搜索结果，不再显示外部数据占位符。
- 已配置搜索但凭据变量未设置时，以 warn 级别记录（原先是 debug，默认过滤器不会写出）。
