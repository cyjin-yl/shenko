(function () {
  try {
    var query = new URLSearchParams(location.search).get('theme');
    var stored = null;
    try { stored = localStorage.getItem('shenko.theme.v1'); } catch (error) { stored = null; }
    var theme = query === 'light' || query === 'dark'
      ? query
      : (stored === 'light' || stored === 'dark' ? stored : null);
    if (theme === null) {
      theme = window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
    }
    var root = document.documentElement;
    root.dataset.theme = theme;
    root.classList.toggle('light', theme === 'light');
    root.classList.toggle('dark', theme === 'dark');
  } catch (error) {
    /* A browser that refuses the storage keeps the default theme. */
  }
})();
