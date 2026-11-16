import { text as t } from './messages';
import { createLiveController } from './live-controller';
import { createReactProjectionRenderer } from './react-renderer';

const host = document.getElementById('app');
if (!host) throw new Error('Shenko root is missing');

async function mount(host: HTMLElement): Promise<void> {
  // Only the explicit demo entry loads fixture code. Live startup imports no fixture module.
  const demo = new URLSearchParams(location.search).get('demo') === '1'
    ? await import('./demo-controller')
    : undefined;
  const controller = demo ? demo.createDemoController() : createLiveController();
  const renderer = createReactProjectionRenderer(host, demo?.demoThreadData);
  const render = () => renderer.patch(controller.projection());
  const unsubscribe = controller.subscribe(render);
  renderer.render(controller.projection(), (intent) => controller.dispatch(intent));

  window.addEventListener('pagehide', () => {
    unsubscribe();
    controller.dispose();
    renderer.dispose();
  }, { once: true });
}

void mount(host).catch((error: unknown) => {
  host.setAttribute('role', 'alert');
  host.textContent = t('shell.startupError', { detail: error instanceof Error ? error.message : String(error) });
});
