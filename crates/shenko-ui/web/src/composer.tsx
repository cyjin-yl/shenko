import { text as t } from './messages';
import * as React from 'react';
import type { UiNode } from './projection';
import { Icon } from './icons';
import { Button, ProtoTextarea, ToggleGroupRoot, ToggleGroupItem, type Emit } from './ui-kit';

export function Composer({ node, emit, variant = 'stream' }: { node: UiNode; emit: Emit; variant?: 'stream' | 'im' }) {
  const draftAction = node.action_refs[0];
  const appendAction = node.action_refs[1];
  const steerAction = node.props.steer_action as string | undefined;
  const append = node.props.append_action as string;
  const canSend = Boolean(node.props.append_enabled) && appendAction === append;
  const canSteer = Boolean(node.props.steer_enabled) && steerAction === node.action_refs[2];
  // The mode is the reader's choice of which admitted action Enter submits;
  // steer is offered only while the server provides it.
  const [mode, setMode] = React.useState<'append' | 'steer'>('append');
  const steerOffered = node.props.steer_available === true && steerAction === node.action_refs[2];
  const steering = mode === 'steer' && steerOffered;
  const submit = () => {
    if (steering && steerAction) emit('composer', steerAction);
    else if (canSend) emit('composer', appendAction);
  };
  const demo = node.props.demo === true;
  const reason = demo
    ? t('composer.demoReason')
    : node.props.connection !== 'online'
      ? t('composer.offlineReason')
      : !node.props.subject_bound
        ? t('composer.bindingReason')
        : node.props.append_offered !== true
          ? t('composer.unavailableReason')
          : null;
  return (
    <footer className={`composer composer-${variant}`} hidden={node.props.hidden}>
      <div className={`composer-card${steering ? ' is-steer' : ''}`}>
        <div
          className="composer-editor"
          id="composer-editor"
          onKeyDownCapture={(event) => {
            if (event.nativeEvent.isComposing) return;
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault();
              submit();
            } else if (event.key === 'Escape') {
              event.stopPropagation();
              if (node.props.draft) emit('composer', draftAction, { value: '' });
              else (event.target as HTMLElement).blur();
            }
          }}
        >
          <ProtoTextarea
            ariaLabel={demo ? t('composer.demoLabel') : steering ? t('composer.steerLabel') : t('composer.appendLabel')}
            value={node.props.draft}
            rows={2}
            readOnly={demo}
            placeholder={demo ? t('composer.demoPlaceholder') : steering ? t('composer.steerPlaceholder') : variant === 'im' ? t('composer.messagePlaceholder') : t('composer.placeholder')}
            className="composer-input"
            onValueChange={(detail) => emit('composer', draftAction, { value: detail.value })}
          />
        </div>
        <div className="composer-bar">
          {steerOffered ? (
            <ToggleGroupRoot
              className="mode-switch"
              a11yLabel={t('composer.mode')}
              value={[steering ? 'steer' : 'append']}
              onValueChange={(detail: { value: readonly string[] }) => {
                const next = detail.value[0];
                if (next === 'append' || next === 'steer') setMode(next);
              }}
            >
              <ToggleGroupItem className="mode-item" value="append">{t('composer.append')}</ToggleGroupItem>
              <ToggleGroupItem className="mode-item" value="steer">{t('composer.steer')}</ToggleGroupItem>
            </ToggleGroupRoot>
          ) : null}
          <span className="composer-hint">
            {reason ?? (steering ? t('composer.steerHint') : t('composer.hint'))}
          </span>
          <Button
            variant="default"
            size="icon"
            className="send"
            label={steering ? t('composer.steer') : t('composer.append')}
            disabled={steering ? !canSteer : !canSend}
            onClick={submit}
          ><Icon name={steering ? 'steer' : 'send'} /></Button>
        </div>
      </div>
    </footer>
  );
}
