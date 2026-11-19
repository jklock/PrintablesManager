/**
 * Minimal, safe Markdown renderer for model descriptions.
 *
 * Printables descriptions are stored as markdown-ish text (headings, lists,
 * links, emphasis). This converts that to HTML for the detail view and the
 * editor preview. Input is escaped FIRST, so nothing in a description can
 * inject markup or script.
 *
 * Supported: headings, bold, italic, inline code, links, unordered and ordered
 * lists, blockquotes, horizontal rules, paragraphs.
 */

export function escapeHtml(value) {
  return String(value == null ? '' : value).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;'
  }[c]));
}

const INLINE_RULES = [
  // `code` first so its contents are not touched by emphasis rules.
  [/`([^`]+)`/g, '<code>$1</code>'],
  [/\*\*([^*]+)\*\*/g, '<strong>$1</strong>'],
  [/(^|[^*])\*([^*\n]+)\*/g, '$1<em>$2</em>'],
  // Only treat _x_ as emphasis at word boundaries, so snake_case_name and
  // file_names stay literal.
  [/(^|[\s(])_([^_\n]+)_(?=[\s).,!?:;]|$)/g, '$1<em>$2</em>'],
  // [label](url) — url is already escaped, but reject javascript: schemes.
  [/\[([^\]]+)\]\((?!javascript:)([^)\s]+)\)/g,
    '<a href="$2" target="_blank" rel="noreferrer noopener">$1</a>']
];

export function renderInline(escaped) {
  let out = escaped;
  for (const [pattern, replacement] of INLINE_RULES) {
    out = out.replace(pattern, replacement);
  }
  return out;
}

export function renderMarkdown(text) {
  const raw = String(text == null ? '' : text).replace(/\r\n/g, '\n').trim();
  if (!raw) return '';

  const lines = escapeHtml(raw).split('\n');
  const html = [];
  let listType = null;

  const closeList = () => {
    if (listType) {
      html.push(`</${listType}>`);
      listType = null;
    }
  };

  for (const line of lines) {
    const trimmed = line.trim();

    if (!trimmed) {
      closeList();
      continue;
    }

    const heading = trimmed.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      closeList();
      const level = Math.min(heading[1].length + 2, 6); // h1 in a description is too loud
      html.push(`<h${level}>${renderInline(heading[2])}</h${level}>`);
      continue;
    }

    if (/^(-{3,}|\*{3,}|_{3,})$/.test(trimmed)) {
      closeList();
      html.push('<hr />');
      continue;
    }

    // Input was escaped before parsing, so a markdown ">" is now "&gt;".
    const quote = trimmed.match(/^&gt;\s?(.*)$/);
    if (quote) {
      closeList();
      html.push(`<blockquote>${renderInline(quote[1])}</blockquote>`);
      continue;
    }

    const unordered = trimmed.match(/^[-*+]\s+(.*)$/);
    if (unordered) {
      if (listType !== 'ul') {
        closeList();
        html.push('<ul>');
        listType = 'ul';
      }
      html.push(`<li>${renderInline(unordered[1])}</li>`);
      continue;
    }

    const ordered = trimmed.match(/^\d+[.)]\s+(.*)$/);
    if (ordered) {
      if (listType !== 'ol') {
        closeList();
        html.push('<ol>');
        listType = 'ol';
      }
      html.push(`<li>${renderInline(ordered[1])}</li>`);
      continue;
    }

    closeList();
    html.push(`<p>${renderInline(trimmed)}</p>`);
  }

  closeList();
  return html.join('\n');
}
