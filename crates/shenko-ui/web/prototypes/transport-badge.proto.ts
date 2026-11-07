import { definePrototype, tw } from '@proto.ui/core';

/**
 * Shenko's transport status chip — a Shenko-authored Prototype, not an upstream
 * family. It is compiled at build time by the pinned Proto-UI compiler
 * (`react-dom-source-v1`) into the React DOM source the shell imports; the
 * browser never imports the compiler or the Proto-UI runtime.
 *
 * The Prototype is presentational on purpose: the caller supplies the already
 * decided label and tone, so stream/receipt semantics stay in the Shenko
 * projection and are not re-implemented here. Only primitive props are admitted
 * by the compiler (`string`/`boolean`/`number`), and template nodes may carry
 * only a static style handle, so the tone styling is expressed as tokens that
 * `web/src/app.css` binds to Shenko's own palette.
 */
export default definePrototype({
  name: 'shenko-transport-status',
  setup(def) {
    def.props.define({
      label: { type: 'string' },
      tone: { type: 'string' },
    });
    def.props.setDefaults({ label: '', tone: 'offline' });
    def.feedback.style.use(tw('shenko-status-chip'));
    def.rule({
      when: (w) => w.prop('tone').eq('online'),
      intent: (i) => i.feedback.style.use(tw('shenko-status-online')),
    });
    def.rule({
      when: (w) => w.prop('tone').eq('connecting'),
      intent: (i) => i.feedback.style.use(tw('shenko-status-connecting')),
    });
    def.rule({
      when: (w) => w.prop('tone').eq('reconnecting'),
      intent: (i) => i.feedback.style.use(tw('shenko-status-reconnecting')),
    });
    def.rule({
      when: (w) => w.prop('tone').eq('local'),
      intent: (i) => i.feedback.style.use(tw('shenko-status-local')),
    });
    return (r) => r.el('span', [r.el('i', { style: tw('shenko-status-dot') }), r.read.props.get().label]);
  },
});
