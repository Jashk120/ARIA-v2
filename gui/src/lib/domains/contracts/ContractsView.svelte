<script>
  import TopBar from '$lib/domains/shell/TopBar.svelte';
  import { contractsState } from './contractsState.svelte.js';
  import { onMount } from 'svelte';

  let editing = $state(false);
  let formName = $state('');
  let formSource = $state('');
  let formCompiler = $state('foundry');
  let formId = $state(/** @type {string | null} */ (null));
  let saveError = $state('');
  let saving = $state(false);

  const selected = $derived(
    contractsState.items.find((c) => c.id === contractsState.selectedId) ?? null
  );

  onMount(async () => {
    await contractsState.load();
    const draft = contractsState.consumeDraft();
    if (draft) {
      formId = null;
      formName = 'ChatImport.sol';
      formSource = draft;
      formCompiler = 'foundry';
      contractsState.selectedId = null;
      editing = true;
    } else if (selected) {
      syncForm(selected);
    }
  });

  /** @param {{ id: string, name: string, source: string, compiler: string }} c */
  function syncForm(c) {
    formId = c.id;
    formName = c.name;
    formSource = c.source;
    formCompiler = c.compiler;
    editing = false;
    saveError = '';
  }

  /** @param {string} id */
  function selectContract(id) {
    const c = contractsState.items.find((x) => x.id === id);
    contractsState.selectedId = id;
    if (c) syncForm(c);
  }

  function newContract() {
    formId = null;
    formName = 'NewContract.sol';
    formSource = '// SPDX-License-Identifier: MIT\npragma solidity ^0.8.24;\n\ncontract NewContract {\n}\n';
    formCompiler = 'foundry';
    contractsState.selectedId = null;
    editing = true;
    saveError = '';
  }

  async function saveContract() {
    if (!formName.trim() || !formSource.trim() || saving) return;
    saving = true;
    saveError = '';
    try {
      await contractsState.save({
        id: formId ?? undefined,
        name: formName.trim(),
        source: formSource,
        compiler: formCompiler,
        status: selected?.status ?? 'draft'
      });
      const cur = contractsState.items.find((c) => c.id === contractsState.selectedId) ?? null;
      if (cur) syncForm(cur);
    } catch (e) {
      saveError = String(e);
    } finally {
      saving = false;
    }
  }

  /** @param {string} id */
  async function deleteContract(id) {
    saveError = '';
    try {
      await contractsState.remove(id);
      const cur = contractsState.items.find((c) => c.id === contractsState.selectedId) ?? null;
      if (cur) syncForm(cur);
      else {
        formId = null;
        formName = '';
        formSource = '';
      }
    } catch (e) {
      saveError = String(e);
    }
  }

  /** @param {number} ts */
  function fmtDate(ts) {
    try {
      return new Date(ts * 1000).toLocaleDateString();
    } catch {
      return '';
    }
  }
</script>

<section class="chat-panel">
  <TopBar title="Contracts">
    <button onclick={newContract}>＋ New contract</button>
  </TopBar>

  <div class="dashboard-panel">
    <section class="dash-section">
      <h2>Contracts</h2>
      {#if !contractsState.loaded}
        <p class="dash-caps-note">LOADING CONTRACTS…</p>
      {:else if contractsState.error}
        <p class="dash-error">Failed to load contracts: {contractsState.error}</p>
      {:else if contractsState.items.length === 0 && !formSource}
        <p class="dash-caps-note">NO CONTRACTS YET — CREATE ONE TO GET STARTED.</p>
      {:else if contractsState.items.length > 0}
        <table class="dash-table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Compiler</th>
              <th>Status</th>
              <th>Created</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {#each contractsState.items as c (c.id)}
              <tr>
                <td>
                  <button class="row-link" onclick={() => selectContract(c.id)} aria-label="Open {c.name}">
                    {c.name}
                  </button>
                </td>
                <td>{c.compiler}</td>
                <td><span class={contractsState.statusBadge(c.status)}>{c.status}</span></td>
                <td>{fmtDate(c.created_at)}</td>
                <td>
                  <button
                    class="row-link row-del"
                    onclick={() => deleteContract(c.id)}
                    aria-label="Delete {c.name}"
                  >Delete</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>

    <section class="dash-section">
      <h2>{formId ? 'Contract details' : 'New contract'}</h2>
      {#if !formSource && !formId}
        <p class="dash-caps-note">SELECT A CONTRACT ABOVE, OR CREATE A NEW ONE.</p>
      {:else}
        <label class="field">
          Name
          <input type="text" bind:value={formName} aria-label="Contract name" placeholder="AriaToken.sol" />
        </label>
        <label class="field">
          Compiler
          <select bind:value={formCompiler} aria-label="Compiler">
            <option value="foundry">Foundry</option>
            <option value="hardhat">Hardhat</option>
          </select>
        </label>
        <section class="dash-section-sub">
          <h3>Source</h3>
          {#if editing}
            <textarea
              class="input-field code-edit"
              rows={16}
              bind:value={formSource}
              aria-label="Contract source"
              spellcheck={false}
            ></textarea>
          {:else}
            <pre>{formSource}</pre>
          {/if}
        </section>
        {#if selected}
          <div class="chips" style="justify-content: flex-start;">
            <span class={contractsState.statusBadge(selected.status)}>{selected.status}</span>
            <span class="chip">{selected.compiler}</span>
          </div>
        {/if}
        {#if saveError}
          <p class="dash-error">Save failed: {saveError}</p>
        {/if}
        <div class="confirmation-actions">
          {#if editing}
            <button onclick={saveContract} disabled={!formName.trim() || !formSource.trim() || saving}>
              {saving ? 'Saving…' : 'Save'}
            </button>
            {#if formId}
              <button onclick={() => selected && syncForm(selected)}>Cancel</button>
            {/if}
          {:else}
            <button onclick={() => (editing = true)}>Edit</button>
          {/if}
          <button
            disabled
            title="Deploy is not wired to the daemon yet — coming soon."
            aria-label="Deploy (disabled: not wired to the daemon yet)"
          >Deploy</button>
        </div>
        <p class="hold-confirm">Deploy is not wired to the daemon yet — coming soon.</p>
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

  .field input[type='text'],
  .field select {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    color: var(--text);
    padding: 0.5rem 0.7rem;
  }

  .field input[type='text']:focus,
  .field select:focus {
    outline: none;
    border-color: var(--accent-border);
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

  .code-edit {
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    width: 100%;
    box-sizing: border-box;
  }
</style>
