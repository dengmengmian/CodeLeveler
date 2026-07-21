// 品牌 CL 标记（内联自 design/mockup.html，源自 assets/brand/codeleveler-mark.svg）

export function BrandMark({ label = 'CodeLeveler' }: { label?: string }) {
  return (
    <svg viewBox="0 0 64 64" fill="currentColor" aria-label={label} role="img">
      <path d="M 27 9 A 23 23 0 1 0 27 55 L 27 47 A 15 15 0 1 1 27 17 Z" />
      <path d="M 29 22 H 37 V 54 H 29 Z M 29 46 H 51 V 54 H 29 Z M 29 38 H 47 V 46 H 29 Z M 29 30 H 43 V 38 H 29 Z" />
    </svg>
  );
}
