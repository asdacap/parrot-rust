import { memo, useDeferredValue } from "react"
import ReactMarkdown, { type Components } from "react-markdown"
import rehypeHighlight from "rehype-highlight"
import remarkGfm from "remark-gfm"

import { cn } from "@/lib/utils"

const components: Components = {
  p: ({ children }) => <p className="my-2 leading-relaxed">{children}</p>,
  a: ({ children, href }) => <a href={href} target="_blank" rel="noreferrer" className="underline">{children}</a>,
  ul: ({ children }) => <ul className="my-2 list-disc pl-6">{children}</ul>,
  ol: ({ children }) => <ol className="my-2 list-decimal pl-6">{children}</ol>,
  h1: ({ children }) => <h1 className="my-3 text-lg font-semibold">{children}</h1>,
  h2: ({ children }) => <h2 className="my-3 text-base font-semibold">{children}</h2>,
  h3: ({ children }) => <h3 className="my-2 font-semibold">{children}</h3>,
  pre: ({ children }) => <pre className="my-2 overflow-x-auto rounded-md bg-muted p-3 text-xs">{children}</pre>,
  code: ({ children, className }) => <code className={cn("rounded-sm bg-muted px-1 font-mono text-xs", className)}>{children}</code>,
  table: ({ children }) => <table className="my-2 border-collapse text-xs">{children}</table>,
  th: ({ children }) => <th className="border px-2 py-1 text-left font-semibold">{children}</th>,
  td: ({ children }) => <td className="border px-2 py-1">{children}</td>,
  blockquote: ({ children }) => <blockquote className="my-2 border-l-2 pl-3 text-muted-foreground">{children}</blockquote>,
}

const remarkPlugins = [remarkGfm]
const rehypePlugins = [rehypeHighlight]

export const Markdown = memo(function Markdown({ text }: { text: string }) {
  const deferredText = useDeferredValue(text)
  return (
    <ReactMarkdown components={components} remarkPlugins={remarkPlugins} rehypePlugins={rehypePlugins}>
      {deferredText}
    </ReactMarkdown>
  )
})
