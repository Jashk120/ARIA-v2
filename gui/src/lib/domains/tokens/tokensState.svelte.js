import { tauriInvoke } from '$lib/services/tauri.js';
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
 * @typedef {{ id: string, name: string, symbol: string, token_type: string, supply: string, decimals: number, memo: string | null, source: string | null, status: string, created_at: number }} TokenItem
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

  /** Load tokens; seed 2 samples on first run when the table is empty. */
  async load() {
    if (this.loaded) return;
    this.error = null;
    try {
      let rows = /** @type {TokenItem[]} */ (await tauriInvoke('list_tokens'));
      if (rows.length === 0) {
        await this.#seed();
        rows = /** @type {TokenItem[]} */ (await tauriInvoke('list_tokens'));
      }
      this.items = rows;
      if (!this.selectedId && rows.length > 0) this.selectedId = rows[0].id;
    } catch (e) {
      this.error = String(e);
    } finally {
      this.loaded = true;
    }
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
      status: 'draft'
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
      status: 'draft'
    });
  }

  /**
   * @param {{ id?: string, name: string, symbol: string, token_type: string, supply: string, decimals: number, memo?: string, source?: string, status?: string }} token
   */
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
      status: token.status ?? 'draft'
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

export const tokensState = new TokensState();
