# CodeLeveler mascot

`codeleveler-mascot.png` is the original user-supplied brand illustration
(a waving otter in a hoodie and headphones), copied unchanged from
`代码水獭欢迎挥手 mascots-2.png`. It is 1312×1199 and has a real alpha channel.
Reference screenshots are layout references, not image assets.

The TUI embeds the original PNG at compile time and displays it using a native
terminal image protocol when the terminal is supported. It does not convert the
illustration to colored character art. A small terminal, a monochrome theme, or
an unsupported terminal uses the existing Level Mark instead.

Native presentation uses `ratatui-image` 8.0.2 with Kitty graphics for Kitty and
Ghostty, and the iTerm2 image protocol for iTerm2 and WezTerm. Detection uses
the terminal's explicit identity without opening a second input reader.
Unknown terminals and tmux/screen sessions use the Level Mark. The image is
decoded once, embedded in the binary, and fitted to the available area with
its aspect ratio and alpha channel preserved. The welcome card sizes the image
slot from the decoded source dimensions, so replacing the PNG keeps the
layout correct without touching hard-coded ratios.

Keep the original resource in the repository so packaged binaries do not depend
on a user's Desktop folder. Do not bake a dark or light background into it.
