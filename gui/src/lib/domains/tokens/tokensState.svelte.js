import { tauriInvoke, tauriListen } from '$lib/services/tauri.js';
import { navigationState } from '$lib/domains/shell/navigationState.svelte.js';

const REAL_ESTATE_FUND_SOURCE = `import { TokenCreateTransaction, TokenType, TokenSupplyType } from "@hashgraph/sdk";

// RealEstateFund — fungible HTS sample (GUI-local preview, not submitted).
const tx = new TokenCreateTransaction()
  .setTokenName("RealEstateFund")
  .setTokenSymbol("REF")
  .setTokenType(TokenType.FungibleCommon)
  .setDecimals(2)
  .setInitialSupply(1_000_000)
  .setSupplyType(TokenSupplyType.Finite)
  .setMaxSupply(10_000_000)
  .setTreasuryAccountId("0.0.12345")
  .setTokenMemo("Fractional real-estate fund");

// const receipt = await (await tx.execute(client)).getReceipt(client);
// console.log("tokenId:", receipt.tokenId?.toString());
`;

const PROPERTY_DEED_SOURCE = `import { TokenCreateTransaction, TokenType, TokenSupplyType } from "@hashgraph/sdk";

// PropertyDeed — NFT HTS sample (GUI-local preview, not submitted).
const tx = new TokenCreateTransaction()
  .setTokenName("PropertyDeed")
  .setTokenSymbol("DEED")
  .setTokenType(TokenType.NonFungibleUnique)
  .setDecimals(0)
  .setInitialSupply(0)
  .setSupplyType(TokenSupplyType.Finite)
  .setMaxSupply(500)
  .setTreasuryAccountId("0.0.12345")
  .setTokenMemo("On-chain property deeds");

// const receipt = await (await tx.execute(client)).getReceipt(client);
// console.log("tokenId:", receipt.tokenId?.toString());
`;

/**
 * @typedef {{ id: string, name: string, symbol: string, token_type: string, supply: string, decimals: number, memo: string | null, source: string | null, status: string, created_at: number, treasury: string | null, token_id: string | null }} TokenItem
 */

/**
 * @typedef {{ task_id: string, content: string, confirm_kind: string | null }} TokenPendingConfirm
 */

/**
 * @typedef {{ id?: string, name: string, symbol: string, token_type: string, supply: string, decimals: number, memo?: string, source?: string, status?: string, treasury?: string, token_id?: string | null }} TokenForm
 */

class TokensState {
  /** @type {TokenItem[]} */
  items = $state([]);
  loaded = $state(false);
  /** @type {string | null} */
  draftCode = $state(null);
  /** @type {string | null} */
  selectedId = $state(null);
  /** @type {string | null} */
  error = $state(null);

  /** @type {'idle' | 'running' | 'awaiting' | 'done' | 'error'} */
  tokenizeStatus = $state('idle');
  /** @type {TokenPendingConfirm | null} */
  pendingConfirm = $state(null);
  /** @type {string | null} */
  tokenizeError = $state(null);
  tokenizeResult = $state('');
  /** @type {string | null} */
  createdTokenId = $state(null);
  /** @type {{ event_type: string, payload: unknown }[]} */
  tokenizeEvents = $state([]);

  /** @type {Array<() => void>} */
  #unlisten = [];
  /** @type {TokenForm | null} */
  #lastTokenizeForm = null;

