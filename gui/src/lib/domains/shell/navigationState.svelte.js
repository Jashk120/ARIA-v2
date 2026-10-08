/**
 * Single SPA tab router. Views navigate via `navigationState.goTo('contracts')`
 * (e.g. chat handoffs); `+page.svelte` binds `activeTab` to `navigationState.tab`.
 */
class NavigationState {
  tab = $state('chat');

  /** @param {string} t */
  goTo(t) {
    this.tab = t;
  }
}

export const navigationState = new NavigationState();
