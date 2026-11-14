import { definePrototype, tw } from '@proto.ui/core';

/**
 * One IM message bubble — a Shenko-authored Prototype compiled at build time
 * into React DOM source, like the transport badge.
 *
 * The bubble owns only geometry and the side it sits on: `side` is `self`
 * (the viewer's side of this thread) or `other`, `tail` marks the last bubble
 * of a run, and `tone` is `plain`, `steer` or `pending`. The message content is
 * the caller's children through the anonymous slot, so text, documents and
 * receipts update as ordinary React content; nothing about the message's
 * meaning is decided here.
 */
export default definePrototype({
  name: 'shenko-message-bubble',
  setup(def) {
    def.props.define({
      side: { type: 'string' },
      tail: { type: 'boolean' },
      tone: { type: 'string' },
    });
    def.props.setDefaults({ side: 'other', tail: false, tone: 'plain' });
    def.feedback.style.use(tw('shenko-bubble'));
    def.rule({
      when: (w) => w.prop('side').eq('self'),
      intent: (i) => i.feedback.style.use(tw('shenko-bubble-self')),
    });
    def.rule({
      when: (w) => w.prop('tail').eq(true),
      intent: (i) => i.feedback.style.use(tw('shenko-bubble-tail')),
    });
    def.rule({
      when: (w) => w.prop('tone').eq('steer'),
      intent: (i) => i.feedback.style.use(tw('shenko-bubble-steer')),
    });
    def.rule({
      when: (w) => w.prop('tone').eq('pending'),
      intent: (i) => i.feedback.style.use(tw('shenko-bubble-pending')),
    });
    return (r) => r.el('div', [r.slot()]);
  },
});
