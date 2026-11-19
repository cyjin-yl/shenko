import { text as t } from './messages';
import * as React from 'react';
import type { PendingAttachment, UiNode } from './projection';
import { Icon } from './icons';
import { Button, ProtoTextarea, type Emit } from './ui-kit';

/// Pasted text longer than this becomes a text attachment instead of filling
/// the input: the owner's words stay byte-exact and the input stays usable.
export const LONG_PASTE_CHARS = 4000;

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

export function Composer({ node, emit, variant = 'stream' }: { node: UiNode; emit: Emit; variant?: 'stream' | 'im' }) {
  const draftAction = node.action_refs[0];
  const appendAction = node.action_refs[1];
  const append = node.props.append_action as string;
  const canSend = Boolean(node.props.append_enabled) && appendAction === append;
  // One send, as in Claude Code: no mode to choose. While the subject is
  // mid-request the server binds the message to that request; otherwise it
  // is read at the next boundary. A message not taken yet can be edited.
  const steerOffered = node.props.steer_available === true;
  const submit = () => {
    if (canSend) emit('composer', appendAction);
  };
  const demo = node.props.demo === true;
  const attachments = (node.props.attachments ?? []) as PendingAttachment[];
  const attachAction = node.props.attach_action as string | undefined;
  const canAttach = node.props.attach_enabled === true && Boolean(attachAction);
  const fileInput = React.useRef<HTMLInputElement>(null);
  const attach = (files: File[]) => {
    if (canAttach && attachAction && files.length) emit('composer', attachAction, { files });
  };
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
    <footer
      className={`composer composer-${variant}`}
      hidden={node.props.hidden}
      onDragOver={(event) => { if (canAttach && event.dataTransfer.types.includes('Files')) event.preventDefault(); }}
      onDrop={(event) => {
        if (!canAttach || !event.dataTransfer.files.length) return;
        event.preventDefault();
        attach(Array.from(event.dataTransfer.files));
      }}
    >
      {variant === 'im' ? (
        <>
          <button
            type="button"
            className="composer-attach"
            aria-label={t('composer.attach')}
            title={t('composer.attach')}
            disabled={!canAttach}
            onClick={() => fileInput.current?.click()}
          ><Icon name="plus" size={18} /></button>
          <input
            ref={fileInput}
            type="file"
            multiple
            hidden
            onChange={(event) => { attach(Array.from(event.currentTarget.files ?? [])); event.currentTarget.value = ''; }}
          />
        </>
      ) : null}
      <div className="composer-card">
        {attachments.length > 0 && (
          <ul className="composer-files" aria-label={t('composer.attachments')}>
            {attachments.map((item) => (
              <li key={item.local_id} className={`composer-file is-${item.status}`} title={item.error ?? item.name}>
                {item.preview
                  ? <img className="composer-file-thumb" src={item.preview} alt="" />
                  : <span className="composer-file-icon"><Icon name="file" size={18} /></span>}
                <span className="composer-file-text">
                  <span className="composer-file-name">{item.name}</span>
                  <span className="composer-file-meta">
                    {item.status === 'uploading' ? t('composer.uploading')
                      : item.status === 'failed' ? t('composer.uploadFailed')
                      : formatBytes(item.bytes)}
                  </span>
                </span>
                <button
                  type="button"
                  className="composer-file-remove"
                  aria-label={t('composer.removeAttachment', { name: item.name })}
                  onClick={() => emit('composer', node.props.detach_action as string, { local_id: item.local_id })}
                ><Icon name="close" size={12} /></button>
              </li>
            ))}
          </ul>
        )}
        <div
          className="composer-editor"
          id="composer-editor"
          onPasteCapture={(event) => {
            if (!canAttach) return;
            const files = Array.from(event.clipboardData.files);
            if (files.length) {
              event.preventDefault();
              attach(files);
              return;
            }
            const pasted = event.clipboardData.getData('text/plain');
            if (pasted.length > LONG_PASTE_CHARS) {
              event.preventDefault();
              attach([new File([pasted], t('composer.pastedTextName'), { type: 'text/plain' })]);
            }
          }}
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
            ariaLabel={demo ? t('composer.demoLabel') : t('composer.appendLabel')}
            value={node.props.draft}
            rows={2}
            readOnly={demo}
            placeholder={demo ? t('composer.demoPlaceholder') : variant === 'im' ? t('composer.messagePlaceholder') : t('composer.placeholder')}
            className="composer-input"
            onValueChange={(detail) => emit('composer', draftAction, { value: detail.value })}
          />
        </div>
        <div className="composer-bar">
          <span className="composer-hint">
            {reason ?? (steerOffered ? t('composer.liveHint') : t('composer.hint'))}
          </span>
          <Button
            variant="default"
            size="icon"
            className="send"
            label={t('composer.append')}
            disabled={!canSend}
            onClick={submit}
          ><Icon name="send" /></Button>
        </div>
      </div>
    </footer>
  );
}
