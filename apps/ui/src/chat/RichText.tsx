import type { ReactNode } from 'react';

/** Small, dependency-free renderer: fenced code, inline code, and bold. */
export function RichText({ text }: { text: string }) {
  return <>{renderBlocks(text)}</>;
}

function renderBlocks(text: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  const fence = /```([\w+-]*)\r?\n?([\s\S]*?)```/g;
  let cursor = 0;
  let match: RegExpExecArray | null;
  let key = 0;
  while ((match = fence.exec(text)) !== null) {
    if (match.index > cursor) {
      nodes.push(
        <span key={`text-${key++}`} className="rich-text">
          {renderInline(text.slice(cursor, match.index))}
        </span>,
      );
    }
    nodes.push(
      <pre key={`code-${key++}`} className="rich-code" data-language={match[1] || undefined}>
        <code>{match[2].replace(/\n$/, '')}</code>
      </pre>,
    );
    cursor = match.index + match[0].length;
  }
  if (cursor < text.length) {
    nodes.push(
      <span key={`text-${key++}`} className="rich-text">
        {renderInline(text.slice(cursor))}
      </span>,
    );
  }
  return nodes;
}

function renderInline(text: string): ReactNode[] {
  const parts = text.split(/(`[^`]+`|\*\*[^*]+\*\*)/g);
  return parts.map((part, index) => {
    if (part.startsWith('`') && part.endsWith('`') && part.length > 2) {
      return (
        <code key={index} className="inline-code">
          {part.slice(1, -1)}
        </code>
      );
    }
    if (part.startsWith('**') && part.endsWith('**') && part.length > 4) {
      return <strong key={index}>{part.slice(2, -2)}</strong>;
    }
    return part;
  });
}
