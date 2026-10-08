import { tauriInvoke } from '$lib/services/tauri.js';
import { navigationState } from '$lib/domains/shell/navigationState.svelte.js';

const ARIA_TOKEN_SOURCE = `// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import "@openzeppelin/contracts/access/Ownable.sol";

/// @title AriaToken — ERC-20-ish sample for the ARIA Contracts screen.
/// @notice GUI-local sample. Deploy is not wired to the daemon yet.
contract AriaToken is ERC20, Ownable {
    uint256 public constant MAX_SUPPLY = 1_000_000 * 1e18;

    constructor() ERC20("AriaToken", "ARIA") Ownable(msg.sender) {
        _mint(msg.sender, 100_000 * 1e18);
    }

    function mint(address to, uint256 amount) external onlyOwner {
        require(totalSupply() + amount <= MAX_SUPPLY, "AriaToken: max supply");
        _mint(to, amount);
    }
}
`;

const VAULT_SOURCE = `// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title Vault — minimal deposit/withdraw sample.
/// @notice GUI-local sample. Deploy is not wired to the daemon yet.
contract Vault {
    mapping(address => uint256) public balances;
    address public owner;

    event Deposited(address indexed user, uint256 amount);
    event Withdrawn(address indexed user, uint256 amount);

    constructor() {
        owner = msg.sender;
    }

    function deposit() external payable {
        require(msg.value > 0, "Vault: zero deposit");
        balances[msg.sender] += msg.value;
        emit Deposited(msg.sender, msg.value);
    }

    function withdraw(uint256 amount) external {
        require(balances[msg.sender] >= amount, "Vault: insufficient");
        balances[msg.sender] -= amount;
        (bool ok, ) = msg.sender.call{value: amount}("");
        require(ok, "Vault: transfer failed");
        emit Withdrawn(msg.sender, amount);
    }
}
`;

/**
 * @typedef {{ id: string, name: string, source: string, compiler: string, status: string, created_at: number }} ContractItem
 */

class ContractsState {
  /** @type {ContractItem[]} */
  items = $state([]);
  loaded = $state(false);
  /** @type {string | null} */
  draftCode = $state(null);
  /** @type {string | null} */
  selectedId = $state(null);
  /** @type {string | null} */
  error = $state(null);

  /** Load contracts; seed 2 samples on first run when the table is empty. */
  async load(force = false) {
    if (this.loaded && !force) return;
    this.error = null;
    try {
      let rows = /** @type {ContractItem[]} */ (
        await tauriInvoke('list_contracts')
      );
      if (rows.length === 0) {
        await this.#seed();
        rows = /** @type {ContractItem[]} */ (
          await tauriInvoke('list_contracts')
        );
      }
      this.items = rows;
      if (!this.selectedId && rows.length > 0) this.selectedId = rows[0].id;
      this.loaded = true;
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Clear the loaded flag so the user can retry after a load failure. */
  async retry() {
    this.loaded = false;
    await this.load();
  }

  async #seed() {
    const now = Date.now();
    await tauriInvoke('save_contract', {
      id: `ctr_${now}_aria`,
      name: 'AriaToken.sol',
      source: ARIA_TOKEN_SOURCE,
      compiler: 'foundry',
      status: 'draft'
    });
    await tauriInvoke('save_contract', {
      id: `ctr_${now + 1}_vault`,
      name: 'Vault.sol',
      source: VAULT_SOURCE,
      compiler: 'foundry',
      status: 'draft'
    });
  }

  /**
   * @param {{ id?: string, name: string, source: string, compiler: string, status?: string }} contract
   */
  async save(contract) {
    const id = contract.id ?? `ctr_${Date.now()}`;
    await tauriInvoke('save_contract', {
      id,
      name: contract.name,
      source: contract.source,
      compiler: contract.compiler,
      status: contract.status ?? 'draft'
    });
    this.loaded = false;
    await this.load();
    this.selectedId = id;
    return id;
  }

  /** @param {string} id */
  async remove(id) {
    await tauriInvoke('delete_contract', { id });
    this.loaded = false;
    await this.load();
    if (this.selectedId === id) this.selectedId = this.items[0]?.id ?? null;
  }

  /** @param {string} code */
  openCode(code) {
    this.draftCode = code;
    navigationState.goTo('contracts');
  }

  consumeDraft() {
    const code = this.draftCode;
    this.draftCode = null;
    return code;
  }

  /** @param {string} status */
  statusBadge(status) {
    return status === 'deployed'
      ? 'trail-status-badge trail-status-success'
      : 'trail-status-badge trail-status-unknown';
  }
}

export const contractsState = new ContractsState();
