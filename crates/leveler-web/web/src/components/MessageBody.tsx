// Markdown 消息体：react-markdown + remark-gfm；流式消息末尾挂光标块。
// 定制渲染：
// - 表格包 .table-wrap：宽表格在工作区内横向滚动
// - 代码块右上角「复制」按钮
// - 行内 code 形如 `path/to/file.rs:128` 时渲染为文件引用 chip，点击打开文件查看弹层

import { useRef } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { useOpenFile } from './FileViewer';

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
  return (
    <div className="pre-wrap">
      <pre ref={ref} {...rest}>
        {children}
      </pre>
      <button
        className="pre-copy"
        title="复制代码"
        onClick={() => {
          const text = ref.current?.innerText ?? '';
          void navigator.clipboard.writeText(text);
        }}
      >
        复制
      </button>
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
