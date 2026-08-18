# CodeLeveler Terminal Brand System V1

TUI 的正式主标识是 **Level Mark**，不是吉祥物、不是幽灵、也不是字母 CL。

Splash 里的关系永远是：

```text
[ Level Mark ]  CodeLeveler
```

图形与 Wordmark 分开。Mark 内部不写 CL / CODE / 字母。

App 图标仍使用 `assets/brand/` 里的 CL 字母 monogram（C 开环 + L 台阶）。那是图标系统，不是 Terminal 主标识。

---

## 为什么是 Level Mark

CodeLeveler 做的事是：把模型能力收成可靠执行。

Level Mark 同时带三个意思：

| 语义 | 怎么看 |
| --- | --- |
| **Level** | 自上而下逐级加宽，能力往上走 |
| **Execution** | 左下是独立的 foundation pier，能力不是漂着的 |
| **Forward** | 底行右侧 runway 伸出脊柱，有向前的动势 |

它必须不像：Wi-Fi、信号格、柱状图、楼梯 icon、云厂商 logo、可爱角色。

颜色是第二位的。关掉颜色以后，几何本身还得能认。

---

## 选定的几何：Candidate A「Cantilever」

三个候选都按同一方向画过，并在 monospace 里比过：

```text
A  Cantilever (选定)          B  Spine Rise               C  Tall Wedge
        ██                           ██                           ██
      ████                         ████                         ████
    ██████                       ██████                       ██████
  ████  ██                      ████ ███                    ████  ██
██      █████                 ██    █████                  ██      ███
                                                           ██       █████
```

| 候选 | 判断 |
| --- | --- |
| A | 缺口清楚，左墩和右跑道分开，5 行就能认，不像楼梯 |
| B | 缺口太窄，右侧容易读成实心台阶 |
| C | 多一行更有存在感，但底两行开始像坡/楼梯 |

选定 A。生产代码里的 Master 不是提示词里的草图原样：画布加到 13 列，terrace 内收一格，底行空隙拉开，给 forward bar 留出右列。

实现：`crates/leveler-tui/src/brand.rs`。

---

## 三档尺寸

同一套 DNA，不是三个 logo。

### Master — 13×5

用于宽终端 Splash、以后的 About / 大 Empty State。

```text
        ██
      ████
    ██████
  ████  ██
██      █████
```

### Compact — 8×4

用于 56–71 列。

```text
     ██
   ████
 ██  ██
██  ████
```

### Micro — 5×2

用于窄终端。不硬压 Master。

```text
  ██
█ ███
```

只使用空格和全块 `█`（U+2588）。不依赖 Nerd Font，不走图片协议，不用 emoji。

---

## Wordmark 与文案

| 角色 | 规则 |
| --- | --- |
| Wordmark | `CodeLeveler`（这个大小写）。不是 `CODELEVELER` / `codeleveler` / `Code Leveler` |
| Version | `v0.1.4` 这种，muted，不属于 Wordmark |
| 中文 tagline | 让模型真正可靠地完成任务 |
| 英文 tagline | Make models reliably complete tasks |
| 中文 CTA | › 输入任务，直接开始 |
| 英文 CTA | › Start by entering a task |
| 更多 | 中文「输入 / 查看更多命令」；英文「Type / for more commands」。`/` 用 accent，其余 muted |

一个 Splash 只显示一种语言。`Locale::Zh` / `Locale::En`。

启动页只放三个命令：`/feature-dev` `/model` `/help`。不要为了把卡片撑大再塞 `/plan` `/goal` `/skill`。

---

## 颜色

最多三级离散 token，定义在 `Theme.brand`，禁止在 splash renderer 里写 `Color::Rgb` / `Color::Blue`。

| Token | 角色 | Dark | Light | High Contrast |
| --- | --- | --- | --- | --- |
| `brand.foundation` | 底 / pier / runway | `#2F7FE0` | `#0550AE` | `#4DA6FF` |
| `brand.primary` | 中层 | `#4EA1FF` | `#0969DA` | `#80C0FF` |
| `brand.highlight` | 顶 / apex | `#8EC8FF` | `#218BFF` | `#B8DEFF` |

`NO_COLOR` 下三个 token 都是 `Reset`。几何还在。

Wordmark 用 `accent.primary` + bold；version 用 `text.muted`；CTA 正文用 `text.primary` + bold；`›` 用 `accent.primary`。

---

## Splash 布局

这是 hero empty state，不是 tooltip，也不是欢迎海报。

| 项 | 值 |
| --- | --- |
| 目标宽 | 76–82，推荐 80，上限 86 |
| 目标高 | 15–17，推荐 16（随 viewport 算，不写死） |
| 100×30 / 120×36 | 宽约 80（约 65–80% 可用宽），不再是屏幕中央小方块 |
| 80×24 | 卡片约 76 宽，两侧留边 |

外框居中之后，**Level Mark + Main Content** 是独立的 `hero_group`，在 card inner rect 里再水平、垂直居中。不要把组钉在 `card.x + fixed_offset`。不要靠缩小 Card 来消灭右侧空白。

| 项 | Wide |
| --- | --- |
| Logo | 12–16 cells（Master 13） |
| Gap | 4–5 cells（Wide 5） |
| Content column | 40–44 cells（Wide 42），内部左对齐 |
| Hero group | 58–64 cells（13+5+42=60） |

Brand / CTA / Commands / More / Trust Warning 共用同一个 `content_x`。Warning 是独立 section，与 More 至少隔 1 行；显示或隐藏都按实际 `content_height` 重算，不留固定空洞。

| 断点 | 条件 | Mark | 排布 |
| --- | --- | --- | --- |
| Wide | 宽 ≥ 72 且高 ≥ 14 | Master，gap 5 | hero_group 在 inner 居中 |
| Medium | 宽 56–71 | Compact，gap 2 | 仍左右，不立刻改成上下 |
| Narrow | 宽 40–55 | Micro | 仍尽量左右 |
| Stack | 宽 < 40 或高 < 10 | 隐藏 | 只保留文案 |

Wordmark 永远比图形优先。Mark 会挤掉 tagline / 命令时，整块隐藏图形。

命令列按 terminal display width 对齐（`UnicodeWidthStr` / `pad_to_width`），禁止 `String::len()`。

---

## 模块边界

```text
brand.rs          Level Mark 几何 + semantic ink
theme.rs          BrandColors 与其它 token 一起定义
i18n.rs           tagline / CTA / more
splash.rs         卡片、断点、命令、信任提示
conversation/*    空会话时画 splash
```

`brand.rs` 不碰 layout、i18n、slash registry、runtime。

---

## 明确不做

- Ghost / 小熊猫 / 任何吉祥物当主 Logo
- 把 CL 字母塞进 Mark
- Nerd Font / 图片协议 / 六色渐变
- 改 slash、reasoning、permission、work-mode、footer、runtime
- 同一屏中英混排

---

## 维护

改 Mark 先改 `brand.rs` 里的三档常量，再跑：

```sh
cargo test -p leveler-tui --lib brand
cargo test -p leveler-tui --lib splash
cargo test -p leveler-tui --lib dump_hero -- --ignored --nocapture
```

几何变了就同步这个文件和 `assets/brand/logo_ascii.txt`。
