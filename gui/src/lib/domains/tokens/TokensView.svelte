<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import { tokensState } from './tokensState.svelte.js';
  import { onMount } from 'svelte';

  let formId = $state(/** @type {string | null} */ (null));
  let formName = $state('');
  let formSymbol = $state('');
  let formType = $state('fungible');
  let formSupply = $state('');
  let formDecimals = $state(0);
  let formMemo = $state('');
  let saveError = $state('');
  let saving = $state(false);

  const selected = $derived(
    tokensState.items.find((t) => t.id === tokensState.selectedId) ?? null
  );

  const preview = $derived.by(() => {
    const name = formName.trim() || 'MyToken';
    const symbol = formSymbol.trim() || 'MTK';
    const isNft = formType === 'nft';
    const parsed = Number.parseInt(String(formDecimals), 10);
    const decimals = isNft ? 0 : Number.isInteger(parsed) && parsed >= 0 ? parsed : 0;
    const supply = formSupply.trim() || (isNft ? '500' : '1000000');
    const memo = formMemo.trim() || `${name} token`;
    const typeExpr = isNft ? 'TokenType.NonFungibleUnique' : 'TokenType.FungibleCommon';
    return `import { TokenCreateTransaction, TokenType, TokenSupplyType } from "@hashgraph/sdk";

// ${name} — generated HTS preview (GUI-local, not submitted).
const tx = new TokenCreateTransaction()
  .setTokenName(${JSON.stringify(name)})
  .setTokenSymbol(${JSON.stringify(symbol)})
  .setTokenType(${typeExpr})
  .setDecimals(${decimals})
  .setInitialSupply(${isNft ? 0 : supply})
  .setSupplyType(TokenSupplyType.Finite)
  .setMaxSupply(${supply})
  .setTreasuryAccountId("0.0.12345")
  .setTokenMemo(${JSON.stringify(memo)});

// const receipt = await (await tx.execute(client)).getReceipt(client);
// console.log("tokenId:", receipt.tokenId?.toString());
`;
  });

  onMount(async () => {
    resetForm();
    await tokensState.load();
    const draft = tokensState.consumeDraft();
    if (draft) {
      // A chat handoff lands as the script preview — keep the form blank for
      // the user to name the token, with the imported script visible below.
      formMemo = 'Imported from chat';
      importedScript = draft;
    } else if (selected) {
      syncForm(selected);
    }
  });

  /** Raw script imported from chat (shown instead of the generated preview). */
  let importedScript = $state(/** @type {string | null} */ (null));

  function resetForm() {
    formId = null;
    formName = '';
    formSymbol = '';
    formType = 'fungible';
    formSupply = '';
    formDecimals = 0;
    formMemo = '';
    importedScript = null;
    saveError = '';
  }

  /** @param {{ id: string, name: string, symbol: string, token_type: string, supply: string, decimals: number, memo: string | null }} t */
  function syncForm(t) {
    formId = t.id;
    formName = t.name;
    formSymbol = t.symbol;
    formType = t.token_type;
    formSupply = t.supply;
    formDecimals = t.decimals;
    formMemo = t.memo ?? '';
    importedScript = null;
    saveError = '';
  }

  /** @param {string} id */
  function selectToken(id) {
    const t = tokensState.items.find((x) => x.id === id);
    tokensState.selectedId = id;
    if (t) syncForm(t);
  }

  function newToken() {
    resetForm();
    tokensState.selectedId = null;
  }

  async function saveToken() {
    if (!formName.trim() || !formSymbol.trim() || !formSupply.trim() || saving) return;
    const d = Number.parseInt(String(formDecimals), 10);
    if (formType !== 'nft' && (!Number.isInteger(d) || d < 0)) {
      saveError = 'Decimals must be a non-negative integer.';
      return;
    }
    saving = true;
    saveError = '';
    try {
      const source = importedScript ?? preview;
      await tokensState.save({
        id: formId ?? undefined,
        name: formName.trim(),
        symbol: formSymbol.trim(),
        token_type: formType,
        supply: formSupply.trim(),
        decimals: formType === 'nft' ? 0 : d,
        memo: formMemo.trim(),
        source,
        status: selected?.status ?? 'draft'
      });
      importedScript = null;
      const cur = tokensState.items.find((t) => t.id === tokensState.selectedId) ?? null;
      if (cur) syncForm(cur);
    } catch (e) {
      saveError = String(e);
    } finally {
      saving = false;
    }
  }

  /** @param {string} id */
  async function deleteToken(id) {
    saveError = '';
    try {
      await tokensState.remove(id);
      const cur = tokensState.items.find((t) => t.id === tokensState.selectedId) ?? null;
      if (cur) syncForm(cur);
      else resetForm();
    } catch (e) {
      saveError = String(e);
    }
  }

  /** @param {number} ts */
  function fmtDate(ts) {
    const d = new Date(ts * 1000);
    return Number.isNaN(d.getTime()) ? '' : d.toLocaleDateString();
  }
