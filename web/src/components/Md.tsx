import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'

/** Room-stream markdown — react-markdown + GFM (tables, task lists,
 *  strikethrough). Raw HTML is never rendered (react-markdown
 *  default), which is the whole reason a library beats a hand-rolled
 *  parser here. Styling maps to our token classes so messages read
 *  like the rest of the dashboard rather than a browser default. */
export function Md({ text, className = '' }: { text: string; className?: string }) {
  return (
    <div className={`space-y-1.5 ${className}`}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          h1: (p) => <p className="pt-1 text-[14px] font-semibold text-ink-1" {...strip(p)} />,
          h2: (p) => <p className="pt-1 text-[13px] font-semibold text-ink-1" {...strip(p)} />,
          h3: (p) => <p className="pt-0.5 text-[12.5px] font-semibold text-ink-1" {...strip(p)} />,
          h4: (p) => <p className="text-[12px] font-semibold text-ink-1" {...strip(p)} />,
          p: (p) => <p className="my-0.5" {...strip(p)} />,
          ul: (p) => <ul className="list-disc space-y-0.5 pl-4" {...strip(p)} />,
          ol: (p) => <ol className="list-decimal space-y-0.5 pl-4" {...strip(p)} />,
          li: (p) => <li className="pl-0.5" {...strip(p)} />,
          blockquote: (p) => (
            <blockquote className="border-l-2 border-edge pl-3 text-ink-3" {...strip(p)} />
          ),
          hr: () => <div className="hairline-x my-2" />,
          a: (p) => (
            <a
              {...strip(p)}
              target="_blank"
              rel="noreferrer"
              className="text-signal-fg underline decoration-signal-bd underline-offset-2 hover:decoration-signal-fg"
            />
          ),
          strong: (p) => <strong className="font-semibold text-ink-1" {...strip(p)} />,
          em: (p) => <em className="italic" {...strip(p)} />,
          code: ({ className: c, children, ...rest }) => (
            // inline code; fenced blocks arrive via `pre` wrapping
            <code
              className={`font-num rounded bg-hover-2 px-1 py-px text-[11px] text-ink-1 ${c ?? ''}`}
              {...rest}
            >
              {children}
            </code>
          ),
          pre: (p) => (
            <pre
              className="font-num overflow-x-auto rounded-lg bg-hover p-2.5 text-[11px] leading-relaxed ring-1 ring-edge-soft"
              {...strip(p)}
            />
          ),
          table: (p) => (
            <div className="overflow-x-auto">
              <table
                className="font-num w-full border-collapse text-[11px] leading-relaxed"
                {...strip(p)}
              />
            </div>
          ),
          th: (p) => (
            <th
              className="border-b border-edge px-2 py-1 text-left font-medium text-ink-3"
              {...strip(p)}
            />
          ),
          td: (p) => (
            <td
              className="border-b border-edge-soft px-2 py-1 text-ink-2 last:[tr:last-child>&]:border-0"
              {...strip(p)}
            />
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  )
}

/** react-markdown injects a `node` prop that must not reach the DOM. */
function strip<T extends { node?: unknown }>({ node: _node, ...rest }: T): Omit<T, 'node'> {
  return rest
}
