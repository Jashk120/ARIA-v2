<script>
  import '../app.css';
  import { onMount } from 'svelte';
  import { openUrl } from '@tauri-apps/plugin-opener';

  let { children } = $props();

  onMount(() => {
    /** @param {MouseEvent} event */
    function handleAnchorClick(event) {
      const target = event.target;
      if (!(target instanceof Element)) return;
      const anchor = target.closest('a');
      if (!anchor) return;

      const href = anchor.getAttribute('href');
      if (!href) return;

      if (
        href.startsWith('http://') ||
        href.startsWith('https://') ||
        href.startsWith('mailto:')
      ) {
        event.preventDefault();
        event.stopPropagation();
        openUrl(href).catch((err) => {
          console.error('Failed to open URL in external browser:', err);
        });
      }
    }

    document.addEventListener('click', handleAnchorClick, true);

    const originalWindowOpen = window.open;
    window.open = (url, target, features) => {
      if (
        url &&
        (String(url).startsWith('http://') ||
          String(url).startsWith('https://') ||
          String(url).startsWith('mailto:'))
      ) {
        openUrl(String(url)).catch((err) => {
          console.error('Failed to open URL via window.open:', err);
        });
        return null;
      }
      return originalWindowOpen
        ? originalWindowOpen.call(window, url, target, features)
        : null;
    };

    return () => {
      document.removeEventListener('click', handleAnchorClick, true);
    };
  });
</script>

{@render children()}