</script>

<section class="chat-panel">
  <TopBar title="Tokens">
    <button onclick={newToken}>＋ New token</button>
  </TopBar>

  <div class="dashboard-panel">
    <section class="dash-section">
      <h2>Define a token</h2>
      <form
        class="settings-add-form"
        onsubmit={(e) => {
          e.preventDefault();
          saveToken();
        }}
      >
        <label>
          <span class="sr-label">Token name</span>
          <input type="text" placeholder="Token name" bind:value={formName} aria-label="Token name" />
        </label>
        <label>
          <span class="sr-label">Symbol</span>
          <input type="text" placeholder="Symbol" bind:value={formSymbol} aria-label="Token symbol" />
        </label>
        <select bind:value={formType} aria-label="Token type">
          <option value="fungible">Fungible</option>
          <option value="nft">NFT</option>
        </select>
        <label>
          <span class="sr-label">Supply</span>
          <input type="text" placeholder="Supply" bind:value={formSupply} aria-label="Supply" inputmode="numeric" />
        </label>
        <label>
          <span class="sr-label">Decimals</span>
          <input
            type="number"
            placeholder="Decimals"
            bind:value={formDecimals}
            aria-label="Decimals"
            inputmode="numeric"
            min="0"
            step="1"
            disabled={formType === 'nft'}
          />
        </label>
        <label>
          <span class="sr-label">Memo</span>
          <input type="text" placeholder="Memo" bind:value={formMemo} aria-label="Memo" />
        </label>
        <button type="submit" disabled={!formName.trim() || !formSymbol.trim() || !formSupply.trim() || saving}>
          {saving ? 'Saving…' : 'Save'}
        </button>
      </form>
    </section>

    <section class="dash-section">
      <h2>HTS script preview</h2>
      <section class="dash-source-section">
        <pre class="source-pre">{importedScript ?? preview}</pre>
      </section>
      {#if importedScript}
        <p class="dash-caps-note">IMPORTED FROM CHAT — NAME THE TOKEN ABOVE, THEN SAVE.</p>
      {/if}
      {#if saveError}
        <p class="dash-error">Save failed: {saveError}</p>
      {/if}
      <div class="confirmation-actions">
        <button
          onclick={saveToken}
          disabled={!formName.trim() || !formSymbol.trim() || !formSupply.trim() || saving}
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
        <button
          disabled
          title="Tokenize is not wired to the daemon yet — coming soon."
          aria-label="Tokenize (disabled: not wired to the daemon yet)"
        >Tokenize</button>
      </div>
      <p class="hold-confirm">Tokenize is not wired to the daemon yet — coming soon.</p>
    </section>

    <section class="dash-section">
      <h2>Tokens</h2>
      {#if !tokensState.loaded}
        <p class="dash-caps-note">LOADING TOKENS…</p>
      {:else if tokensState.error}
        <p class="dash-error">Failed to load tokens: {tokensState.error}</p>
        <button onclick={() => tokensState.retry()}>↻ Retry</button>
      {:else if tokensState.items.length === 0}
        <p class="dash-caps-note">NO TOKENS YET — DEFINE ONE ABOVE TO GET STARTED.</p>
      {:else}
        <table class="dash-table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Symbol</th>
              <th>Type</th>
              <th>Status</th>
              <th>Created</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {#each tokensState.items as t (t.id)}
              <tr>
                <td>
                  <button class="row-link" onclick={() => selectToken(t.id)} aria-label="Open {t.name}">
                    {t.name}
                  </button>
                </td>
                <td>{t.symbol}</td>
                <td>{t.token_type}</td>
                <td><span class={tokensState.statusBadge(t.status)}>{t.status}</span></td>
                <td>{fmtDate(t.created_at)}</td>
                <td>
                  <button
                    class="row-link row-del"
                    onclick={() => deleteToken(t.id)}
                    aria-label="Delete {t.name}"
                  >Delete</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
      {#if selected?.source}
        <section class="dash-source-section">
          <h3>Saved script — {selected.name}</h3>
          <pre class="source-pre">{selected.source}</pre>
        </section>
      {/if}
    </section>

    <p class="dash-caps-note">Preview — artifacts are stored locally in the GUI, not yet wired to the daemon.</p>
  </div>
</section>

<style>
  .field {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    margin-bottom: 1.1rem;
    font-size: 0.82rem;
    color: var(--text-dim);
    font-weight: 600;
  }

  .sr-label {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }

  .row-link {
    background: none;
    border: none;
    padding: 0;
    color: inherit;
    font: inherit;
    cursor: pointer;
    text-decoration: underline;
    text-underline-offset: 2px;
  }

  .row-del {
    opacity: 0.75;
  }
</style>
