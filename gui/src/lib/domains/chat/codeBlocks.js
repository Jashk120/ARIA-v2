/**
 * Fenced-code-block detection for chat handoffs.
 *
 * Scans markdown text for ``` fenced blocks and classifies each one:
 * - `solidity` — lang is `solidity`, or the body matches
 *   `/pragma solidity|contract\s+\w+\s*\{/i`
 * - `hts` — lang is js/ts/javascript/typescript AND the body matches
 *   `/hedera|hashgraph|TokenCreate|TokenId|hts|@hashgraph/i`
 * - `other` — everything else (callers skip these: no handoff button)
 *
 * @param {string} text
 * @returns {Array<{ lang: string, code: string, kind: 'solidity' | 'hts' | 'other' }>}
 */
export function findCodeBlocks(text) {
  if (typeof text !== 'string' || !text.includes('```')) return [];
  const blocks = [];
  const fence = /```(\w[\w+-]*?)?[ \t]*\n([\s\S]*?)(?:```|$)/g;
  let m;
  while ((m = fence.exec(text)) !== null) {
    const lang = (m[1] ?? '').toLowerCase();
    const code = m[2].replace(/\n$/, '');
    if (!code.trim()) continue;
    blocks.push({ lang, code, kind: classifyBlock(lang, code) });
  }
  return blocks;
}

/**
 * @param {string} lang
 * @param {string} code
 * @returns {'solidity' | 'hts' | 'other'}
 */
function classifyBlock(lang, code) {
  if (lang === 'solidity' || /pragma solidity|contract\s+\w+\s*\{/i.test(code)) {
    return 'solidity';
  }
  const isJs = ['js', 'ts', 'javascript', 'typescript'].includes(lang);
  if (isJs && /hedera|hashgraph|TokenCreate|TokenId|hts|@hashgraph/i.test(code)) {
    return 'hts';
  }
  return 'other';
}
