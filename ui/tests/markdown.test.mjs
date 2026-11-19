import test from 'node:test';
import assert from 'node:assert/strict';

import { escapeHtml, renderMarkdown, renderInline } from '../dist/js/markdown.js';

test('escapes HTML so descriptions cannot inject markup', () => {
  assert.equal(
    escapeHtml('<img src=x onerror="alert(1)">'),
    '&lt;img src=x onerror=&quot;alert(1)&quot;&gt;'
  );
  const html = renderMarkdown('<script>alert(1)</script>');
  assert.ok(!html.includes('<script>'), html);
  assert.ok(html.includes('&lt;script&gt;'));
});

test('headings are demoted so a description does not shout', () => {
  assert.equal(renderMarkdown('# Title'), '<h3>Title</h3>');
  assert.equal(renderMarkdown('## Print settings'), '<h4>Print settings</h4>');
});

test('paragraphs, bold, italic and inline code', () => {
  assert.equal(
    renderMarkdown('Use **0.2mm** layer height with *care* and `PETG`.'),
    '<p>Use <strong>0.2mm</strong> layer height with <em>care</em> and <code>PETG</code>.</p>'
  );
});

test('links are rendered and javascript: schemes are refused', () => {
  assert.equal(
    renderMarkdown('See [the profile](https://example.com/p).'),
    '<p>See <a href="https://example.com/p" target="_blank" rel="noreferrer noopener">the profile</a>.</p>'
  );
  const html = renderMarkdown('[x](javascript:alert(1))');
  assert.ok(!html.includes('href="javascript:'), html);
});

test('ordered and unordered lists close correctly', () => {
  assert.equal(
    renderMarkdown('- one\n- two\n\nafter'),
    '<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n<p>after</p>'
  );
  assert.equal(
    renderMarkdown('1. first\n2) second'),
    '<ol>\n<li>first</li>\n<li>second</li>\n</ol>'
  );
});

test('blockquotes, rules and empty input', () => {
  assert.equal(renderMarkdown('> note'), '<blockquote>note</blockquote>');
  assert.equal(renderMarkdown('---'), '<hr />');
  assert.equal(renderMarkdown(''), '');
  assert.equal(renderMarkdown(null), '');
});

test('multiple blank lines do not produce empty paragraphs', () => {
  assert.equal(renderMarkdown('a\n\n\n\nb'), '<p>a</p>\n<p>b</p>');
});

test('renderInline leaves unmatched markers alone', () => {
  assert.equal(renderInline('2 * 3 = 6'), '2 * 3 = 6');
  assert.equal(renderInline('snake_case_name'), 'snake_case_name');
});
