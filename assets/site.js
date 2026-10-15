(() => {
  const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');

  document.querySelectorAll('[data-copy]').forEach((button) => {
    button.addEventListener('click', async () => {
      const original = button.textContent;
      try {
        await navigator.clipboard.writeText(button.dataset.copy);
        button.textContent = 'Copied';
      } catch {
        button.textContent = 'Select text';
      }
      window.setTimeout(() => { button.textContent = original; }, 1600);
    });
  });

  if (!reducedMotion.matches && 'IntersectionObserver' in window) {
    document.documentElement.classList.add('has-motion');
    const observer = new IntersectionObserver((entries) => {
      entries.forEach((entry) => {
        if (entry.isIntersecting) {
          entry.target.classList.add('is-visible');
          observer.unobserve(entry.target);
        }
      });
    }, { rootMargin: '0px 0px -8% 0px', threshold: 0.08 });
    document.querySelectorAll('.pr-card, .feature-grid article, .loop-grid li').forEach((node) => observer.observe(node));
  }
})();
