# CodeLeveler brand mark

Minimal geometric **CL** monogram for CodeLeveler — an AI coding agent that helps developers level up their code.

## Concept

| Letter | Meaning | Form |
|--------|---------|------|
| **C** | Code | Open continuous ring — code flow & iteration |
| **L** | Level | Rising steps — progression & improvement |

The two forms share a spine so they read as one symbol, not plain typography.

## Files

| File | Use |
|------|-----|
| `codeleveler-mark.svg` | Primary vector (uses `currentColor`) |
| `codeleveler-mark-black.png` | Black mark, transparent |
| `codeleveler-mark-white.png` | White mark, transparent |
| `codeleveler-mark-bw.png` | Black on white |
| `codeleveler-app-icon.svg` / `.png` | Dark app icon |
| `codeleveler-app-icon-light.png` | Light app icon |
| `codeleveler-app-icon-{16..512}.png` | App icon sizes |
| `codeleveler-favicon-{16..512}.png` | Favicon sizes |
| `codeleveler-terminal.svg` / `*-32.png` / `*-64.png` | Terminal / TUI |

## Rules

- Flat monochrome (or single brand color via `currentColor`)
- No gradients, mascots, robots, brains, or circuit clichés
- Must stay legible at 16×16

## Usage

```html
<!-- Inherit color from parent -->
<img src="codeleveler-mark.svg" width="32" alt="CodeLeveler" style="color: #0B0F14" />
```

In CSS:

```css
.logo { color: #0B0F14; }
.logo-dark { color: #fff; }
```
