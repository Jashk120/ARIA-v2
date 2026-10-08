<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import { tokensState } from './tokensState.svelte.js';
  import { daemonState } from '$lib/services/daemonState.svelte.js';
  import { tauriInvoke } from '$lib/services/tauri.js';
  import { onMount, onDestroy } from 'svelte';

  let formId = $state(/** @type {string | null} */ (null));
  let formName = $state('');
  let formSymbol = $state('');
  let formType = $state('fungible');
  let formSupply = $state('');
  let formDecimals = $state(0);
  let formMemo = $state('');
  let formTreasury = $state('');
  let saveError = $state('');
  let saving = $state(false);
  let dltEnabled = $state(true);

  const selected = $derived(
    tokensState.items.find((t) => t.id === tokensState.selectedId) ?? null
  );

  const tokenizeBusy =
    $derived(tokensState.tokenizeStatus === 'running' || tokensState.tokenizeStatus === 'awaiting');

  const canTokenize = $derived(
    daemonState.online &&
      dltEnabled &&
      !tokenizeBusy &&
      !!formName.trim() &&
      !!formSymbol.trim() &&
      !!formSupply.trim() &&
      !!formTreasury.trim()
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
  .setTreasuryAccountId(${JSON.stringify(formTreasury.trim() || '0.0.12345')})
  .setTokenMemo(${JSON.stringify(memo)});

// const receipt = await (await tx.execute(client)).getReceipt(client);
// console.log("tokenId:", receipt.tokenId?.toString());
`;
  });

  onMount(async () => {
    await tokensState.init();
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
    // Best-effort treasury auto-fill from the daemon wallet (skip silently
    // when the daemon is offline or has no Hedera keys).
    if (daemonState.online && !formTreasury.trim()) {
      try {
        const wallet = /** @type {{ account_id?: string }} */ (
          await tauriInvoke('dashboard_query', { query: 'query_wallet_balance' })
        );
        if (wallet?.account_id) formTreasury = wallet.account_id;
      } catch {
        // ignore — the user can type the treasury account by hand
      }
    }
    // Best-effort DLT air-gap flag (default stays enabled on error).
    try {
      const dlt = /** @type {{ enabled?: boolean }} */ (
        await tauriInvoke('dashboard_query', { query: 'query_dlt_status' })
      );
      if (typeof dlt?.enabled === 'boolean') dltEnabled = dlt.enabled;
    } catch {
      // ignore — assume DLT is enabled
    }
  });

  onDestroy(() => tokensState.destroy());

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
    formTreasury = '';
    importedScript = null;
    saveError = '';
  }

  /** @param {{ id: string, name: string, symbol: string, token_type: string, supply: string, decimals: number, memo: string | null, treasury?: string | null }} t */
  function syncForm(t) {
    formId = t.id;
    formName = t.name;
    formSymbol = t.symbol;
    formType = t.token_type;
    formSupply = t.supply;
    formDecimals = t.decimals;
    formMemo = t.memo ?? '';
    formTreasury = t.treasury ?? '';
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
        status: selected?.status ?? 'draft',
        treasury: formTreasury.trim(),
        token_id: selected?.token_id ?? null
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

  async function tokenizeToken() {
    if (!canTokenize) return;
    const d = Number.parseInt(String(formDecimals), 10);
    // No form re-sync here: on success the done-event handler persists the
    // token id + status and reloads the list; the selected row updates via
    // the derived `selected` below.
    await tokensState.tokenize({
      id: formId ?? undefined,
      name: formName.trim(),
      symbol: formSymbol.trim(),
      token_type: formType,
      supply: formSupply.trim(),
      decimals: formType === 'nft' ? 0 : Number.isInteger(d) && d >= 0 ? d : 0,
      memo: formMemo.trim(),
      treasury: formTreasury.trim(),
      status: selected?.status ?? 'draft',
      token_id: selected?.token_id ?? null
    });
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
          <span class="sr-label">Treasury</span>
          <input
            type="text"
            placeholder="0.0.12345"
            bind:value={formTreasury}
            aria-label="Treasury account"
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
          onclick={tokenizeToken}
          disabled={!canTokenize}
          title={!daemonState.online
            ? 'Daemon is offline.'
            : !dltEnabled
              ? 'DLT is disabled (air-gap mode). Enable it in Settings to tokenize.'
              : 'Create this token on Hedera testnet via the daemon.'}
          aria-label="Tokenize on Hedera testnet"
        >{tokenizeBusy ? 'Tokenizing…' : 'Tokenize'}</button>
      </div>
      {#if tokensState.pendingConfirm}
        <div class="hold-confirm-card" role="group" aria-label="Tokenize confirmation">
          <pre class="hold-confirm-text">{tokensState.pendingConfirm.content}</pre>
          <div class="confirmation-actions">
            <button
              onclick={() => tokensState.respondConfirm('yes')}
              disabled={!daemonState.online}
            >Yes</button>
            <button
              onclick={() => tokensState.respondConfirm('no')}
              disabled={!daemonState.online}
            >No</button>
          </div>
        </div>
      {/if}
      {#if tokensState.tokenizeStatus === 'running'}
        <p class="dash-caps-note">TOKENIZING — WAITING ON THE DAEMON…</p>
      {/if}
      {#if tokensState.tokenizeStatus === 'error' && tokensState.tokenizeError}
        <p class="dash-error">Tokenize failed: {tokensState.tokenizeError}</p>
      {/if}
      {#if tokensState.tokenizeStatus === 'done'}
        <p class="dash-caps-note">
          TOKENIZED{#if tokensState.createdTokenId} — TOKEN ID {tokensState.createdTokenId}{/if}.
        </p>
        {#if tokensState.tokenizeResult}
          <pre class="source-pre">{tokensState.tokenizeResult}</pre>
        {/if}
      {/if}
      {#if !dltEnabled}
        <p class="hold-confirm">DLT is disabled (air-gap mode) — enable it in Settings to tokenize.</p>
      {/if}
      <p class="dash-caps-note">
        TOKENIZE CREATES A REAL HTS TOKEN ON HEDERA TESTNET. THE TREASURY ACCOUNT MUST BE ON THE
        PAYMENT ALLOWLIST (SETTINGS → ACCOUNT ALLOWLIST), AND DLT MUST BE ENABLED.
      </p>
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
              <th>Treasury</th>
              <th>Token ID</th>
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
                <td>{t.treasury ?? ''}</td>
                <td>{t.token_id ?? ''}</td>
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
          {#if selected.token_id}
            <p class="dash-caps-note">TOKEN ID {selected.token_id}</p>
          {/if}
          {#if selected.treasury}
            <p class="dash-caps-note">TREASURY {selected.treasury}</p>
          {/if}
          <pre class="source-pre">{selected.source}</pre>
        </section>
      {/if}
    </section>
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

  .hold-confirm-card {
    margin-top: 0.75rem;
    border: 1px solid var(--border, #444);
    border-radius: 0.5rem;
    padding: 0.75rem;
  }

  .hold-confirm-text {
    white-space: pre-wrap;
    margin: 0 0 0.75rem 0;
  }
</style>