  async init() {
    if (this.#unlisten.length > 0) return;
    const off = await tauriListen('token-daemon-event', (e) => this.handleTokenEvent(e.payload));
    this.#unlisten.push(off);
  }

  destroy() {
    this.#unlisten.forEach((fn) => fn());
    this.#unlisten = [];
  }

  /** @param {any} event */
  handleTokenEvent(event) {
    const { kind, ...data } = event;
    switch (kind) {
      case 'started':
        this.tokenizeEvents = [
          ...this.tokenizeEvents,
          { event_type: 'started', payload: { content: `${data.skill_type}: ${data.task}` } }
        ];
        break;
      case 'event': {
        this.tokenizeEvents = [
          ...this.tokenizeEvents,
          { event_type: data.event_type, payload: data.payload }
        ];
        const content = data.payload?.content;
        if (data.event_type === 'observation' && typeof content === 'string') {
          const tokenId = extractTokenId(content);
          if (tokenId) this.createdTokenId = tokenId;
        }
        break;
      }
      case 'awaiting_confirmation':
        this.tokenizeStatus = 'awaiting';
        this.pendingConfirm = {
          task_id: data.task_id,
          content: data.content,
          confirm_kind: data.confirm_kind ?? null
        };
        break;
      case 'done':
        this.tokenizeStatus = 'done';
        this.tokenizeResult = data.result ?? '';
        this.pendingConfirm = null;
        if (!this.createdTokenId) {
          const tokenId = extractTokenId(this.tokenizeResult);
          if (tokenId) this.createdTokenId = tokenId;
        }
        if (this.createdTokenId && this.#lastTokenizeForm) {
          const form = this.#lastTokenizeForm;
          const tokenId = this.createdTokenId;
          this.#lastTokenizeForm = null;
          void this.markTokenized(form, tokenId);
        } else {
          this.#lastTokenizeForm = null;
        }
        break;
      case 'error':
        this.tokenizeStatus = 'error';
        this.tokenizeError = data.message ?? 'Tokenize failed.';
        this.pendingConfirm = null;
        this.#lastTokenizeForm = null;
        break;
    }
  }

  /** @param {TokenForm} token */
  async tokenize(token) {
    this.tokenizeStatus = 'running';
    this.pendingConfirm = null;
    this.tokenizeError = null;
    this.tokenizeResult = '';
    this.createdTokenId = null;
    this.tokenizeEvents = [];
    this.#lastTokenizeForm = { ...token };
    const task = buildTokenCreatePrompt(token);
    try {
      await tauriInvoke('send_token_task', { task, skill_type: 'create', task_id: null });
    } catch (e) {
      this.tokenizeStatus = 'error';
      this.tokenizeError = String(e);
      this.#lastTokenizeForm = null;
    }
  }

  /** @param {'yes' | 'no'} reply */
  async respondConfirm(reply) {
    const pending = this.pendingConfirm;
    if (!pending) return;
    const taskId = pending.task_id;
    this.tokenizeStatus = 'running';
    this.pendingConfirm = null;
    this.tokenizeError = null;
    try {
      await tauriInvoke('send_token_task', { task: reply, skill_type: 'create', task_id: taskId });
    } catch (e) {
      this.tokenizeStatus = 'error';
      this.tokenizeError = String(e);
      this.#lastTokenizeForm = null;
    }
  }

  /** @param {TokenForm} token @param {string} tokenId */
  async markTokenized(token, tokenId) {
    const id = token.id ?? `tok_${Date.now()}`;
    const existing = this.items.find((t) => t.id === id);
    await tauriInvoke('save_token', {
      id,
      name: token.name,
      symbol: token.symbol,
      token_type: token.token_type,
      supply: token.supply,
      decimals: token.decimals,
      memo: token.memo ?? '',
      source: token.source ?? existing?.source ?? '',
      status: 'tokenized',
      treasury: token.treasury ?? '',
      token_id: tokenId
    });
    this.loaded = false;
    await this.load();
    this.selectedId = id;
  }

  /** Load tokens; seed 2 samples on first run when the table is empty. */
  async load(force = false) {
    if (this.loaded && !force) return;
    this.error = null;
    try {
      let rows = /** @type {TokenItem[]} */ (await tauriInvoke('list_tokens'));
      if (rows.length === 0) {
        await this.#seed();
        rows = /** @type {TokenItem[]} */ (await tauriInvoke('list_tokens'));
      }
      this.items = rows;
      if (!this.selectedId && rows.length > 0) this.selectedId = rows[0].id;
      this.loaded = true;
    } catch (e) {
      this.error = String(e);
    }
  }

  async retry() {
    this.loaded = false;
    await this.load();
  }

  async #seed() {
    const now = Date.now();
    await tauriInvoke('save_token', {
      id: `tok_${now}_ref`,
      name: 'RealEstateFund',
      symbol: 'REF',
      token_type: 'fungible',
      supply: '1000000',
      decimals: 2,
      memo: 'Fractional real-estate fund',
      source: REAL_ESTATE_FUND_SOURCE,
      status: 'draft',
      treasury: '',
      token_id: null
    });
    await tauriInvoke('save_token', {
      id: `tok_${now + 1}_deed`,
      name: 'PropertyDeed',
      symbol: 'DEED',
      token_type: 'nft',
      supply: '500',
      decimals: 0,
      memo: 'On-chain property deeds',
      source: PROPERTY_DEED_SOURCE,
      status: 'draft',
      treasury: '',
      token_id: null
    });
  }

