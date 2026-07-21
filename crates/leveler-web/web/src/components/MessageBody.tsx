// Markdown 消息体：react-markdown + remark-gfm；流式消息末尾挂光标块。
// 定制渲染：
// - 表格包 .table-wrap：宽表格在工作区内横向滚动
// - 代码块右上角「复制」按钮
// - 行内 code 形如 `path/to/file.rs:128` 时渲染为文件引用 chip，点击打开文件查看弹层

import { useRef } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { CopyButtonLazy } from './CopyButton';
import { DiffBlock } from './DiffBlock';
import { useOpenFile } from './FileViewer';

/** 提取 ``` 围栏代码块的语言标记（class 形如 `language-diff`）。 */
function fenceLanguage(children: React.ReactNode): string | null {
  const child = Array.isArray(children) ? children[0] : children;
  if (child && typeof child === 'object' && 'props' in child) {
    const cls = (child as { props?: { className?: string } }).props?.className ?? '';
    const m = /language-(\w+)/.exec(cls);
    return m ? m[1] : null;
  }
  return null;
}

/** 取围栏代码块的纯文本内容。 */
function fenceText(children: React.ReactNode): string {
  const child = Array.isArray(children) ? children[0] : children;
  const kids = (child as { props?: { children?: unknown } })?.props?.children;
  return String(Array.isArray(kids) ? kids.join('') : (kids ?? ''));
}

/** 仓库相对路径（必须含 / 和扩展名），可带 :行号 */
const FILE_REF = /^([\w.\-/]+\/[\w.\-/]*\.\w{1,12})(?::(\d+))?$/;

function TableWrap(props: React.ComponentPropsWithoutRef<'table'> & { node?: unknown }) {
  const { node: _node, children, ...rest } = props;
  return (
    <div className="table-wrap">
      <table {...rest}>{children}</table>
    </div>
  );
}

function PreBlock(props: React.ComponentPropsWithoutRef<'pre'> & { node?: unknown }) {
  const { node: _node, children, ...rest } = props;
  const ref = useRef<HTMLPreElement>(null);
  // ```diff / ```patch 围栏 → 真正的 DiffViewer（着色 + 行号 + 复制完整 diff），
  // 而非普通 pre/code。其余语言仍走普通代码块。
  const lang = fenceLanguage(children);
  if (lang === 'diff' || lang === 'patch') {
    return <DiffBlock source={fenceText(children)} />;
  }
  return (
    <div className="pre-wrap">
      <pre ref={ref} {...rest}>
        {children}
      </pre>
      <CopyButtonLazy className="pre-copy" getText={() => ref.current?.innerText ?? ''} />
    </div>
  );
}

function CodeInline(props: React.ComponentPropsWithoutRef<'code'> & { node?: unknown }) {
  const { node: _node, className, children, ...rest } = props;
  const openFile = useOpenFile();
  const text = String(children ?? '');
  const m = FILE_REF.exec(text);
  if (m) {
    return (
      <button
        className="file-ref"
        title="点击打开文件"
        onClick={() => openFile(m[1], m[2] ? Number(m[2]) : undefined)}
      >
        {text}
      </button>
    );
  }
  return (
    <code className={className} {...rest}>
      {children}
    </code>
  );
}

const components: Components = {
  table: TableWrap,
  pre: PreBlock,
  code: CodeInline,
};

export function MessageBody({ text, streaming }: { text: string; streaming: boolean }) {
  return (
    <div className="body">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {text}
      </ReactMarkdown>
      {streaming && <span className="cursor" />}
    </div>
  );
}
