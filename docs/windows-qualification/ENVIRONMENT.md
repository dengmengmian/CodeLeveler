# Windows 11 真机环境身份

采集时间：2026-11-15 22:00 至 2026-11-16 10:30（UTC+08:00）
采集方式：`Get-CimInstance`、注册表、`whoami`、`Get-AppxPackage`、`Get-WinUserLanguageList`、工具自身 `--version`，原始证据见 `evidence/environment.txt` 与 `evidence/whoami-groups.txt`。

## 机器与操作系统

| 项目 | 值 |
| --- | --- |
| 机型 | LENOVO 20UFA008CD |
| CPU | AMD Ryzen 5 PRO 4650U，6 核 / 12 逻辑处理器 |
| 内存 | 16,349,057,024 bytes（约 15.2 GiB） |
| 系统盘 | C: 476 GB，可用约 367 GB |
| 系统 Caption | Microsoft Windows 11 家庭中文版（`Win32_OperatingSystem.Caption`） |
| 注册表 ProductName | `Windows 10 Home Single Language`（Windows 11 的兼容字段，Caption 与构建号为准） |
| DisplayVersion | 26H2 |
| Build / UBR | 26300.9711 |
| EditionID | CoreSingleLanguage |
| 架构 | x64（AMD64），64 位操作系统 / 64 位进程 |
| 最后启动 | 2026-11-15 18:41:43 |

## 终端宿主

| 项目 | 值 |
| --- | --- |
| 实际当前宿主 | Windows Terminal（当前进程 `WT_SESSION` 存在；窗口为 WT 1.25.3131.0） |
| Windows Terminal | 1.25.3131.0（Appx `Microsoft.WindowsTerminal_1.25.3131.0_x64__8wekyb3d8bbwe`） |
| Console Host 二进制 | `C:\Windows\System32\conhost.exe` 10.0.26100.9949 |
| 默认终端委派 | `HKCU\Console\%%Startup` 未显式设置；Windows 11 默认把控制台应用委派给 Windows Terminal |
| 显示 | 物理 1920×1080，DPI 144（150% 缩放）；DPI 未感知进程看到 1280×720 |
| 激活水印 | 桌面右下角有“激活 Windows”水印，属系统环境，截图中可见 |

Windows 11 默认将传统控制台应用委派给 Windows Terminal。本次未修改 `HKCU\Console\%%Startup` 或其他用户终端设置，因此**独立 Console Host 窗口矩阵标记为 BLOCKED / NOT VERIFIED**，而不是推断为通过。

## Shell、工具链与输入法

| 项目 | 值 |
| --- | --- |
| PowerShell | 5.1.26100.9711（Windows PowerShell） |
| PowerShell 7 (`pwsh`) | 未安装 |
| VS Code (`code`) | 未安装 |
| CMD | 10.0.26300.9711 |
| Git Bash | GNU bash 5.3.15 (x86_64-pc-cygwin) |
| Git | 2.56.0.windows.4 |
| Rust | rustc 1.90.0 (1159e78c4 2025-09-14)，host `x86_64-pc-windows-msvc` |
| Cargo | 1.90.0 (840b83a10 2025-07-30) |
| 系统 Python | 未安装（仅有 Microsoft Store 别名占位）；测试文档工具使用便携式 CPython 3.12.10 |
| WSL | 未安装 Linux 发行版 |
| ripgrep (`rg`) | 未安装；产品按契约回退到内置实现 |
| 用户语言 | `zh-Hans-CN`；已安装 Microsoft Pinyin（输入法 tip 存在），但本轮未驱动 IME 组合/候选 |
| Windows Defender | 实时防护开启；本轮未关闭、未添加排除。新建的 CLI 测试二进制曾被误报为 `TrojanDropper:Win32/Sirefef`（os error 225），作为环境限制记录 |

## 区域、编码与路径

| 项目 | 值 |
| --- | --- |
| 用户区域 | zh-CN（中文，中国） |
| ANSI / OEM 代码页 | 936（GBK / CP936），`chcp` 输出 936 |
| 系统语言 | 简体中文 |
| 长路径支持 | `LongPathsEnabled=0`（未启用） |
| 用户目录 | `C:\Users\code`（无中文 / 空格） |
| TEMP / TMP | `C:\Users\code\AppData\Local\Temp` |
| 测试工作区 | `C:\qlab`、`C:\qlab\repos\中文 工程`、`C:\qlab\repos\库存 对账`、本轮 `C:\qlab\ts-final-7966c829\工作 空间` |
| 仓库路径 | `C:\CodeLeveler`（ASCII，短） |

## 用户身份与完整性级别

| 项目 | 值 |
| --- | --- |
| 账户 | `DESKTOP-6EGC57E\code`（SID `S-1-5-21-3985448448-3491830538-3645417405-1001`） |
| Administrators 组 | 是，但在过滤后的非提升 token 中为“只用于拒绝的组” |
| 当前 token 是否提升 | 否（`IsInRole(Administrator) = False`） |
| 当前进程完整性级别 | Medium（`Mandatory Label\Medium Mandatory Level`，S-1-16-8192） |
| 本机启用的本地账户 | 仅 `code`；Administrator/Guest/DefaultAccount/WDAGUtilityAccount 均禁用 |
| 独立标准用户（非 Administrators） | 未准备；创建需要管理员授权，未获授权，标记 BLOCKED |
| 受限子进程完整性级别 | Low（`S-1-16-4096`，由 `leveler-confine.exe` 启动，实测） |
| 定时动作子进程 | 同一用户、Low Integrity（Assisted 写沙箱）；宿主为 Medium |

本次安全验收使用“管理员组成员 + 非提升 Medium token”的真实会话，不能等同于独立标准用户；报告对该行单独标记，不将其外推为标准用户 PASS。

## 二进制与候选

| 项目 | 值 |
| --- | --- |
| 用户已安装二进制 | `C:\Users\code\.local\bin\leveler.exe`，仍为上一候选 `1.0.27 (4bc082963fee)`；本轮未覆盖 |
| 上一候选归档 | `C:\qlab\artifacts\leveler-v1.0.27-x86_64-pc-windows-msvc.zip`（4bc08296，保留不动） |
| 本轮最终候选归档 | `C:\qlab\artifacts\candidate-7966c829\leveler-v1.0.27-x86_64-pc-windows-msvc.zip` |
| 本轮最终候选解压 | `C:\qlab\install\candidate-7966c829\leveler-v1.0.27-x86_64-pc-windows-msvc\leveler.exe` |
| 用途 | 最终业务验收只使用从正式 ZIP 解压的字节；debug 构建仅用于功能定位 |

## 未修改的系统状态

本次未修改 Windows Defender、UAC、注册表安全策略、PowerShell 执行策略、系统终端默认设置、全局 Git 配置、用户字体或配色，也未删除/覆盖用户已有配置或会话。所有测试使用 `C:\qlab` 下的隔离 `LEVELER_HOME`、工作区、config 与证据目录；本轮创建的 Task Scheduler 任务均在验收后删除。