  /** @param {TokenForm} token */
  async save(token) {
    const id = token.id ?? `tok_${Date.now()}`;
    await tauriInvoke('save_token', {
      id,
      name: token.name,
      symbol: token.symbol,
      token_type: token.token_type,
      supply: token.supply,
      decimals: token.decimals,
      memo: token.memo ?? '',
      source: token.source ?? '',
      status: token.status ?? 'draft',
      treasury: token.treasury ?? '',
      token_id: token.token_id ?? null
    });
    this.loaded = false;
    await this.load();
    this.selectedId = id;
    return id;
  }

  /** @param {string} id */
  async remove(id) {
    await tauriInvoke('delete_token', { id });
    this.loaded = false;
    await this.load();
    if (this.selectedId === id) this.selectedId = this.items[0]?.id ?? null;
  }

  /** @param {string} code */
  openCode(code) {
    this.draftCode = code;
    navigationState.goTo('tokens');
  }

  consumeDraft() {
    const code = this.draftCode;
    this.draftCode = null;
    return code;
  }

  /** @param {string} status */
  statusBadge(status) {
    return status === 'tokenized'
      ? 'trail-status-badge trail-status-success'
      : 'trail-status-badge trail-status-unknown';
  }
}

/** @param {TokenForm} token */
function buildTokenCreatePrompt(token) {
  const name = token.name.trim();
  const symbol = token.symbol.trim();
  const tokenType = token.token_type === 'nft' ? 'nft' : 'fungible';
  const parsedDecimals = Number.parseInt(String(token.decimals), 10);
  const decimals = tokenType === 'nft' ? 0 : Number.isInteger(parsedDecimals) && parsedDecimals >= 0 ? parsedDecimals : 0;
  const amount = token.supply.trim();
  const treasury = (token.treasury ?? '').trim();
  const memo = (token.memo ?? '').trim() || `${name} token`;
  // Must stay "token.create tool": the daemon's forge_triggered_by() matches the
  // substring "create skill", so "token.create skill" would be misrouted to the
  // skill-forge flow instead of tokenizing.
  return (
    `Call the token.create tool immediately with exactly these arguments and no others: ` +
    `name=${JSON.stringify(name)}, symbol=${JSON.stringify(symbol)}, ` +
    `token_type=${JSON.stringify(tokenType)}, decimals=${decimals}, ` +
    `amount=${JSON.stringify(amount)}, treasury=${JSON.stringify(treasury)}, ` +
    `memo=${JSON.stringify(memo)}. Do not ask follow-up questions; just execute the tool.`
  );
}

/** @param {string} text */
function extractTokenId(text) {
  const trimmed = text.trim();
  if (!trimmed) return null;
  try {
    const parsed = JSON.parse(trimmed);
    const candidates = [parsed?.token_id, parsed?.tokenId];
    for (const c of candidates) {
      if (typeof c === 'string' && c.trim()) return c.trim();
    }
    return null;
  } catch {
    return null;
  }
}

export const tokensState = new TokensState();
