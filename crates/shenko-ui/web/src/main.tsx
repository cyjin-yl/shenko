import { createDemoController } from './demo-controller';
import { createLiveController } from './live-controller';
import { createReactProjectionRenderer } from './react-renderer';

const host = document.getElementById('app');
if (!host) throw new Error('Shenko root is missing');

// The demo branch never constructs LiveController: no live fetch, socket, or
// submission path exists in that store. Both branches use the same renderer.
const demo = new URLSearchParams(location.search).get('demo') === '1';
const controller = demo ? createDemoController() : createLiveController();
const renderer = createReactProjectionRenderer(host);
const render = () => renderer.patch(controller.projection());
const unsubscribe = controller.subscribe(render);
renderer.render(controller.projection(), (intent) => controller.dispatch(intent));

window.addEventListener('pagehide', () => {
  unsubscribe();
  controller.dispose();
  renderer.dispose();
}, { once: true });
